//! Embedded SQLite adapter. One native actor owns the handle, preparation,
//! stepping, decoding and authorization. No SQLite FFI escapes this module.
use crate::{stream::EventSink, *};
use anyhow::{Context, Result, bail, ensure};
use libsqlite3_sys as ffi;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc as sync_mpsc,
    },
};
use tokio::sync::{mpsc, oneshot};

mod administrative;
mod import;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Null,
    Integer(i64),
    Real(u64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}
impl Value {
    pub(super) fn literal(&self) -> String {
        match self {
            Self::Null => "NULL".into(),
            Self::Integer(v) => v.to_string(),
            Self::Real(v) => {
                let value = f64::from_bits(*v);
                if value.is_infinite() {
                    if value.is_sign_negative() {
                        "-9e999".into()
                    } else {
                        "9e999".into()
                    }
                } else {
                    format!("CAST('{value}' AS REAL)")
                }
            }
            Self::Text(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => sql::literal(Engine::Sqlite, &Cell::Text(text.into())),
                Err(_) => format!("CAST(X'{}' AS TEXT)", hex::encode(bytes)),
            },
            Self::Blob(bytes) => sql::literal(Engine::Sqlite, &Cell::Binary(bytes.clone())),
        }
    }
    pub(crate) fn cell(&self) -> Cell {
        match self {
            Self::Null => Cell::Null,
            Self::Integer(v) => Cell::Text(v.to_string()),
            Self::Real(v) => Cell::Text(f64::from_bits(*v).to_string()),
            Self::Text(v) => String::from_utf8(v.clone())
                .map(Cell::Text)
                .unwrap_or_else(|_| Cell::Binary(v.clone())),
            Self::Blob(v) => Cell::Binary(v.clone()),
        }
    }
    fn from_cell(value: &Cell) -> Self {
        match value {
            Cell::Null => Self::Null,
            Cell::Text(s) => Self::Text(s.as_bytes().to_vec()),
            Cell::Binary(b) => Self::Blob(b.clone()),
        }
    }
    fn size(&self) -> usize {
        std::mem::size_of::<Self>()
            + match self {
                Self::Text(v) | Self::Blob(v) => v.len(),
                _ => 0,
            }
    }
    pub(crate) fn storage(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Integer(_) => "integer",
            Self::Real(_) => "real",
            Self::Text(_) => "text",
            Self::Blob(_) => "blob",
        }
    }
}
#[derive(Debug, Clone, Default)]
pub(crate) struct NativeSnapshot {
    pub rows: Vec<Vec<Value>>,
    pub origins: Vec<(String, String, String)>,
    pub complete: bool,
}
pub(crate) struct Control {
    pointer: Mutex<usize>,
    read_only: bool,
    namespace_epoch: AtomicU64,
    instance: String,
    // Actor-scoped preparation only. DML authorization may be needed to compile
    // EXPLAIN; native statement classification must prove it before any step.
    readonly_prepare: AtomicBool,
}
impl Control {
    pub(crate) fn interrupt(&self) {
        let pointer = self.pointer.lock().expect("SQLite handle lifetime mutex");
        if *pointer != 0 {
            // sqlite3_interrupt is explicitly safe from another thread.
            unsafe { ffi::sqlite3_interrupt(*pointer as *mut ffi::sqlite3) };
        }
    }
}
enum Command {
    ValidateSingle {
        sql: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    FileUnmoved {
        schema: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Query {
        sql: String,
        params: Vec<Value>,
        events: mpsc::Sender<NativeEvent>,
    },
    Transaction(oneshot::Sender<bool>),
}
enum NativeEvent {
    Schema(Vec<Column>, Vec<(String, String, String)>),
    Row(Vec<Value>),
    End(u64),
    Error(anyhow::Error),
}
pub(crate) struct SqliteAdapter {
    tx: sync_mpsc::Sender<Command>,
    pub(super) control: Arc<Control>,
    pub(crate) path: PathBuf,
    pub(crate) read_only: bool,
    pub(crate) last: NativeSnapshot,
    pub(crate) browse_proof: Option<BrowseProof>,
    pub(crate) edit_reusable: bool,
}
#[derive(Clone)]
pub(crate) struct BrowseProof {
    pub table: TableRef,
    pub identity: Vec<Cell>,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    pub native: NativeSnapshot,
}
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub table: TableRef,
    pub identity: Vec<Cell>,
    pub columns: Vec<Column>,
    pub editable: Vec<bool>,
    pub signature: Vec<Vec<Cell>>,
}
impl Drop for SqliteAdapter {
    fn drop(&mut self) {
        self.control.interrupt();
    }
}
impl SqliteAdapter {
    pub(super) async fn connect(profile: &ConnectionProfile) -> Result<Self> {
        ensure!(
            profile.ssh.is_none() && profile.transport.is_none(),
            "SQLite file connections do not support SSH or network transports"
        );
        let file = profile
            .file
            .as_ref()
            .context("SQLite requires file options")?;
        ensure!(
            !file.path.as_os_str().is_empty(),
            "SQLite file path is empty"
        );
        ensure!(
            file.path.to_string_lossy() != ":memory:"
                && !file.path.to_string_lossy().starts_with("file:"),
            "SQLite requires a filesystem path, not a memory/URI connection string"
        );
        ensure!(
            !(profile.read_only && file.mode == SqliteOpenMode::CreateIfMissing),
            "Read-only SQLite cannot create a file"
        );
        let path = if file.path.is_absolute() {
            file.path.clone()
        } else {
            std::env::current_dir()?.join(&file.path)
        };
        if file.mode == SqliteOpenMode::OpenExisting {
            ensure!(
                path.is_file(),
                "SQLite file does not exist; creation must be explicitly requested"
            );
        }
        let filename = CString::new(path.as_os_str().as_encoded_bytes())
            .context("SQLite path contains NUL")?;
        let read_only = profile.read_only;
        let flags = if read_only {
            ffi::SQLITE_OPEN_READONLY
        } else {
            ffi::SQLITE_OPEN_READWRITE
                | if file.mode == SqliteOpenMode::CreateIfMissing {
                    ffi::SQLITE_OPEN_CREATE
                } else {
                    0
                }
        } | ffi::SQLITE_OPEN_FULLMUTEX;
        let control = Arc::new(Control {
            pointer: Mutex::new(0),
            read_only,
            namespace_epoch: AtomicU64::new(0),
            instance: uuid::Uuid::new_v4().to_string(),
            readonly_prepare: AtomicBool::new(false),
        });
        let shared = control.clone();
        let (tx, rx) = sync_mpsc::channel();
        let (ready, started) = oneshot::channel();
        std::thread::Builder::new()
            .name("rowboat-sqlite".into())
            .spawn(move || {
                let mut ptr = std::ptr::null_mut();
                // The actor is the sole owner; only interrupt accesses it concurrently,
                // protected against close by Control's lifetime mutex.
                let rc = unsafe {
                    ffi::sqlite3_open_v2(filename.as_ptr(), &mut ptr, flags, std::ptr::null())
                };
                if rc != ffi::SQLITE_OK {
                    let message = if ptr.is_null() {
                        "Opening SQLite failed".into()
                    } else {
                        unsafe { message(ptr) }
                    };
                    if !ptr.is_null() {
                        unsafe { ffi::sqlite3_close(ptr) };
                    }
                    let _ = ready.send(Err(message));
                    return;
                }
                *shared.pointer.lock().expect("SQLite handle lifetime mutex") = ptr as usize;
                let handle = Handle {
                    ptr,
                    control: shared,
                };
                unsafe {
                    ffi::sqlite3_busy_timeout(ptr, 5000);
                    ffi::sqlite3_enable_load_extension(ptr, 0);
                }
                let setup = unsafe {
                    exec_simple(
                        ptr,
                        if read_only {
                            "PRAGMA query_only=ON; PRAGMA foreign_keys=ON"
                        } else {
                            "PRAGMA foreign_keys=ON"
                        },
                    )
                };
                if let Err(error) = setup {
                    let _ = ready.send(Err(error.to_string()));
                    return;
                }
                unsafe {
                    ffi::sqlite3_set_authorizer(
                        ptr,
                        Some(authorize),
                        Arc::as_ptr(&handle.control).cast_mut().cast(),
                    );
                }
                if ready.send(Ok(())).is_err() {
                    return;
                }
                while let Ok(command) = rx.recv() {
                    match command {
                        Command::ValidateSingle { sql, reply } => {
                            let result =
                                unsafe { administrative::validate_single(handle.ptr, &sql) };
                            let _ = reply.send(result.map_err(|e| format!("{e:#}")));
                        }
                        Command::FileUnmoved { schema, reply } => {
                            let result =
                                unsafe { administrative::file_unmoved(handle.ptr, &schema) };
                            let _ = reply.send(result.map_err(|e| format!("{e:#}")));
                        }
                        Command::Transaction(reply) => {
                            let _ =
                                reply.send(unsafe { ffi::sqlite3_get_autocommit(handle.ptr) == 0 });
                        }
                        Command::Query {
                            sql,
                            params,
                            events,
                        } => {
                            if let Err(error) =
                                unsafe { run(handle.ptr, &handle.control, &sql, &params, &events) }
                            {
                                let _ = events
                                    .blocking_send(NativeEvent::Error(error.context("SQLite")));
                            }
                        }
                    }
                }
            })?;
        started
            .await
            .context("SQLite worker stopped opening file")?
            .map_err(anyhow::Error::msg)?;
        let path = path
            .canonicalize()
            .context("Resolving opened SQLite file")?;
        Ok(Self {
            tx,
            control,
            path,
            read_only,
            last: NativeSnapshot::default(),
            browse_proof: None,
            edit_reusable: true,
        })
    }
    fn request(&self, sql: &str, params: Vec<Value>) -> Result<mpsc::Receiver<NativeEvent>> {
        let (events, rx) = mpsc::channel(2);
        self.tx
            .send(Command::Query {
                sql: sql.into(),
                params,
                events,
            })
            .map_err(|_| anyhow::anyhow!("SQLite connection closed; reconnect required"))?;
        Ok(rx)
    }
    pub(crate) async fn in_transaction(&self) -> Result<bool> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Transaction(reply))
            .map_err(|_| anyhow::anyhow!("SQLite connection closed"))?;
        rx.await.context("SQLite state worker stopped")
    }
    pub(crate) async fn execute(&mut self, sql: &str) -> Result<()> {
        let sql = match sql {
            "START TRANSACTION" => "BEGIN",
            "START TRANSACTION READ ONLY" => "BEGIN",
            other => other,
        };
        self.query_native(sql, Vec::new(), 0).await?;
        Ok(())
    }
    pub(crate) async fn query(
        &mut self,
        sql: &str,
        params: &[Cell],
        limit: usize,
    ) -> Result<Vec<QueryResult>> {
        self.query_native(sql, params.iter().map(Value::from_cell).collect(), limit)
            .await
    }
    pub(crate) async fn query_native(
        &mut self,
        sql: &str,
        params: Vec<Value>,
        limit: usize,
    ) -> Result<Vec<QueryResult>> {
        let started = std::time::Instant::now();
        let mut rx = self.request(sql, params)?;
        let mut results = Vec::new();
        let mut current = QueryResult::default();
        let mut bytes = 0usize;
        let mut native = NativeSnapshot {
            complete: true,
            ..Default::default()
        };
        while let Some(event) = rx.recv().await {
            match event {
                NativeEvent::Schema(columns, origins) => {
                    current.columns = columns;
                    native.origins = origins;
                }
                NativeEvent::Row(values) => {
                    let size = values.iter().map(Value::size).sum::<usize>();
                    if current.rows.len() < limit && bytes.saturating_add(size) <= 64 * 1024 * 1024
                    {
                        bytes += size;
                        current.rows.push(values.iter().map(Value::cell).collect());
                        native.rows.push(values);
                    } else {
                        current.truncated = true;
                    }
                }
                NativeEvent::End(affected) => {
                    current.rows_affected = affected;
                    current.elapsed_ms = started.elapsed().as_millis() as u64;
                    results.push(std::mem::take(&mut current));
                }
                NativeEvent::Error(error) => {
                    self.last = NativeSnapshot::default();
                    bail!(error);
                }
            }
        }
        native.complete = results.len() == 1;
        self.last = native;
        Ok(results)
    }
    pub(crate) async fn stream(&mut self, sql: &str, sink: &mut EventSink) -> Result<()> {
        self.last = NativeSnapshot::default();
        let mut native = NativeSnapshot {
            complete: true,
            ..Default::default()
        };
        let mut bytes = 0usize;
        let mut count = 0;
        let mut rx = self.request(sql, Vec::new())?;
        sink.dispatch();
        while let Some(event) = rx.recv().await {
            sink.control.check()?;
            match event {
                NativeEvent::Schema(columns, origins) => {
                    native.origins = origins;
                    sink.start(columns, true).await?;
                }
                NativeEvent::Row(values) => {
                    for value in &values {
                        ensure!(
                            value.size()
                                <= sink.options.max_cell_bytes + std::mem::size_of::<Value>(),
                            "SQLite cell exceeds max_cell_bytes"
                        );
                    }
                    let before = sink.emitted_rows();
                    sink.row(values.iter().map(Value::cell).collect()).await?;
                    if sink.emitted_rows() > before && native.complete {
                        let size = values.iter().map(Value::size).sum::<usize>();
                        bytes = bytes.saturating_add(size);
                        if bytes > 64 * 1024 * 1024 || native.rows.len() >= 100_000 {
                            native = NativeSnapshot::default();
                        } else {
                            native.rows.push(values);
                        }
                    }
                }
                NativeEvent::End(affected) => {
                    count += 1;
                    sink.end(affected).await?;
                }
                NativeEvent::Error(error) => bail!(error),
            }
        }
        native.complete &= count == 1;
        self.last = native;
        sink.control.check()
    }
    async fn one(&mut self, sql: &str, params: &[Cell], limit: usize) -> Result<QueryResult> {
        let mut sets = self.query(sql, params, limit).await?;
        ensure!(sets.len() == 1, "Expected one SQLite metadata result");
        let result = sets.remove(0);
        ensure!(!result.truncated, "SQLite metadata exceeds bounds");
        Ok(result)
    }
    pub(crate) async fn databases(&mut self) -> Result<Vec<(String, String)>> {
        let result = self.one("PRAGMA database_list", &[], 129).await?;
        ensure!(
            result.rows.len() <= 128,
            "SQLite attachment count exceeds 128"
        );
        let mut databases = vec![("temp".into(), String::new())];
        for row in result.rows {
            let name = text(&row[1]);
            if name != "temp" {
                databases.push((name, text(&row[2])));
            }
        }
        Ok(databases)
    }
    pub(crate) async fn completion_catalog(
        &mut self,
        extra: &[String],
    ) -> Result<crate::CompletionCatalog> {
        let databases = self.databases().await?;
        let path: Vec<_> = databases.iter().map(|(n, _)| n.clone()).collect();
        ensure!(
            extra.iter().all(|name| path.contains(name)),
            "Unknown SQLite attached namespace"
        );
        let mut tables = Vec::new();
        for name in &path {
            tables.extend(self.tables(name).await?);
            ensure!(
                tables.len() <= 10000,
                "SQLite completion catalog exceeds 10000 relations"
            );
        }
        Ok(crate::CompletionCatalog {
            search_path: path,
            tables,
        })
    }
    pub(crate) async fn tables(&mut self, namespace: &str) -> Result<Vec<TableInfo>> {
        ensure!(
            self.databases()
                .await?
                .iter()
                .any(|(name, _)| name == namespace),
            "Unknown SQLite namespace"
        );
        let sql = format!(
            "SELECT name,upper(type) FROM {}.sqlite_schema WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name LIMIT 10001",
            sql::quote_ident(Engine::Sqlite, namespace)
        );
        let rows = self.one(&sql, &[], 10001).await?;
        ensure!(
            rows.rows.len() <= 10000,
            "SQLite catalog exceeds 10000 relations"
        );
        Ok(rows
            .rows
            .into_iter()
            .map(|r| TableInfo {
                table: TableRef {
                    schema: namespace.into(),
                    name: text(&r[0]),
                },
                kind: text(&r[1]),
            })
            .collect())
    }
    pub(crate) async fn resolve(&mut self, requested: &TableRef) -> Result<(TableRef, Vec<Cell>)> {
        let databases = self.databases().await?;
        for (schema, file) in databases {
            if !requested.schema.is_empty() && requested.schema != schema {
                continue;
            }
            let query = format!(
                "SELECT name,type,rootpage,sql FROM {}.sqlite_schema WHERE name=? COLLATE NOCASE AND type IN ('table','view') LIMIT 2",
                sql::quote_ident(Engine::Sqlite, &schema)
            );
            let rows = self
                .one(&query, &[Cell::Text(requested.name.clone())], 2)
                .await?;
            if let Some(row) = rows.rows.first() {
                ensure!(rows.rows.len() == 1, "Ambiguous SQLite relation name");
                let version = self
                    .one(
                        &format!(
                            "PRAGMA {}.schema_version",
                            sql::quote_ident(Engine::Sqlite, &schema)
                        ),
                        &[],
                        1,
                    )
                    .await?;
                let mut identity = vec![Cell::Text(file), Cell::Text(schema.clone())];
                identity.extend(row.clone());
                identity.extend(version.rows[0].clone());
                identity.push(Cell::Text(
                    self.control
                        .namespace_epoch
                        .load(Ordering::Acquire)
                        .to_string(),
                ));
                return Ok((
                    TableRef {
                        schema,
                        name: text(&row[0]),
                    },
                    identity,
                ));
            }
        }
        bail!("SQLite relation does not exist in the actual session path")
    }
    async fn column_metadata(&mut self, table: &TableRef) -> Result<Vec<Vec<Cell>>> {
        let result=self.one("SELECT cid,name,type,\"notnull\",dflt_value,pk,hidden FROM pragma_table_xinfo(?,?) ORDER BY cid LIMIT 2001",&[Cell::Text(table.name.clone()),Cell::Text(table.schema.clone())],2001).await?;
        ensure!(
            !result.rows.is_empty() && result.rows.len() <= 2000,
            "SQLite columns unavailable or exceed 2000"
        );
        Ok(result.rows)
    }
    pub(crate) async fn column_names(&mut self, table: &TableRef) -> Result<Vec<String>> {
        let table = if table.schema.is_empty() {
            self.resolve(table).await?.0
        } else {
            table.clone()
        };
        let rows = self.column_metadata(&table).await?;
        Ok(rows
            .iter()
            .filter(|r| text(&r[6]) != "1")
            .map(|r| text(&r[1]))
            .collect())
    }
    pub(crate) async fn inspect(&mut self, requested: &TableRef) -> Result<Target> {
        let (table, identity) = self.resolve(requested).await?;
        ensure!(
            text(&identity[3]) == "table"
                && !table.name.starts_with("sqlite_")
                && !text(&identity[5])
                    .to_ascii_uppercase()
                    .starts_with("CREATE VIRTUAL"),
            "SQLite query editing requires an ordinary base table, not a view/virtual/internal table"
        );
        let rows = self.column_metadata(&table).await?;
        let listed = self
            .one(
                "SELECT wr,strict FROM pragma_table_list WHERE schema=? AND name=?",
                &[
                    Cell::Text(table.schema.clone()),
                    Cell::Text(table.name.clone()),
                ],
                2,
            )
            .await?;
        ensure!(listed.rows.len() == 1, "SQLite table identity is ambiguous");
        let forced = text(&listed.rows[0][0]) == "1" || text(&listed.rows[0][1]) == "1";
        let indexes = self
            .one(
                "SELECT origin FROM pragma_index_list(?,?) WHERE origin='pk'",
                &[
                    Cell::Text(table.name.clone()),
                    Cell::Text(table.schema.clone()),
                ],
                2,
            )
            .await?;
        let count = rows.iter().filter(|r| text(&r[5]) != "0").count();
        let mut columns = Vec::new();
        let mut editable = Vec::new();
        for row in &rows {
            if text(&row[6]) == "1" {
                continue;
            }
            let pk = text(&row[5]) != "0";
            let rowid = pk
                && count == 1
                && text(&row[2]).eq_ignore_ascii_case("INTEGER")
                && indexes.rows.is_empty();
            columns.push(Column {
                name: text(&row[1]),
                data_type: text(&row[2]),
                nullable: !(text(&row[3]) == "1" || pk && (forced || rowid)),
                primary_key: pk,
                default_value: if row[4] == Cell::Null {
                    None
                } else {
                    Some(text(&row[4]))
                },
            });
            editable.push(text(&row[6]) == "0");
        }
        let mut signature = rows;
        signature.extend(listed.rows);
        signature.extend(indexes.rows);
        Ok(Target {
            table,
            identity,
            columns,
            editable,
            signature,
        })
    }
    pub(crate) async fn structure(&mut self, requested: &TableRef) -> Result<Structure> {
        let (table, identity) = self.resolve(requested).await?;
        let columns = if text(&identity[3]) == "table"
            && !text(&identity[5])
                .to_ascii_uppercase()
                .starts_with("CREATE VIRTUAL")
        {
            self.inspect(&table).await?.columns
        } else {
            self.column_metadata(&table)
                .await?
                .iter()
                .filter(|r| text(&r[6]) != "1")
                .map(|r| Column {
                    name: text(&r[1]),
                    data_type: text(&r[2]),
                    nullable: text(&r[3]) != "1",
                    primary_key: false,
                    default_value: None,
                })
                .collect()
        };
        let rows = self
            .one(
                "SELECT name,\"unique\" FROM pragma_index_list(?,?) ORDER BY seq",
                &[
                    Cell::Text(table.name.clone()),
                    Cell::Text(table.schema.clone()),
                ],
                2001,
            )
            .await?;
        ensure!(
            rows.rows.len() <= 2000,
            "SQLite index metadata exceeds bounds"
        );
        let mut indexes = Vec::new();
        for row in rows.rows {
            let name = text(&row[0]);
            let parts = self
                .one(
                    "SELECT name FROM pragma_index_xinfo(?,?) WHERE key=1 ORDER BY seqno",
                    &[Cell::Text(name.clone()), Cell::Text(table.schema.clone())],
                    2001,
                )
                .await?;
            let ddl = self
                .one(
                    &format!(
                        "SELECT sql FROM {}.sqlite_schema WHERE type='index' AND name=?",
                        sql::quote_ident(Engine::Sqlite, &table.schema)
                    ),
                    &[Cell::Text(name.clone())],
                    1,
                )
                .await?;
            indexes.push(IndexInfo {
                name,
                unique: text(&row[1]) == "1",
                columns: parts
                    .rows
                    .iter()
                    .map(|r| {
                        if r[0] == Cell::Null {
                            "<expression>".into()
                        } else {
                            text(&r[0])
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                definition: ddl
                    .rows
                    .first()
                    .and_then(|r| r.first())
                    .filter(|c| **c != Cell::Null)
                    .map(text)
                    .unwrap_or_else(|| "-- SQLite automatic index".into()),
            });
        }
        let relationships = self.outgoing_relationships(&table, &columns).await?;
        let foreign_keys = relationships
            .into_iter()
            .map(|r| ForeignKeyInfo {
                name: r.name,
                columns: r
                    .columns
                    .iter()
                    .map(|c| c.source.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
                referenced_table: sql::qualified_table(Engine::Sqlite, &r.referenced),
                referenced_columns: r
                    .columns
                    .iter()
                    .map(|c| c.referenced.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
                on_update: r.on_update,
                on_delete: r.on_delete,
            })
            .collect();
        Ok(Structure {
            columns,
            indexes,
            foreign_keys,
            definition: text(&identity[5]),
        })
    }
    pub(crate) async fn outgoing_relationships(
        &mut self,
        table: &TableRef,
        columns: &[Column],
    ) -> Result<Vec<ForeignKeyRelationship>> {
        let rows=self.one("SELECT id,seq,\"table\",\"from\",\"to\",on_update,on_delete FROM pragma_foreign_key_list(?,?) ORDER BY id,seq",&[Cell::Text(table.name.clone()),Cell::Text(table.schema.clone())],10001).await?;
        ensure!(
            rows.rows.len() <= 10000,
            "SQLite foreign keys exceed bounds"
        );
        let mut result: Vec<ForeignKeyRelationship> = Vec::new();
        for row in rows.rows {
            let name = format!("fk_{}", text(&row[0]));
            if result.last().is_none_or(|r| r.name != name) {
                result.push(ForeignKeyRelationship {
                    name,
                    source: table.clone(),
                    referenced: TableRef {
                        schema: table.schema.clone(),
                        name: text(&row[2]),
                    },
                    columns: Vec::new(),
                    on_update: text(&row[5]),
                    on_delete: text(&row[6]),
                });
            }
            let reference = if row[4] == Cell::Null {
                let cols = self
                    .column_metadata(&result.last().unwrap().referenced)
                    .await?;
                let mut pk: Vec<_> = cols.iter().filter(|r| text(&r[5]) != "0").collect();
                pk.sort_by_key(|r| text(&r[5]).parse::<usize>().unwrap_or(0));
                let ordinal = text(&row[1]).parse::<usize>()?;
                text(
                    &pk.get(ordinal)
                        .context("SQLite implicit referenced key unavailable")?[1],
                )
            } else {
                text(&row[4])
            };
            result.last_mut().unwrap().columns.push(ForeignKeyColumn {
                source: text(&row[3]),
                referenced: reference,
                nullable: columns
                    .iter()
                    .find(|c| c.name == text(&row[3]))
                    .is_none_or(|c| c.nullable),
            });
        }
        Ok(result)
    }
    pub(crate) async fn relationships(
        &mut self,
        requested: &TableRef,
    ) -> Result<Vec<ForeignKeyRelationship>> {
        let target = self.resolve(requested).await?.0;
        let mut relationships = Vec::new();
        // SQLite foreign keys are namespace-local. Enumerate base tables only;
        // this includes incoming references and preserves composite-key order.
        for info in self.tables(&target.schema).await? {
            if info.kind != "TABLE" {
                continue;
            }
            let columns = self.structure(&info.table).await?.columns;
            for relation in self.outgoing_relationships(&info.table, &columns).await? {
                if relation.source == target || relation.referenced == target {
                    relationships.push(relation);
                }
            }
            ensure!(
                relationships.len() <= 10000,
                "SQLite relationships exceed bounds"
            );
        }
        Ok(relationships)
    }
    pub(crate) async fn browse(
        &mut self,
        requested: &TableRef,
        request: &BrowseRequest,
        expression: Option<&FilterExpr>,
    ) -> Result<QueryResult> {
        self.browse_proof = None;
        self.edit_reusable = true;
        let outer = self.in_transaction().await?;
        self.edit_reusable = false;
        if !outer {
            self.execute("BEGIN").await?;
        }
        let result: Result<(QueryResult, BrowseProof)> = async {
            // The first schema read establishes the same SQLite read snapshot as
            // the subsequent row query. Names are pinned before data is fetched.
            let (table, identity) = self.resolve(requested).await?;
            let structure = self.structure(&table).await?;
            let (sql, params, limit) = super::super::browse_sql_filtered(
                Engine::Sqlite,
                &table,
                &structure.columns,
                request,
                expression,
            )?;
            let mut sets = self.query(&sql, &params, limit).await?;
            ensure!(sets.len() == 1, "Expected one SQLite browse result");
            let mut result = sets.remove(0);
            result.columns = structure.columns;
            let proof = BrowseProof {
                table,
                identity,
                columns: result.columns.clone(),
                rows: result.rows.clone(),
                native: std::mem::take(&mut self.last),
            };
            Ok((result, proof))
        }
        .await;
        if !outer {
            self.execute("ROLLBACK").await?;
        }
        ensure!(
            self.in_transaction().await? == outer,
            "SQLite browse cleanup changed transaction ownership"
        );
        self.edit_reusable = true;
        let (result, proof) = result?;
        self.browse_proof = Some(proof);
        Ok(result)
    }
    pub(crate) async fn apply(
        &mut self,
        table: &TableRef,
        changes: &[RowChange],
        proof: Option<&BrowseProof>,
        cancel: super::EditCancellation<'_>,
    ) -> Result<u64> {
        self.edit_reusable = true;
        cancel.check()?;
        ensure!(!self.read_only, "This SQLite connection is read-only");
        if changes.is_empty() {
            return Ok(0);
        }
        let outer = self.in_transaction().await?;
        let savepoint = format!("pass12_edit_{}", uuid::Uuid::new_v4().simple());
        self.edit_reusable = false;
        self.execute(&if outer {
            format!("SAVEPOINT {savepoint}")
        } else {
            "BEGIN IMMEDIATE".into()
        })
        .await?;
        let operation:Result<u64>=async {
            let target=self.inspect(table).await?;
            let mut affected=0;for change in changes {
                cancel.check()?;
                let (original,values,delete)=match change {RowChange::Insert {values}=>(None,values.as_slice(),false),RowChange::Update {original,values}=>(Some(original.as_slice()),values.as_slice(),false),RowChange::Delete {original}=>(Some(original.as_slice()),&[][..],true)};
                let mut params=Vec::new();let mut assignments=Vec::new();let mut names=std::collections::HashSet::new();
                for(name,value)in values {let index=target.columns.iter().position(|c|c.name==*name).context("Unknown SQLite edited column")?;ensure!(target.editable[index],"SQLite generated columns are read-only");ensure!(names.insert(name),"Duplicate SQLite edited column");params.push(Value::from_cell(value));assignments.push(format!("{}=?",sql::quote_ident(Engine::Sqlite,name)));}
                let source=sql::qualified_table(Engine::Sqlite,&target.table);
                let sql=if let Some(original)=original {
                    let proof=proof.context("SQLite original storage evidence unavailable; reload the table/query before editing")?;
                    ensure!(proof.table==target.table&&proof.identity==target.identity,"SQLite source identity or schema changed; reload explicitly");
                    ensure!(target.columns.iter().any(|c|c.primary_key)&&target.columns.iter().filter(|c|c.primary_key).all(|c|!c.nullable&&original.iter().any(|(name,value)|name==&c.name&&*value!=Cell::Null)),"SQLite edits require a complete non-nullable primary key");
                    ensure!(original.len()==proof.columns.len(),"SQLite originals must contain every captured column");
                    let cells:Vec<_>=proof.columns.iter().map(|c|original.iter().find(|(name,_)|name==&c.name).map(|(_,v)|v.clone()).context("Missing captured SQLite column")).collect::<Result<_>>()?;
                    let mut matches=proof.rows.iter().enumerate().filter(|(_,r)|*r==&cells);
                    let index=matches.next().map(|(i,_)|i).context("SQLite originals differ from captured values")?;
                    ensure!(matches.next().is_none(),"Ambiguous SQLite displayed originals; native row identity cannot be selected safely. Reload with a uniquely distinguishable projection");
                    let typed=proof.native.rows.get(index).context("SQLite original storage evidence incomplete")?;ensure!(proof.native.complete&&typed.len()==proof.columns.len(),"SQLite original storage evidence incomplete");
                    let mut predicate=Vec::new();for(col,value)in proof.columns.iter().zip(typed) {let name=sql::quote_ident(Engine::Sqlite,&col.name);predicate.push(format!("typeof({name})=? AND {name} COLLATE BINARY IS ?"));params.push(Value::Text(value.storage().as_bytes().to_vec()));params.push(value.clone());}
                    if delete {format!("DELETE FROM {source} WHERE {}",predicate.join(" AND "))}else{ensure!(!assignments.is_empty(),"Update has no changed values");format!("UPDATE {source} SET {} WHERE {}",assignments.join(","),predicate.join(" AND "))}
                }else if values.is_empty(){format!("INSERT INTO {source} DEFAULT VALUES")}else{format!("INSERT INTO {source} ({}) VALUES ({})",values.iter().map(|(n,_)|sql::quote_ident(Engine::Sqlite,n)).collect::<Vec<_>>().join(","),vec!["?";values.len()].join(","))};
                let result=self.query_native(&sql,params,0).await?;ensure!(result.len()==1&&result[0].rows_affected==1,"Conflict: SQLite original values or storage classes changed; batch rolled back");affected+=1;
            }Ok(affected)
        }.await;
        // cancel() uses this same gate. A cancellation acknowledged before
        // finalization rolls back; one arriving after COMMIT observes its outcome.
        let _gate = cancel.gate.lock().await;
        let operation = operation.and_then(|count| {
            cancel.check()?;
            Ok(count)
        });
        let operation = match operation {
            Ok(count) => self
                .execute(&if outer {
                    format!("RELEASE SAVEPOINT {savepoint}")
                } else {
                    "COMMIT".into()
                })
                .await
                .map(|()| count),
            Err(error) => Err(error),
        };
        match operation {
            Ok(count) => {
                ensure!(
                    self.in_transaction().await? == outer,
                    "SQLite edit finalization left unexpected transaction state"
                );
                self.edit_reusable = true;
                Ok(count)
            }
            Err(error) => {
                self.execute(&if outer {
                    format!("ROLLBACK TO SAVEPOINT {savepoint}")
                } else {
                    "ROLLBACK".into()
                })
                .await?;
                if outer {
                    self.execute(&format!("RELEASE SAVEPOINT {savepoint}"))
                        .await?;
                }
                ensure!(
                    self.in_transaction().await? == outer,
                    "SQLite edit rollback left unexpected transaction state"
                );
                self.edit_reusable = true;
                Err(error)
            }
        }
    }
}
fn text(cell: &Cell) -> String {
    match cell {
        Cell::Null => String::new(),
        Cell::Text(s) => s.clone(),
        Cell::Binary(b) => String::from_utf8_lossy(b).into_owned(),
    }
}
struct Handle {
    ptr: *mut ffi::sqlite3,
    control: Arc<Control>,
}
impl Drop for Handle {
    fn drop(&mut self) {
        let mut pointer = self
            .control
            .pointer
            .lock()
            .expect("SQLite handle lifetime mutex");
        *pointer = 0;
        unsafe { ffi::sqlite3_close(self.ptr) };
    }
}
struct Statement(*mut ffi::sqlite3_stmt);
impl Drop for Statement {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_finalize(self.0) };
    }
}
unsafe fn message(db: *mut ffi::sqlite3) -> String {
    unsafe {
        CStr::from_ptr(ffi::sqlite3_errmsg(db))
            .to_string_lossy()
            .into_owned()
    }
}
unsafe fn exec_simple(db: *mut ffi::sqlite3, sql: &str) -> Result<()> {
    let sql = CString::new(sql)?;
    let rc = unsafe {
        ffi::sqlite3_exec(
            db,
            sql.as_ptr(),
            None,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    ensure!(rc == ffi::SQLITE_OK, "{}", unsafe { message(db) });
    Ok(())
}
unsafe extern "C" fn authorize(
    context: *mut c_void,
    action: c_int,
    first: *const c_char,
    second: *const c_char,
    _database: *const c_char,
    _source: *const c_char,
) -> c_int {
    let string = |p: *const c_char| {
        if p.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(p).to_string_lossy().to_ascii_lowercase() }
        }
    };
    if action == ffi::SQLITE_FUNCTION && string(second) == "load_extension" {
        return ffi::SQLITE_DENY;
    }
    if action == ffi::SQLITE_PRAGMA
        && matches!(string(first).as_str(), "writable_schema" | "schema_version")
        && !second.is_null()
    {
        return ffi::SQLITE_DENY;
    }
    // Control is Arc-owned by Handle through sqlite3_close; never a borrowed
    // stack pointer. Preparation observes every ATTACH/DETACH, including raw SQL.
    let control = unsafe { &*context.cast::<Control>() };
    if matches!(action, ffi::SQLITE_ATTACH | ffi::SQLITE_DETACH) {
        control.namespace_epoch.fetch_add(1, Ordering::AcqRel);
    }
    if !control.read_only {
        return ffi::SQLITE_OK;
    }
    if control.readonly_prepare.load(Ordering::Relaxed)
        && matches!(
            action,
            ffi::SQLITE_INSERT | ffi::SQLITE_UPDATE | ffi::SQLITE_DELETE
        )
    {
        // This only permits compilation, not execution. Real DML is rejected by
        // sqlite3_stmt_isexplain/readonly below. PRAGMA/ATTACH/DDL keep their
        // normal authorizer restrictions. Automatic reprepare during step does
        // not enter this scope and therefore cannot authorize new writes.
        return ffi::SQLITE_OK;
    }
    match action {
        ffi::SQLITE_SELECT
        | ffi::SQLITE_READ
        | ffi::SQLITE_FUNCTION
        | ffi::SQLITE_TRANSACTION
        | ffi::SQLITE_SAVEPOINT
        | ffi::SQLITE_RECURSIVE => ffi::SQLITE_OK,
        ffi::SQLITE_PRAGMA => {
            let name = string(first);
            if matches!(
                name.as_str(),
                "table_info"
                    | "table_xinfo"
                    | "index_list"
                    | "index_info"
                    | "index_xinfo"
                    | "foreign_key_list"
                    | "database_list"
                    | "table_list"
            ) || second.is_null()
                && matches!(
                    name.as_str(),
                    "query_only" | "schema_version" | "data_version" | "compile_options"
                )
            {
                ffi::SQLITE_OK
            } else {
                ffi::SQLITE_DENY
            }
        }
        _ => ffi::SQLITE_DENY,
    }
}
unsafe fn run(
    db: *mut ffi::sqlite3,
    control: &Control,
    sql: &str,
    params: &[Value],
    events: &mpsc::Sender<NativeEvent>,
) -> Result<()> {
    let sql = CString::new(sql).context("SQL contains NUL")?;
    let mut cursor = sql.as_ptr();
    let end = unsafe { cursor.add(sql.as_bytes().len()) };
    let mut count = 0;
    while cursor < end {
        let mut statement = std::ptr::null_mut();
        let mut tail = std::ptr::null();
        control
            .readonly_prepare
            .store(control.read_only, Ordering::Relaxed);
        let rc = unsafe {
            ffi::sqlite3_prepare_v3(
                db,
                cursor,
                end.offset_from(cursor) as i32,
                0,
                &mut statement,
                &mut tail,
            )
        };
        control.readonly_prepare.store(false, Ordering::Relaxed);
        if rc != ffi::SQLITE_OK {
            let offset = unsafe { ffi::sqlite3_error_offset(db) };
            let message = unsafe { message(db) };
            if offset >= 0 {
                return Err(crate::execution::LocatedError {
                    message,
                    position: crate::execution::ErrorPosition {
                        byte_offset: unsafe { cursor.offset_from(sql.as_ptr()) } as usize
                            + offset as usize,
                    },
                }
                .into());
            }
            bail!(message);
        }
        ensure!(tail > cursor, "SQLite preparation made no progress");
        cursor = tail;
        if statement.is_null() {
            continue;
        }
        let statement = Statement(statement);
        if control.read_only {
            ensure!(
                unsafe { ffi::sqlite3_stmt_isexplain(statement.0) } != 0
                    || unsafe { ffi::sqlite3_stmt_readonly(statement.0) } != 0,
                "Read-only SQLite permits native EXPLAIN planning, not execution of modifying statements"
            );
        }
        count += 1;
        ensure!(count <= 256, "More than 256 SQLite result sets");
        let parameters = unsafe { ffi::sqlite3_bind_parameter_count(statement.0) } as usize;
        ensure!(
            parameters == params.len(),
            "SQLite parameter count differs from bound values"
        );
        ensure!(
            params.is_empty() || count == 1,
            "Bound SQLite operations require one statement"
        );
        for (index, value) in params.iter().enumerate() {
            let i = index as i32 + 1;
            let rc = unsafe {
                match value {
                    Value::Null => ffi::sqlite3_bind_null(statement.0, i),
                    Value::Integer(v) => ffi::sqlite3_bind_int64(statement.0, i, *v),
                    Value::Real(v) => ffi::sqlite3_bind_double(statement.0, i, f64::from_bits(*v)),
                    Value::Text(v) => ffi::sqlite3_bind_text64(
                        statement.0,
                        i,
                        v.as_ptr().cast(),
                        v.len() as u64,
                        ffi::SQLITE_TRANSIENT(),
                        ffi::SQLITE_UTF8 as u8,
                    ),
                    Value::Blob(v) => ffi::sqlite3_bind_blob64(
                        statement.0,
                        i,
                        v.as_ptr().cast(),
                        v.len() as u64,
                        ffi::SQLITE_TRANSIENT(),
                    ),
                }
            };
            ensure!(rc == ffi::SQLITE_OK, "SQLite value binding failed");
        }
        let width = unsafe { ffi::sqlite3_column_count(statement.0) };
        ensure!(width <= 4096, "SQLite result exceeds 4096 columns");
        let columns = (0..width)
            .map(|i| unsafe {
                let name = CStr::from_ptr(ffi::sqlite3_column_name(statement.0, i))
                    .to_string_lossy()
                    .into_owned();
                let declared = ffi::sqlite3_column_decltype(statement.0, i);
                Column {
                    name,
                    data_type: if declared.is_null() {
                        "DYNAMIC".into()
                    } else {
                        CStr::from_ptr(declared).to_string_lossy().into_owned()
                    },
                    nullable: true,
                    primary_key: false,
                    default_value: None,
                }
            })
            .collect();
        let origins = (0..width)
            .map(|i| unsafe {
                let string = |p: *const c_char| {
                    if p.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr(p).to_string_lossy().into_owned()
                    }
                };
                (
                    string(ffi::sqlite3_column_database_name(statement.0, i)),
                    string(ffi::sqlite3_column_table_name(statement.0, i)),
                    string(ffi::sqlite3_column_origin_name(statement.0, i)),
                )
            })
            .collect();
        if events
            .blocking_send(NativeEvent::Schema(columns, origins))
            .is_err()
        {
            return Ok(());
        }
        let before = unsafe { ffi::sqlite3_total_changes64(db) };
        let mut rows = 0u64;
        loop {
            let rc = unsafe { ffi::sqlite3_step(statement.0) };
            if rc == ffi::SQLITE_DONE {
                break;
            }
            ensure!(rc == ffi::SQLITE_ROW, "{}", unsafe { message(db) });
            let mut row = Vec::with_capacity(width as usize);
            let mut bytes = 0usize;
            for i in 0..width {
                let kind = unsafe { ffi::sqlite3_column_type(statement.0, i) };
                let value = unsafe {
                    match kind {
                        ffi::SQLITE_NULL => Value::Null,
                        ffi::SQLITE_INTEGER => {
                            Value::Integer(ffi::sqlite3_column_int64(statement.0, i))
                        }
                        ffi::SQLITE_FLOAT => {
                            Value::Real(ffi::sqlite3_column_double(statement.0, i).to_bits())
                        }
                        _ => {
                            let n = ffi::sqlite3_column_bytes(statement.0, i) as usize;
                            ensure!(n <= 16 * 1024 * 1024, "SQLite cell exceeds 16 MiB");
                            let p = if kind == ffi::SQLITE_BLOB {
                                ffi::sqlite3_column_blob(statement.0, i).cast::<u8>()
                            } else {
                                ffi::sqlite3_column_text(statement.0, i)
                            };
                            let value = if n == 0 {
                                Vec::new()
                            } else {
                                std::slice::from_raw_parts(p, n).to_vec()
                            };
                            if kind == ffi::SQLITE_BLOB {
                                Value::Blob(value)
                            } else {
                                Value::Text(value)
                            }
                        }
                    }
                };
                bytes = bytes.saturating_add(value.size());
                ensure!(bytes <= 32 * 1024 * 1024, "SQLite row exceeds 32 MiB");
                row.push(value);
            }
            rows += 1;
            if events.blocking_send(NativeEvent::Row(row)).is_err() {
                return Ok(());
            }
        }
        let affected = if width > 0 {
            rows
        } else {
            let changed = unsafe { ffi::sqlite3_total_changes64(db) } - before;
            if changed > 0 {
                (unsafe { ffi::sqlite3_changes64(db) }) as u64
            } else {
                0
            }
        };
        if events.blocking_send(NativeEvent::End(affected)).is_err() {
            return Ok(());
        }
    }
    Ok(())
}
