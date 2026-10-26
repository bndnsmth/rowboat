use anyhow::Result;
use clap::{Args, ValueEnum};
use rowboat_core::{ConnectionProfile, Database, sql::qualified_table};
use rowboat_services::{TransferControl, TransferFormat, TransferOptions};
use std::path::PathBuf;

#[derive(Args)]
pub struct QueryArgs {
    #[arg(long, required_unless_present = "file", conflicts_with = "file")]
    pub sql: Option<String>,
    #[arg(long)]
    pub file: Option<PathBuf>,
    /// Maximum emitted rows per result; zero emits no rows but still executes SQL.
    #[arg(long, default_value_t = 10000, conflicts_with = "unlimited")]
    pub limit: usize,
    /// Explicitly remove total row/byte emission limits; batches remain bounded.
    #[arg(long)]
    pub unlimited: bool,
    #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u32).range(1..=4096))]
    pub batch_rows: u32,
    #[command(flatten)]
    pub timing: super::execution::Timing,
}

#[derive(Clone, Copy, ValueEnum)]
enum ExportFormat {
    Csv,
    Jsonl,
    Sql,
}
impl From<ExportFormat> for TransferFormat {
    fn from(value: ExportFormat) -> Self {
        match value {
            ExportFormat::Csv => Self::Csv,
            ExportFormat::Jsonl => Self::JsonLines,
            ExportFormat::Sql => Self::Sql,
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
enum ImportFormat {
    Csv,
    Jsonl,
    /// A bounded streaming top-level JSON array of row objects.
    #[value(alias = "json")]
    JsonArray,
}
impl From<ImportFormat> for TransferFormat {
    fn from(value: ImportFormat) -> Self {
        match value {
            ImportFormat::Csv => Self::Csv,
            ImportFormat::Jsonl => Self::JsonLines,
            ImportFormat::JsonArray => Self::JsonArray,
        }
    }
}
#[derive(Args)]
pub struct ExportArgs {
    table: String,
    #[arg(long)]
    schema: Option<String>,
    #[arg(long, value_enum, default_value = "csv")]
    format: ExportFormat,
    #[arg(long)]
    output: PathBuf,
}
#[derive(Args)]
pub struct ImportArgs {
    table: String,
    file: PathBuf,
    #[arg(long)]
    schema: Option<String>,
    #[arg(long, value_enum)]
    format: ImportFormat,
    /// Re-read the fingerprinted input and commit all rows only after successful EOF.
    #[arg(long)]
    apply: bool,
}

struct TaskGuard(tokio::task::JoinHandle<()>);
impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn cancellation(control: &TransferControl) -> TaskGuard {
    let control = control.clone();
    TaskGuard(tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            control.cancel();
        }
    }))
}

pub async fn export(db: &Database, profile: &ConnectionProfile, args: ExportArgs) -> Result<()> {
    let table = super::table(profile, args.schema, args.table);
    let control = TransferControl::default();
    let _cancel = cancellation(&control);
    let request = rowboat_services::ExportRequest {
        sql: format!("SELECT * FROM {}", qualified_table(profile.engine, &table)),
        destination: args.output,
        format: args.format.into(),
        table: Some(table),
        options: TransferOptions::default(),
    };
    let summary = rowboat_services::export_query(db, &request, &control).await?;
    eprintln!("Exported {} rows / {} bytes", summary.rows, summary.bytes);
    Ok(())
}

pub async fn import(db: &Database, profile: &ConnectionProfile, args: ImportArgs) -> Result<()> {
    let table = super::table(profile, args.schema, args.table);
    let control = TransferControl::default();
    let _cancel = cancellation(&control);
    let preview = rowboat_services::preview_import(
        &args.file,
        args.format.into(),
        &TransferOptions::default(),
        &control,
    )
    .await?;
    if args.apply {
        let summary = rowboat_services::apply_import(db, &table, &preview, &control).await?;
        eprintln!(
            "Committed {} imported rows / {} bytes",
            summary.rows, summary.bytes
        );
    } else {
        let report = serde_json::json!({"columns":preview.columns,"rows":preview.summary.rows,"bytes":preview.summary.bytes,"sample":preview.sample,"sample_only":true,"applied":false});
        super::output(
            &format!("{}\n", serde_json::to_string_pretty(&report)?),
            None,
        )?;
        eprintln!(
            "Preview only: full file scanned, bounded sample shown. Use --apply to import in one transaction."
        );
    }
    Ok(())
}
