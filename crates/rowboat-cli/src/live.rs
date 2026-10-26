//! Two live connections, explicit namespace mapping, review-only output.
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use rowboat_admin::live_diff::{CompareOptions, NamespaceMapping, compare_live};
use rowboat_core::{ConnectionKind, Database};
use rowboat_services::{Store, Tunnel};
use std::path::PathBuf;

#[derive(Clone, Copy, Default, ValueEnum)]
enum ReportFormat {
    #[default]
    Json,
    Sql,
}

#[derive(Clone, Copy, ValueEnum)]
enum DefinitionScope {
    ViewQuery,
    RoutineBody,
}
impl From<DefinitionScope> for rowboat_admin::live_diff::definitions::Scope {
    fn from(scope: DefinitionScope) -> Self {
        match scope {
            DefinitionScope::ViewQuery => Self::ViewQuery,
            DefinitionScope::RoutineBody => Self::RoutineBody,
        }
    }
}

#[derive(Args)]
pub struct CompareArgs {
    /// Saved source profile ID or unique name. The global connection is the destination.
    #[arg(
        long,
        conflicts_with = "source_url",
        required_unless_present = "source_url"
    )]
    source_connection: Option<String>,
    /// Source database URL. Prefer ROWBOAT_SOURCE_URL and ROWBOAT_SOURCE_PASSWORD for credentials.
    #[arg(long, env = rowboat_services::environment_name("ROWBOAT_SOURCE_URL"), hide_env_values = true)]
    source_url: Option<String>,
    /// Independent source SSH trust file; never modifies known-hosts files.
    #[arg(long, env = rowboat_services::environment_name("ROWBOAT_SOURCE_SSH_KNOWN_HOSTS"), hide_env_values = true)]
    source_ssh_known_hosts: Option<PathBuf>,
    #[arg(long)]
    source_schema: String,
    #[arg(long)]
    destination_schema: String,
    /// Include destination-only drops in the review. Nothing is executed.
    #[arg(long)]
    drop_destination_only: bool,
    /// Compare advanced MySQL columns in one existing same-name table only.
    #[arg(long, conflicts_with_all = ["drop_destination_only", "object_kind", "object_name"])]
    columns_table: Option<String>,
    /// Compare one existing object; attributes by default, or --definition-scope.
    #[arg(
        long,
        value_enum,
        requires = "object_name",
        conflicts_with = "drop_destination_only"
    )]
    object_kind: Option<super::ObjectType>,
    #[arg(long, requires = "object_kind")]
    object_name: Option<String>,
    /// Review bounded view/routine definition changes with dependency evidence.
    #[arg(
        long,
        value_enum,
        requires = "object_kind",
        conflicts_with = "columns_table"
    )]
    definition_scope: Option<DefinitionScope>,
    /// Exact routine identity arguments, used on both mapped namespaces.
    #[arg(long, requires = "object_kind")]
    signature: Option<String>,
    /// Parent table identity for an attribute-only trigger comparison.
    #[arg(long, requires = "object_kind")]
    parent_table: Option<String>,
    #[arg(long, value_enum, default_value = "json")]
    format: ReportFormat,
    #[arg(long)]
    output: Option<PathBuf>,
}

struct Cancellation(tokio::task::JoinHandle<()>);
impl Drop for Cancellation {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn compare(destination: &Database, args: CompareArgs) -> Result<()> {
    let keyring = args.source_connection.is_some();
    let (profile, url_password) = if let Some(raw) = args.source_url {
        let parsed = rowboat_services::parse_connection_url(&raw)?;
        (parsed.profile, parsed.password)
    } else {
        let state = Store::open()?.load()?;
        (
            super::profiles::select(
                &state.connections,
                args.source_connection
                    .as_deref()
                    .context("Source connection is required")?,
            )?
            .clone(),
            None,
        )
    };
    if profile.engine != destination.engine() {
        bail!("Live comparison requires source and destination to use the same database engine");
    }
    rowboat_admin::providers::operation_support(
        profile.engine,
        rowboat_admin::providers::Operation::SchemaComparison,
    )?;
    let password = if profile.engine.descriptor().connection_kind != ConnectionKind::Network {
        String::new()
    } else if let Some(password) = rowboat_services::environment_text("ROWBOAT_SOURCE_PASSWORD")? {
        password
    } else if let Some(password) = url_password {
        password
    } else if keyring {
        rowboat_services::credential_get(&profile.id)?.unwrap_or_default()
    } else {
        String::new()
    };
    let (_tunnel, endpoint) = Tunnel::open_with_options(
        &profile,
        &rowboat_services::TunnelOptions {
            known_hosts_file: args.source_ssh_known_hosts,
        },
    )
    .await?;
    let source = Database::connect(&endpoint, &password).await?;
    let cancel_source = source.clone();
    let _cancellation = Cancellation(tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = cancel_source.cancel().await;
        }
    }));
    let mapping = NamespaceMapping {
        source: args.source_schema,
        destination: args.destination_schema,
    };
    if let Some(table) = args.columns_table {
        let report = rowboat_admin::live_diff::columns::compare_mysql_columns(
            &source,
            destination,
            &mapping,
            &table,
        )
        .await?;
        for warning in &report.warnings {
            eprintln!("{warning}");
        }
        for blocker in &report.blockers {
            eprintln!("Blocked: {blocker}");
        }
        let text = match args.format {
            ReportFormat::Json => serde_json::to_string_pretty(&report)?,
            ReportFormat::Sql => {
                if report.is_blocked() {
                    bail!("Column comparison is blocked; use JSON to inspect blockers");
                }
                report.plan.sql()
            }
        };
        super::output(&format!("{text}\n"), args.output)?;
        if report.is_blocked() {
            bail!("Column comparison contains blockers; no migration SQL generated");
        }
        eprintln!(
            "Review only: advanced columns compared; no changes executed or whole-schema equivalence claimed"
        );
        return Ok(());
    }
    if let Some(kind) = args.object_kind {
        let name = args.object_name.context("Object name is required")?;
        let src = rowboat_admin::object_editor::resolve_object(
            &source,
            &mapping.source,
            kind.into(),
            &name,
            args.signature.as_deref(),
            args.parent_table.as_deref(),
        )
        .await?;
        let dst = rowboat_admin::object_editor::resolve_object(
            destination,
            &mapping.destination,
            kind.into(),
            &name,
            args.signature.as_deref(),
            args.parent_table.as_deref(),
        )
        .await?;
        if let Some(scope) = args.definition_scope {
            let report = rowboat_admin::live_diff::definitions::compare_definitions(
                &source,
                &src,
                destination,
                &dst,
                &mapping,
                scope.into(),
            )
            .await?;
            for warning in report.warnings() {
                eprintln!("{warning}");
            }
            for blocker in report.blockers() {
                eprintln!("Blocked: {blocker}");
            }
            let text = match args.format {
                ReportFormat::Json => serde_json::to_string_pretty(&report)?,
                ReportFormat::Sql => {
                    if report.is_blocked() {
                        bail!(
                            "Definition comparison is blocked; use JSON to inspect dependency evidence"
                        );
                    }
                    report.plan().sql()
                }
            };
            super::output(&format!("{text}\n"), args.output)?;
            if report.is_blocked() {
                bail!("Definition comparison contains blockers; no changes executed");
            }
            eprintln!(
                "Review only: supported definitions compared; destination attributes retained. No changes executed."
            );
            return Ok(());
        }
        let src = rowboat_admin::object_editor::inspect_object(&source, &src).await?;
        let dst = rowboat_admin::object_editor::inspect_object(destination, &dst).await?;
        let report = rowboat_admin::live_diff::compare_object_attributes(&src, &dst, &mapping)?;
        for warning in &report.edit().plan().warnings {
            eprintln!("{warning}");
        }
        let text = match args.format {
            ReportFormat::Json => serde_json::to_string_pretty(&report)?,
            ReportFormat::Sql => report.edit().plan().sql(),
        };
        super::output(&format!("{text}\n"), args.output)?;
        eprintln!(
            "Review only: supported attributes compared; bodies/queries and runtime counters are retained"
        );
        return Ok(());
    }
    let report = compare_live(
        &source,
        destination,
        &mapping,
        CompareOptions {
            drop_destination_only: args.drop_destination_only,
        },
    )
    .await?;
    for warning in &report.warnings {
        eprintln!("{warning}");
    }
    for blocker in &report.blockers {
        eprintln!("Blocked: {blocker}");
    }
    let text = match args.format {
        ReportFormat::Json => serde_json::to_string_pretty(&report)?,
        ReportFormat::Sql => {
            if report.is_blocked() {
                bail!("Comparison is blocked; use JSON output to inspect unsupported definitions");
            }
            for warning in &report.plan.warnings {
                eprintln!("{warning}");
            }
            report.plan.sql()
        }
    };
    super::output(&format!("{text}\n"), args.output)?;
    if report.is_blocked() {
        bail!("Comparison report contains blockers; no migration SQL was generated");
    }
    eprintln!("Review only: no schema changes were executed");
    Ok(())
}
