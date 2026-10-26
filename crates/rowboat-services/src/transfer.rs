//! Bounded, lossless file transfer with explicit JSON Lines/JSON array formats.
use anyhow::{Context, Result, bail, ensure};
use rowboat_core::{
    Cell, Column, Database, Engine, QueryEvent, QueryResult, QueryStreamOptions, TableRef,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::sync::{mpsc, watch};

type ImportRow = Vec<(String, Cell)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferFormat {
    Csv,
    JsonLines,
    /// Import a top-level JSON array of row objects; streaming export is unsupported.
    JsonArray,
    Sql,
}

#[derive(Debug, Clone)]
pub struct TransferOptions {
    pub batch_rows: usize,
    pub batch_bytes: usize,
    pub max_record_bytes: usize,
    pub max_columns: usize,
    pub sample_rows: usize,
    pub max_sample_bytes: usize,
}
impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            batch_rows: 256,
            batch_bytes: 4 * 1024 * 1024,
            max_record_bytes: 1024 * 1024,
            max_columns: 1024,
            sample_rows: 50,
            max_sample_bytes: 1024 * 1024,
        }
    }
}
impl TransferOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=4096).contains(&self.batch_rows),
            "Batch rows must be 1–4096"
        );
        ensure!(
            (1024..=16 * 1024 * 1024).contains(&self.batch_bytes),
            "Batch bytes must be 1KiB–16MiB"
        );
        ensure!(
            (256..=8 * 1024 * 1024).contains(&self.max_record_bytes)
                && self.max_record_bytes <= self.batch_bytes,
            "Record limit must be 256 bytes–8MiB and fit a batch"
        );
        ensure!(
            (1..=1024).contains(&self.max_columns),
            "Column limit must be 1–1024"
        );
        ensure!(
            self.sample_rows <= 1000 && self.max_sample_bytes <= 8 * 1024 * 1024,
            "Sample exceeds retention limit"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferPhase {
    #[default]
    Scanning,
    Exporting,
    Applying,
    Done,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransferProgress {
    pub phase: TransferPhase,
    pub rows: u64,
    pub bytes: u64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransferSummary {
    pub rows: u64,
    pub bytes: u64,
    pub columns: usize,
}

#[derive(Debug, Clone)]
pub struct TransferControl {
    cancel: watch::Sender<bool>,
    progress: watch::Sender<TransferProgress>,
}
impl Default for TransferControl {
    fn default() -> Self {
        Self {
            cancel: watch::channel(false).0,
            progress: watch::channel(TransferProgress::default()).0,
        }
    }
}
impl TransferControl {
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }
    pub fn subscribe(&self) -> watch::Receiver<TransferProgress> {
        self.progress.subscribe()
    }
    pub fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }
    fn check(&self) -> Result<()> {
        ensure!(!self.is_cancelled(), "File transfer cancelled");
        Ok(())
    }
    async fn cancelled(&self) {
        let _ = self.cancel.subscribe().wait_for(|v| *v).await;
    }
    fn update(&self, phase: TransferPhase, rows: u64, bytes: u64) {
        self.progress
            .send_replace(TransferProgress { phase, rows, bytes });
    }
}
struct CancelOnDrop {
    control: TransferControl,
    armed: bool,
}
impl CancelOnDrop {
    fn new(control: &TransferControl) -> Self {
        Self {
            control: control.clone(),
            armed: true,
        }
    }
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.control.cancel();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl FileStamp {
    fn read(file: &File) -> Result<Self> {
        let m = file.metadata()?;
        ensure!(m.is_file(), "Import source must be a regular file");
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ImportPreview {
    pub columns: Vec<String>,
    pub summary: TransferSummary,
    pub sample: Vec<ImportRow>,
    source: PathBuf,
    format: TransferFormat,
    options: TransferOptions,
    fingerprint: [u8; 32],
    stamp: FileStamp,
    sealed_columns: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub sql: String,
    pub destination: PathBuf,
    pub format: TransferFormat,
    pub table: Option<TableRef>,
    pub options: TransferOptions,
}

struct HashReader {
    file: File,
    hash: Sha256,
    bytes: u64,
}
impl Read for HashReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let count = self.file.read(output)?;
        self.hash.update(&output[..count]);
        self.bytes += count as u64;
        Ok(count)
    }
}

fn open_source(path: &Path) -> Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("Open regular import file without following symlinks")
}

// Bound the raw record BEFORE asking serde/csv to allocate decoded values.
fn record<R: BufRead>(
    reader: &mut R,
    csv: bool,
    limit: usize,
    control: &TransferControl,
) -> Result<Option<Vec<u8>>> {
    let mut raw = Vec::new();
    let mut state = 0u8; // 0 field start, 1 unquoted, 2 quoted, 3 closing quote
    loop {
        if raw.len() % 8192 == 0 {
            control.check()?;
        }
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            ensure!(!csv || state != 2, "Unterminated quoted CSV field");
            return Ok((!raw.is_empty()).then_some(raw));
        }
        let byte = buffer[0];
        reader.consume(1);
        ensure!(
            raw.len() < limit,
            "Import record exceeds configured byte limit"
        );
        raw.push(byte);
        if (!csv || state != 2) && (byte == b'\n' || byte == b'\r') {
            if byte == b'\r' && reader.fill_buf()?.first() == Some(&b'\n') {
                ensure!(
                    raw.len() < limit,
                    "Import record exceeds configured byte limit"
                );
                reader.consume(1);
                raw.push(b'\n');
            }
            return Ok(Some(raw));
        }
        if csv {
            state = match (state, byte) {
                (0, b'"') => 2,
                (0, b',') => 0,
                (0, _) => 1,
                (1, b'"') => bail!("Quote inside an unquoted CSV field"),
                (1, b',') => 0,
                (1, _) => 1,
                (2, b'"') => 3,
                (2, _) => 2,
                (3, b'"') => 2,
                (3, b',') => 0,
                (3, _) => bail!("Unexpected character after a quoted CSV field"),
                _ => unreachable!(),
            };
        }
    }
}

#[derive(Default)]
struct JsonArrayReader {
    started: bool,
    after_row: bool,
    finished: bool,
}
impl JsonArrayReader {
    fn nonspace<R: BufRead>(reader: &mut R, control: &TransferControl) -> Result<Option<u8>> {
        loop {
            control.check()?;
            let buffer = reader.fill_buf()?;
            let spaces = buffer
                .iter()
                .take_while(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                .count();
            if spaces == 0 {
                return Ok(buffer.first().copied());
            }
            reader.consume(spaces);
        }
    }

    fn next<R: BufRead>(
        &mut self,
        reader: &mut R,
        limit: usize,
        control: &TransferControl,
    ) -> Result<Option<Vec<u8>>> {
        if self.finished {
            return Ok(None);
        }
        if !self.started {
            ensure!(
                Self::nonspace(reader, control)? == Some(b'['),
                "Expected a top-level JSON array"
            );
            reader.consume(1);
            self.started = true;
        }
        let mut next = Self::nonspace(reader, control)?;
        if next == Some(b']') {
            reader.consume(1);
            ensure!(
                Self::nonspace(reader, control)?.is_none(),
                "Unexpected content after JSON array"
            );
            self.finished = true;
            return Ok(None);
        }
        if self.after_row {
            ensure!(
                next == Some(b','),
                "Expected comma or closing JSON array bracket"
            );
            reader.consume(1);
            next = Self::nonspace(reader, control)?;
        }
        ensure!(
            next == Some(b'{'),
            "JSON array requires row objects (no trailing comma)"
        );
        let mut raw = Vec::new();
        let mut depth = 0;
        let mut quoted = false;
        let mut escaped = false;
        loop {
            if raw.len() % 8192 == 0 {
                control.check()?;
            }
            let byte = reader
                .fill_buf()?
                .first()
                .copied()
                .context("Unterminated JSON row object")?;
            ensure!(
                raw.len() < limit,
                "Import record exceeds configured byte limit"
            );
            raw.push(byte);
            reader.consume(1);
            if quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = false;
                }
            } else {
                match byte {
                    b'"' => quoted = true,
                    b'{' => {
                        depth += 1;
                        ensure!(depth <= 2, "Unsupported nested JSON object");
                    }
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    b'[' | b']' => bail!("Unsupported nested JSON array"),
                    _ => {}
                }
            }
        }
        self.after_row = true;
        Ok(Some(raw))
    }
}

fn column_names(names: &[String], options: &TransferOptions) -> Result<()> {
    ensure!(
        !names.is_empty() && names.len() <= options.max_columns,
        "Missing or excessive import columns"
    );
    let mut seen = HashSet::new();
    for name in names {
        ensure!(
            !name.is_empty() && name.len() <= 256 && !name.contains('\0') && seen.insert(name),
            "Invalid or duplicate import column name"
        );
    }
    Ok(())
}
fn row_bytes(row: &ImportRow) -> usize {
    std::mem::size_of::<ImportRow>()
        + row
            .iter()
            .map(|(name, cell)| {
                name.len()
                    + std::mem::size_of::<(String, Cell)>()
                    + match cell {
                        Cell::Null => 0,
                        Cell::Text(s) => s.len(),
                        Cell::Binary(b) => b.len(),
                    }
            })
            .sum::<usize>()
}
fn csv_cell(text: &str) -> Result<Cell> {
    Ok(if text == "\\N" {
        Cell::Null
    } else if let Some(s) = text.strip_prefix("\\\\") {
        Cell::Text(format!("\\{s}"))
    } else if let Some(s) = text.strip_prefix("\\x") {
        Cell::Binary(hex::decode(s).context("Invalid hex binary CSV field")?)
    } else {
        Cell::Text(text.into())
    })
}

struct Scan {
    columns: Vec<String>,
    rows: u64,
    bytes: u64,
    hash: [u8; 32],
    stamp: FileStamp,
}
fn scan_file(
    path: &Path,
    format: TransferFormat,
    options: &TransferOptions,
    control: &TransferControl,
    expected: Option<&ImportPreview>,
    mut consume: impl FnMut(Vec<ImportRow>) -> Result<()>,
) -> Result<Scan> {
    ensure!(
        format != TransferFormat::Sql,
        "SQL script import is not supported; choose lossless CSV, JSON Lines or JSON array"
    );
    options.validate()?;
    control.check()?;
    let file = open_source(path)?;
    let stamp = FileStamp::read(&file)?;
    if let Some(expected) = expected {
        ensure!(
            stamp == expected.stamp,
            "Import file metadata changed since preview"
        );
    }
    let mut reader = BufReader::with_capacity(
        64 * 1024,
        HashReader {
            file,
            hash: Sha256::new(),
            bytes: 0,
        },
    );
    // A BOM is an encoding marker only at byte zero of the file. Keep it in
    // every data field, including fields consisting solely of U+FEFF.
    if format == TransferFormat::Csv && reader.fill_buf()?.starts_with(b"\xef\xbb\xbf") {
        reader.consume(3);
    }
    let mut columns: Option<Vec<String>> = None;
    let mut count = 0;
    let mut batch = Vec::new();
    let mut batch_bytes = std::mem::size_of::<Vec<ImportRow>>();
    let mut array = JsonArrayReader::default();
    loop {
        let raw = if format == TransferFormat::JsonArray {
            array.next(&mut reader, options.max_record_bytes, control)?
        } else {
            record(
                &mut reader,
                format == TransferFormat::Csv,
                options.max_record_bytes,
                control,
            )?
        };
        let Some(raw) = raw else { break };
        let row = match format {
            TransferFormat::Csv => {
                let mut csv = csv::ReaderBuilder::new()
                    .has_headers(false)
                    // Consume CSV's start-of-input BOM detection on a blank
                    // line, so this per-record reader cannot strip data BOMs.
                    .from_reader(b"\n".as_slice().chain(raw.as_slice()));
                let Some(record) = csv.records().next() else {
                    continue;
                };
                let record = record.map_err(|_| anyhow::anyhow!("Invalid UTF-8/CSV record"))?;
                if columns.is_none() {
                    let names = record.iter().map(str::to_owned).collect::<Vec<_>>();
                    column_names(&names, options)?;
                    if let Some(expected) = expected {
                        ensure!(
                            names == expected.sealed_columns,
                            "Import header changed since preview"
                        );
                    }
                    columns = Some(names);
                    continue;
                }
                let names = columns.as_ref().expect("header loaded");
                ensure!(
                    record.len() == names.len(),
                    "Ragged CSV record at data row {}",
                    count + 1
                );
                names
                    .iter()
                    .zip(record.iter())
                    .map(|(name, text)| Ok((name.clone(), csv_cell(text)?)))
                    .collect::<Result<ImportRow>>()?
            }
            TransferFormat::JsonLines | TransferFormat::JsonArray => {
                if format == TransferFormat::JsonLines
                    && raw
                        .iter()
                        .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                {
                    continue;
                }
                let row =
                    crate::data::import_json_row(&raw, options.max_columns).with_context(|| {
                        format!("Invalid JSON row object at data row {}", count + 1)
                    })?;
                let names: Vec<_> = row.iter().map(|(name, _)| name.clone()).collect();
                column_names(&names, options)?;
                if let Some(columns) = &columns {
                    ensure!(*columns == names, "Inconsistent JSON column names");
                } else {
                    if let Some(expected) = expected {
                        ensure!(
                            names == expected.sealed_columns,
                            "Import header changed since preview"
                        );
                    }
                    columns = Some(names);
                }
                row
            }
            TransferFormat::Sql => unreachable!(),
        };
        let size = row_bytes(&row);
        ensure!(
            size + std::mem::size_of::<Vec<ImportRow>>() <= options.batch_bytes,
            "Decoded import row exceeds batch byte budget"
        );
        if !batch.is_empty()
            && (batch.len() >= options.batch_rows || batch_bytes + size > options.batch_bytes)
        {
            consume(std::mem::take(&mut batch))?;
            batch_bytes = std::mem::size_of::<Vec<ImportRow>>();
            control.update(
                if expected.is_some() {
                    TransferPhase::Applying
                } else {
                    TransferPhase::Scanning
                },
                count,
                reader.get_ref().bytes,
            );
        }
        batch.push(row);
        batch_bytes += size;
        count += 1;
    }
    control.check()?;
    if !batch.is_empty() {
        consume(batch)?;
    }
    let reader = reader.into_inner();
    ensure!(
        FileStamp::read(&reader.file)? == stamp && reader.bytes == stamp.size,
        "Import file changed while being scanned"
    );
    let hash: [u8; 32] = reader.hash.finalize().into();
    if let Some(expected) = expected {
        ensure!(
            hash == expected.fingerprint,
            "Import file content changed since preview"
        );
    }
    let columns = columns.context("Import file has no header/row schema")?;
    Ok(Scan {
        columns,
        rows: count,
        bytes: reader.bytes,
        hash,
        stamp,
    })
}

pub async fn preview_import(
    path: &Path,
    format: TransferFormat,
    options: &TransferOptions,
    control: &TransferControl,
) -> Result<ImportPreview> {
    let mut guard = CancelOnDrop::new(control);
    let path = std::path::absolute(path)?;
    let options = options.clone();
    let worker_control = control.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut sample = Vec::new();
        let mut sample_bytes = 0;
        let scan = scan_file(&path, format, &options, &worker_control, None, |batch| {
            for row in batch {
                let size = row_bytes(&row);
                if sample.len() < options.sample_rows
                    && sample_bytes + size <= options.max_sample_bytes
                {
                    sample_bytes += size;
                    sample.push(row);
                }
            }
            Ok(())
        })?;
        let summary = TransferSummary {
            rows: scan.rows,
            bytes: scan.bytes,
            columns: scan.columns.len(),
        };
        Ok::<_, anyhow::Error>(ImportPreview {
            columns: scan.columns.clone(),
            summary,
            sample,
            source: path,
            format,
            options,
            fingerprint: scan.hash,
            stamp: scan.stamp,
            sealed_columns: scan.columns,
        })
    })
    .await??;
    control.check()?;
    control.update(
        TransferPhase::Done,
        result.summary.rows,
        result.summary.bytes,
    );
    guard.armed = false;
    Ok(result)
}

/// Apply the original, fingerprinted file, not the mutable public preview sample.
/// Once Finish is requested, await the actual commit outcome even if cancelled.
pub async fn apply_import(
    database: &Database,
    table: &TableRef,
    preview: &ImportPreview,
    control: &TransferControl,
) -> Result<TransferSummary> {
    control.check()?;
    preview.options.validate()?;
    let mut guard = CancelOnDrop::new(control);
    let options = rowboat_core::RowImportOptions {
        batch_rows: preview.options.batch_rows,
        batch_bytes: preview.options.batch_bytes,
        channel_capacity: 2,
        max_cell_bytes: preview.options.max_record_bytes,
        ..Default::default()
    };
    let mut sink = tokio::select! {
        _ = control.cancelled() => bail!("File transfer cancelled"),
        sink = database.import_rows(table, options) => sink?,
    };
    let (sender, mut receiver) = mpsc::channel(2);
    let source = preview.clone();
    let worker_control = control.clone();
    let worker = tokio::task::spawn_blocking(move || {
        scan_file(
            &source.source,
            source.format,
            &source.options,
            &worker_control,
            Some(&source),
            |batch| {
                sender
                    .blocking_send(batch)
                    .context("Import consumer stopped")
            },
        )
    });
    let pumped: Result<()> = async {
        loop {
            let batch = tokio::select! {
                _ = control.cancelled() => bail!("File transfer cancelled"),
                batch = receiver.recv() => batch,
            };
            let Some(batch) = batch else { break };
            tokio::select! {
                _ = control.cancelled() => bail!("File transfer cancelled"),
                result = sink.send(batch) => result?,
            }
        }
        Ok(())
    }
    .await;
    drop(receiver);
    if pumped.is_err() {
        control.cancel();
        sink.cancel();
    }
    let scanned = worker
        .await
        .context("Import scanner task failed")
        .and_then(|r| r);
    let scanned = match pumped.and(scanned).and_then(|scan| {
        control.check()?;
        Ok(scan)
    }) {
        Ok(scan) => scan,
        Err(error) => {
            sink.close().await.context("Import rollback failed")?;
            return Err(error);
        }
    };
    // The scan has verified EOF, stable metadata, and the complete original hash.
    // Never race this future against cancellation: COMMIT may already have begun.
    let rows = sink.finish().await?;
    let summary = TransferSummary {
        rows,
        bytes: scanned.bytes,
        columns: scanned.columns.len(),
    };
    control.update(TransferPhase::Done, summary.rows, summary.bytes);
    guard.armed = false;
    Ok(summary)
}

enum ExportPart {
    Schema(Vec<Column>),
    Rows(Vec<Vec<Cell>>),
    Finish,
}
struct RecordWriter<'a> {
    file: &'a mut tempfile::NamedTempFile,
    written: usize,
    limit: usize,
}
impl Write for RecordWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.written) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Encoded export record exceeds configured byte limit",
            ));
        }
        let written = self.file.write(bytes)?;
        self.written += written;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
struct PreparedExport {
    file: tempfile::NamedTempFile,
    summary: TransferSummary,
}
fn export_worker(
    request: ExportRequest,
    engine: Engine,
    mut input: mpsc::Receiver<ExportPart>,
    control: TransferControl,
) -> Result<PreparedExport> {
    let parent = request
        .destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    let mut columns: Option<Vec<Column>> = None;
    let mut count = 0;
    while let Some(part) = input.blocking_recv() {
        control.check()?;
        match part {
            ExportPart::Schema(schema) => {
                ensure!(
                    columns.is_none(),
                    "File export supports one result set only"
                );
                column_names(
                    &schema.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
                    &request.options,
                )?;
                if request.format == TransferFormat::Csv {
                    let mut writer = csv::Writer::from_writer(RecordWriter {
                        file: &mut file,
                        written: 0,
                        limit: request.options.max_record_bytes,
                    });
                    writer.write_record(schema.iter().map(|c| &c.name))?;
                    writer.flush()?;
                }
                columns = Some(schema);
            }
            ExportPart::Rows(rows) => {
                let columns = columns
                    .as_ref()
                    .context("Stream rows arrived before schema")?;
                ensure!(
                    rows.iter().all(|r| r.len() == columns.len()),
                    "Stream row shape mismatch"
                );
                count += rows.len() as u64;
                match request.format {
                    TransferFormat::Csv => {
                        for row in rows {
                            let mut writer = csv::Writer::from_writer(RecordWriter {
                                file: &mut file,
                                written: 0,
                                limit: request.options.max_record_bytes,
                            });
                            writer.write_record(row.into_iter().map(|c| match c {
                                Cell::Null => "\\N".into(),
                                Cell::Binary(b) => format!("\\x{}", hex::encode(b)),
                                Cell::Text(s) if s.starts_with('\\') => format!("\\{s}"),
                                Cell::Text(s) => s,
                            }))?;
                            writer.flush()?;
                        }
                    }
                    TransferFormat::JsonLines => {
                        for row in rows {
                            let object: serde_json::Map<String, serde_json::Value> = columns
                                .iter()
                                .zip(row)
                                .map(|(c, v)| {
                                    (
                                        c.name.clone(),
                                        match v {
                                            Cell::Null => serde_json::Value::Null,
                                            Cell::Text(s) => serde_json::Value::String(s),
                                            Cell::Binary(b) => {
                                                serde_json::json!({"$binary":hex::encode(b)})
                                            }
                                        },
                                    )
                                })
                                .collect();
                            let mut writer = RecordWriter {
                                file: &mut file,
                                written: 0,
                                limit: request.options.max_record_bytes,
                            };
                            serde_json::to_writer(&mut writer, &object)?;
                            writer.write_all(b"\n")?;
                        }
                    }
                    TransferFormat::Sql => {
                        for row in rows {
                            let result = QueryResult {
                                columns: columns.clone(),
                                rows: vec![row],
                                ..Default::default()
                            };
                            let sql = crate::export_sql(
                                engine,
                                request
                                    .table
                                    .as_ref()
                                    .context("SQL export requires a table")?,
                                &result,
                            )?;
                            ensure!(
                                sql.len() <= request.options.max_record_bytes,
                                "Encoded SQL export record exceeds configured byte limit"
                            );
                            file.write_all(sql.as_bytes())?;
                        }
                    }
                    TransferFormat::JsonArray => {
                        bail!("Streaming JSON array export is not supported")
                    }
                }
                control.update(
                    TransferPhase::Exporting,
                    count,
                    file.as_file().metadata()?.len(),
                );
            }
            ExportPart::Finish => {
                let columns = columns.context("Query produced no exportable schema")?;
                file.as_file().sync_all()?;
                let bytes = file.as_file().metadata()?.len();
                return Ok(PreparedExport {
                    file,
                    summary: TransferSummary {
                        rows: count,
                        bytes,
                        columns: columns.len(),
                    },
                });
            }
        }
    }
    bail!("Export stream ended without successful completion")
}

pub async fn export_query(
    database: &Database,
    request: &ExportRequest,
    control: &TransferControl,
) -> Result<TransferSummary> {
    request.options.validate()?;
    control.check()?;
    ensure!(
        request.format != TransferFormat::JsonArray,
        "Streaming JSON array export is not supported; choose JSON Lines"
    );
    ensure!(
        request.format != TransferFormat::Sql || request.table.is_some(),
        "SQL export requires a target table"
    );
    ensure!(
        fs::symlink_metadata(&request.destination)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "Export destination exists or cannot be inspected"
    );
    let mut guard = CancelOnDrop::new(control);
    let mut stream = database
        .query_stream(
            &request.sql,
            QueryStreamOptions {
                row_limit: None,
                byte_limit: None,
                batch_rows: request.options.batch_rows,
                batch_bytes: request.options.batch_bytes,
                channel_capacity: 2,
                max_cell_bytes: request.options.max_record_bytes,
                ..Default::default()
            },
        )
        .await?;
    let (sender, receiver) = mpsc::channel(2);
    let worker_request = request.clone();
    let worker_control = control.clone();
    let engine = database.engine();
    let mut worker = tokio::task::spawn_blocking(move || {
        export_worker(worker_request, engine, receiver, worker_control)
    });
    let mut worker_result = None;
    let mut writer_failed = false;
    let pump: Result<()> = async {
    let mut complete = false;
    loop {
        let event = tokio::select! {
            result = &mut worker => { worker_result = Some(result); writer_failed = true; bail!("Export writer stopped"); },
            _ = control.cancelled() => { stream.cancel(); bail!("File transfer cancelled"); },
            event = stream.recv() => event,
        };
        let part = match event {
            Some(QueryEvent::Schema {
                result_index,
                columns,
                metadata_complete,
            }) => {
                ensure!(
                    result_index == 0 && metadata_complete,
                    "Export requires exactly one result with complete schema metadata"
                );
                Some(ExportPart::Schema(columns))
            }
            Some(QueryEvent::Rows { result_index, rows }) => {
                ensure!(result_index == 0, "Export supports one result only");
                Some(ExportPart::Rows(rows))
            }
            Some(QueryEvent::ResultEnd { truncated, .. }) => {
                ensure!(!truncated, "Refusing to export truncated query data");
                None
            }
            Some(QueryEvent::Complete { result_count, .. }) => {
                ensure!(result_count == 1, "Export requires one result");
                complete = true;
                Some(ExportPart::Finish)
            }
            Some(QueryEvent::Error { message, .. }) => bail!("Query export failed: {message}"),
            None => bail!("Query stream ended without successful completion"),
        };
        if let Some(part) = part {
            tokio::select! {
                result = &mut worker => { worker_result = Some(result); writer_failed = true; bail!("Export writer stopped"); },
                _ = control.cancelled() => { stream.cancel(); bail!("File transfer cancelled"); },
                result = sender.send(part) => { if result.is_err() { writer_failed = true; bail!("Export writer stopped"); } },
            }
        }
        if complete {
            break;
        }
    }
    Ok(())
    }.await;
    drop(sender);
    if pump.is_err() {
        stream.cancel();
        control.cancel();
    }
    let written = match worker_result {
        Some(result) => result,
        None => worker.await,
    }
    .context("Export writer task failed")
    .and_then(|result| result);
    if let Err(error) = pump {
        let _ = stream.close().await;
        if writer_failed {
            return Err(written
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("Export writer stopped before completion")));
        }
        return Err(error);
    }
    let prepared = written?;
    control.check()?;
    let parent = request
        .destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    prepared
        .file
        .persist_noclobber(&request.destination)
        .map_err(|error| {
            let tempfile::PersistError { error, file } = error;
            drop(file);
            error
        })
        .context("Publish export without overwriting an existing path")?;
    File::open(parent)?.sync_all()?;
    control.update(
        TransferPhase::Done,
        prepared.summary.rows,
        prepared.summary.bytes,
    );
    guard.armed = false;
    Ok(prepared.summary)
}
