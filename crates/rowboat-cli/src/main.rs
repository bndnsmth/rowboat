use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use rowboat_core::{
    BrowseRequest, ConnectionKind, ConnectionProfile, Database, Engine, NamespaceBehavior,
    QueryResult, RowChange, TableRef,
};
use rowboat_services::{ClientRuntime, Store, Tunnel};
use std::{
    io::{self, Write},
    path::PathBuf,
};

mod execution;
mod inspection;
mod live;
mod profiles;
mod response;
mod skills;
mod streaming;
mod workbench;

#[derive(Parser)]
#[command(
    name = "rowboatctl",
    version,
    about = "Rowboat PostgreSQL/MySQL/SQLite command-line workbench",
    after_help = skills::HELP
)]
struct Cli {
    /// Saved connection name or id (uses the same state directory as the GUI).
    #[arg(long, global = true, conflicts_with_all = ["url", "database_file"])]
    connection: Option<String>,
    /// postgres://user@host:port/database or mysql://user@host:port/database.
    #[arg(long, env = rowboat_services::environment_name("ROWBOAT_URL"), hide_env_values = true, global = true)]
    url: Option<String>,
    /// Open an existing SQLite database file (does not create a missing file).
    #[arg(long, global = true, conflicts_with_all = ["url", "connection", "ssh_known_hosts"])]
    database_file: Option<PathBuf>,
    /// Allow creating a SQLite database; requires --database-file.
    #[arg(
        long,
        global = true,
        requires = "database_file",
        conflicts_with = "read_only"
    )]
    create: bool,
    /// Enforce database-session read-only mode, including SQLite file access.
    #[arg(long, global = true)]
    read_only: bool,
    /// Read-only SSH known-hosts file; omit to use existing OpenSSH trust files.
    #[arg(long, env = rowboat_services::environment_name("ROWBOAT_SSH_KNOWN_HOSTS"), hide_env_values = true, global = true)]
    ssh_known_hosts: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read embedded agent skills offline; no connection or profile store needed.
    #[command(after_help = skills::HELP)]
    Skills {
        #[command(subcommand)]
        command: skills::SkillCommand,
    },
    /// Read a live schema's supported definitions and native catalog evidence as JSON.
    SchemaSnapshot {
        #[arg(long)]
        schema: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Compare two live schemas and generate a review-only destination migration.
    CompareLive(live::CompareArgs),
    /// List saved connection metadata (never passwords).
    Connections,
    /// Share, import and manage saved connection profiles without connecting.
    Profiles {
        #[command(subcommand)]
        command: profiles::ProfileCommand,
    },
    /// Test a database handshake and report server version.
    Ping,
    /// Execute SQL or a SQL file; emits every result set.
    #[command(after_help = skills::HELP)]
    Query {
        #[arg(long, required_unless_present = "file", conflicts_with = "file")]
        sql: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = execution::QueryFormat::Table)]
        format: execution::QueryFormat,
        #[command(flatten)]
        timing: execution::Timing,
    },
    /// Emit progressive newline-delimited JSON events; Complete marks success.
    QueryStream(streaming::QueryArgs),
    /// Browse a table with a typed nested JSON filter expression.
    Browse {
        table: String,
        #[arg(long)]
        schema: Option<String>,
        #[arg(long)]
        filter: Option<PathBuf>,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
    /// Inspect incoming/outgoing foreign keys with exact ordered column mappings.
    Relationships {
        table: String,
        #[arg(long)]
        schema: Option<String>,
    },
    /// Export a complete table using bounded memory and an atomic output file.
    ExportStream(streaming::ExportArgs),
    /// Scan a large CSV/JSONL file; --apply imports in one owned transaction.
    ImportStream(streaming::ImportArgs),
    /// List tables and views from the server catalog.
    Tables,
    /// Inspect functions, procedures, triggers, views, roles, or sequences.
    Objects {
        #[arg(value_enum)]
        kind: ObjectType,
    },
    /// Inspect one exact object, including its native definition and catalog evidence.
    ObjectDefinition {
        #[command(flatten)]
        object: inspection::ObjectArgs,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Review a typed JSON object patch without executing DDL.
    ObjectReview {
        #[command(flatten)]
        object: inspection::ObjectArgs,
        #[arg(long)]
        patch: PathBuf,
        #[arg(long, value_enum, default_value = "json")]
        format: inspection::ReviewFormat,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Show active sessions and running queries.
    Activity,
    /// Sample real server/database counters and gauges.
    Metrics,
    /// Explicitly sample one scalar SELECT in a bounded server read-only transaction.
    /// No retry, saved metadata, or background refresh is performed.
    MetricSample { sql: String },
    /// Show server, connection and database information.
    Info,
    /// Inspect grants for a PostgreSQL role or MySQL user (MySQL requires --host).
    Grants {
        account: String,
        #[arg(long)]
        host: Option<String>,
    },
    /// Inspect non-secret PostgreSQL role or MySQL account settings.
    AccountInfo {
        account: String,
        #[arg(long)]
        host: Option<String>,
    },
    /// List a bounded page of visible role/account identities.
    Principals {
        #[arg(long, default_value_t = 0)]
        offset: u32,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=500))]
        limit: u16,
    },
    /// Inspect effective privileges and direct/inherited grant provenance.
    Access(workbench::AccessArgs),
    /// Preview a typed privilege change; never executes GRANT/REVOKE.
    AccessReview {
        #[command(flatten)]
        access: workbench::AccessArgs,
        #[arg(long)]
        change: PathBuf,
        #[arg(long, value_enum, default_value = "json")]
        format: inspection::ReviewFormat,
    },
    /// Inspect a table's current primary/unique/foreign/check constraints.
    Constraints {
        table: String,
        #[arg(long)]
        schema: Option<String>,
    },
    /// Inspect a plan; --analyze explicitly executes the query for actual metrics.
    Explain {
        sql: String,
        #[arg(long, value_enum, default_value = "raw")]
        format: inspection::PlanFormat,
        /// Execute EXPLAIN ANALYZE once. SELECT functions can have side effects.
        #[arg(long)]
        analyze: bool,
    },
    /// Get schema-aware SQL completion candidates.
    Complete { prefix: String },
    /// Contextual completion from a bounded catalog and the live session search path.
    CompleteSql {
        sql: String,
        /// UTF-8 byte offset; defaults to the end of SQL.
        #[arg(long)]
        caret: Option<usize>,
        /// Include this namespace in addition to the live search path.
        #[arg(long)]
        schema: Option<String>,
        #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u16).range(1..=200))]
        limit: u16,
    },
    /// Expand a saved snippet template to editor text; never executes SQL.
    SnippetExpand {
        name: String,
        /// Named placeholder value; repeat for multiple values.
        #[arg(long = "value", value_parser = workbench::parse_value)]
        values: Vec<(String, String)>,
    },
    /// Generate reviewable DDL from a JSON array of typed operations, offline.
    Ddl {
        file: PathBuf,
        #[arg(long, value_enum)]
        engine: Dialect,
    },
    /// Compare two JSON schema designs and emit a reviewable DDL plan, offline.
    Diff {
        current: PathBuf,
        desired: PathBuf,
        #[arg(long, value_enum)]
        engine: Dialect,
    },
    /// Show columns, indexes, foreign keys and creation SQL.
    Structure {
        table: String,
        #[arg(long)]
        schema: Option<String>,
    },
    /// Export a bounded table page as CSV, JSON, or INSERT SQL.
    Export {
        table: String,
        #[arg(long)]
        schema: Option<String>,
        #[arg(long, value_enum, default_value_t = Format::Csv)]
        format: Format,
        #[arg(long, default_value_t = 10000)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Preview import SQL; --apply inserts all rows in one atomic batch.
    Import {
        table: String,
        file: PathBuf,
        #[arg(long)]
        schema: Option<String>,
        #[arg(long, value_enum)]
        format: ImportFormat,
        #[arg(long)]
        apply: bool,
    },
    /// Write a SQLite snapshot or a plain SQL server backup using Podman.
    Backup {
        output: PathBuf,
        /// Already-present Podman client image for server engines; unavailable for SQLite.
        #[arg(long)]
        image: Option<String>,
    },
    /// Restore a SQLite snapshot or a SQL server backup using Podman.
    Restore {
        file: PathBuf,
        #[arg(long)]
        image: Option<String>,
    },
    /// Format SQL without connecting; an explicit source selects its engine formatter.
    Format { file: PathBuf },
}

#[derive(Clone, Copy, ValueEnum, Default)]
enum Format {
    #[default]
    Table,
    Csv,
    Json,
    Sql,
}
#[derive(Clone, Copy, ValueEnum)]
enum ImportFormat {
    Csv,
    Json,
}

#[derive(Clone, Copy, ValueEnum)]
enum Dialect {
    Postgres,
    Mysql,
    Sqlite,
}
impl From<Dialect> for Engine {
    fn from(value: Dialect) -> Self {
        match value {
            Dialect::Postgres => Self::Postgres,
            Dialect::Mysql => Self::MySql,
            Dialect::Sqlite => Self::Sqlite,
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
enum ObjectType {
    Function,
    Procedure,
    Trigger,
    View,
    MaterializedView,
    Role,
    Sequence,
}
impl From<ObjectType> for rowboat_admin::objects::ObjectKind {
    fn from(value: ObjectType) -> Self {
        match value {
            ObjectType::Function => Self::Function,
            ObjectType::Procedure => Self::Procedure,
            ObjectType::Trigger => Self::Trigger,
            ObjectType::View => Self::View,
            ObjectType::MaterializedView => Self::MaterializedView,
            ObjectType::Role => Self::Role,
            ObjectType::Sequence => Self::Sequence,
        }
    }
}

fn parse_url(raw: &str) -> Result<(ConnectionProfile, Option<String>)> {
    let parsed = rowboat_services::parse_connection_url(raw)?;
    Ok((parsed.profile, parsed.password))
}

fn table(profile: &ConnectionProfile, schema: Option<String>, name: String) -> TableRef {
    TableRef {
        schema: schema.unwrap_or_else(|| match profile.engine.descriptor().namespaces {
            NamespaceBehavior::Schemas => "public".into(),
            NamespaceBehavior::Databases => profile.database.clone(),
            NamespaceBehavior::AttachedDatabases => "main".into(),
        }),
        name,
    }
}

/// Reject unavailable optional tools before connecting, opening a file, or
/// starting an external client. Provider availability is shared with the GUI.
fn check_command_support(engine: Engine, command: &Command) -> Result<()> {
    use rowboat_admin::providers::{Operation, operation_support};
    use rowboat_services::{ServiceOperation, service_support};
    match command {
        Command::Backup { .. } => service_support(engine, ServiceOperation::Backup)?,
        Command::Restore { .. } => service_support(engine, ServiceOperation::Restore)?,
        Command::Format { .. } => service_support(engine, ServiceOperation::FormatSql)?,
        _ => {}
    }
    let operation = match command {
        Command::SchemaSnapshot { .. } | Command::CompareLive(_) => {
            Some(Operation::SchemaComparison)
        }
        Command::Constraints { .. } => Some(Operation::SchemaInspection),
        Command::Objects { kind } => Some(Operation::ObjectCatalog((*kind).into())),
        Command::ObjectDefinition { .. } | Command::ObjectReview { .. } => {
            Some(Operation::ObjectEdit)
        }
        Command::Activity => Some(Operation::ServerActivity),
        Command::Metrics | Command::MetricSample { .. } => Some(Operation::ServerMetrics),
        Command::Info => Some(Operation::ConnectionInfo),
        Command::Grants { .. }
        | Command::AccountInfo { .. }
        | Command::Principals { .. }
        | Command::Access(_)
        | Command::AccessReview { .. } => Some(Operation::AccessManagement),
        Command::Explain { analyze: true, .. } => Some(Operation::Analyze),
        Command::Explain { .. } => Some(Operation::Explain),
        _ => None,
    };
    if let Some(operation) = operation {
        operation_support(engine, operation)?;
    }
    Ok(())
}

fn emit(
    result: &QueryResult,
    format: Format,
    engine: Engine,
    target: Option<&TableRef>,
) -> Result<String> {
    match format {
        Format::Csv => rowboat_services::export_csv(result),
        Format::Json => rowboat_services::export_json(result),
        Format::Sql => rowboat_services::export_sql(
            engine,
            target.context("SQL export requires a target table; use export --format sql")?,
            result,
        ),
        Format::Table => {
            let mut output = result
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
                .join("\t");
            output.push('\n');
            for row in &result.rows {
                output.push_str(
                    &row.iter()
                        .map(|v| v.display().replace('\t', "\\t").replace('\n', "\\n"))
                        .collect::<Vec<_>>()
                        .join("\t"),
                );
                output.push('\n');
            }
            Ok(output)
        }
    }
}

fn output(text: &str, destination: Option<PathBuf>) -> Result<()> {
    if let Some(path) = destination {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&path)
            .with_context(|| format!("Create {} (existing files are preserved)", path.display()))?
            .write_all(text.as_bytes())?;
    } else {
        io::stdout().lock().write_all(text.as_bytes())?;
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    if let Command::Skills { command } = &cli.command {
        return skills::run(command);
    }
    if let Command::SnippetExpand { name, values } = &cli.command {
        return workbench::snippet(name, values);
    }
    if let Command::Profiles { command } = cli.command {
        return profiles::run(command);
    }
    if matches!(
        cli.command,
        Command::Query {
            format: execution::QueryFormat::Sql,
            ..
        }
    ) {
        bail!("INSERT SQL output requires a table: use export --format sql");
    }
    if let Command::Ddl { file, engine } = &cli.command {
        rowboat_admin::providers::operation_support(
            (*engine).into(),
            rowboat_admin::providers::Operation::SchemaDdl,
        )?;
        let operations = serde_json::from_str::<Vec<rowboat_admin::ddl::DdlOperation>>(
            &std::fs::read_to_string(file)?,
        )?;
        let plan = rowboat_admin::ddl::preview((*engine).into(), &operations)?;
        for warning in &plan.warnings {
            eprintln!("{warning}");
        }
        return output(&format!("{}\n", plan.sql()), None);
    }
    if let Command::Diff {
        current,
        desired,
        engine,
    } = &cli.command
    {
        rowboat_admin::providers::operation_support(
            (*engine).into(),
            rowboat_admin::providers::Operation::SchemaComparison,
        )?;
        let current = serde_json::from_str::<Vec<rowboat_admin::ddl::TableDesign>>(
            &std::fs::read_to_string(current)?,
        )?;
        let desired = serde_json::from_str::<Vec<rowboat_admin::ddl::TableDesign>>(
            &std::fs::read_to_string(desired)?,
        )?;
        let plan = rowboat_admin::diff::compare((*engine).into(), &current, &desired)?;
        for warning in &plan.warnings {
            eprintln!("{warning}");
        }
        return output(&format!("{}\n", plan.sql()), None);
    }
    if let Command::Format { file } = &cli.command
        && cli.database_file.is_none()
        && cli.url.is_none()
        && cli.connection.is_none()
    {
        return output(
            &rowboat_services::format_sql(&std::fs::read_to_string(file)?),
            None,
        );
    }
    if matches!(cli.command, Command::Connections) {
        let state = Store::open()?.load()?;
        return output(
            &format!("{}\n", serde_json::to_string_pretty(&state.connections)?),
            None,
        );
    }
    let use_keyring = cli.connection.is_some();
    let (mut profile, url_password) = if let Some(path) = cli.database_file {
        (
            profiles::file_profile(path, cli.create, cli.read_only)?,
            None,
        )
    } else if let Some(url) = cli.url {
        parse_url(&url)?
    } else if let Some(name) = cli.connection {
        let state = Store::open()?.load()?;
        let profile = profiles::select(&state.connections, &name)?.clone();
        (profile, None)
    } else {
        bail!(
            "Provide --connection <name>, --url <database-url> (or ROWBOAT_URL), or --database-file <path>"
        );
    };
    profile.read_only |= cli.read_only;
    check_command_support(profile.engine, &cli.command)?;
    if let Command::Format { file } = &cli.command {
        return output(
            &rowboat_services::format_sql_for_engine(
                profile.engine,
                &std::fs::read_to_string(file)?,
            )?,
            None,
        );
    }
    let network = profile.engine.descriptor().connection_kind == ConnectionKind::Network;
    let password = if !network {
        String::new()
    } else if let Some(password) = rowboat_services::environment_text("ROWBOAT_PASSWORD")? {
        password
    } else if let Some(password) = url_password {
        password
    } else if use_keyring {
        rowboat_services::credential_get(&profile.id)?.unwrap_or_default()
    } else {
        String::new()
    };
    let (_tunnel, profile) = if network {
        let (tunnel, profile) = Tunnel::open_with_options(
            &profile,
            &rowboat_services::TunnelOptions {
                known_hosts_file: cli.ssh_known_hosts,
            },
        )
        .await?;
        (Some(tunnel), profile)
    } else {
        anyhow::ensure!(
            cli.ssh_known_hosts.is_none(),
            "File connections do not support --ssh-known-hosts"
        );
        rowboat_services::validate_profile(&profile)?;
        (None, profile)
    };
    match &cli.command {
        Command::Backup { output, image }
        | Command::Restore {
            file: output,
            image,
        } => {
            let runtime = if profile.engine.descriptor().connection_kind == ConnectionKind::File {
                anyhow::ensure!(image.is_none(), "File backup/restore does not use --image");
                ClientRuntime::Native
            } else {
                ClientRuntime::Podman {
                    image: match image {
                        Some(image) => image.clone(),
                        None => rowboat_services::default_backup_image(profile.engine)?.into(),
                    },
                }
            };
            if matches!(cli.command, Command::Backup { .. }) {
                rowboat_services::backup(&profile, &password, output, &runtime).await?;
                eprintln!("Backup written to {}", output.display());
            } else {
                rowboat_services::restore(&profile, &password, output, &runtime).await?;
                eprintln!("Restore completed from {}", output.display());
            }
            return Ok(());
        }
        _ => {}
    }
    let db = Database::connect(&profile, &password).await?;
    // Ctrl-C requests server-side cancellation rather than leaving a query running.
    let canceller = db.clone();
    // Streaming jobs have their own operation-scoped cancellation protocol,
    // including a commit boundary for import. Do not race it with a broad KILL.
    let scoped_cancellation = matches!(
        &cli.command,
        Command::QueryStream(_) | Command::ExportStream(_) | Command::ImportStream(_)
    );
    let cancellation = tokio::spawn(async move {
        if !scoped_cancellation
            && tokio::signal::ctrl_c().await.is_ok()
            && let Err(error) = canceller.cancel().await
        {
            eprintln!("Cancel: {error:#}");
        }
    });
    let grants_requested = matches!(&cli.command, Command::Grants { .. });
    let result = match cli.command {
        Command::SchemaSnapshot {
            schema,
            output: destination,
        } => {
            let snapshot = rowboat_admin::live_diff::snapshot_schema(&db, &schema).await?;
            output(
                &format!("{}\n", serde_json::to_string_pretty(&snapshot)?),
                destination,
            )
        }
        Command::CompareLive(arguments) => live::compare(&db, arguments).await,
        Command::QueryStream(_) => unreachable!("query commands use scoped execution"),
        Command::Browse {
            table: name,
            schema,
            filter,
            limit,
            offset,
            format,
        } => {
            let table = table(&profile, schema, name);
            let expression: Option<rowboat_core::FilterExpr> = filter
                .map(|path| {
                    serde_json::from_reader(std::fs::File::open(path)?)
                        .context("Read nested filter JSON")
                })
                .transpose()?;
            let result = db
                .browse_filtered(
                    &table,
                    &BrowseRequest {
                        limit,
                        offset,
                        ..Default::default()
                    },
                    expression.as_ref(),
                )
                .await?;
            output(&emit(&result, format, profile.engine, Some(&table))?, None)
        }
        Command::Relationships {
            table: name,
            schema,
        } => output(
            &format!(
                "{}\n",
                serde_json::to_string_pretty(
                    &db.relationships(&table(&profile, schema, name)).await?
                )?
            ),
            None,
        ),
        Command::ExportStream(arguments) => streaming::export(&db, &profile, arguments).await,
        Command::ImportStream(arguments) => streaming::import(&db, &profile, arguments).await,
        Command::Ping => {
            let version = db.server_version().await?;
            output(
                &format!(
                    "server_version\n{}\n",
                    version.replace('\t', "\\t").replace('\n', "\\n")
                ),
                None,
            )
        }
        Command::Query { .. } => unreachable!("query commands use scoped execution"),
        Command::Tables => output(
            &format!("{}\n", serde_json::to_string_pretty(&db.catalog().await?)?),
            None,
        ),
        Command::Objects { kind } => output(
            &format!(
                "{}\n",
                serde_json::to_string_pretty(
                    &rowboat_admin::objects::browse_objects(&db, kind.into()).await?
                )?
            ),
            None,
        ),
        Command::ObjectDefinition {
            object,
            output: destination,
        } => inspection::definition(&db, &profile, object, destination).await,
        Command::ObjectReview {
            object,
            patch,
            format,
            output: destination,
        } => inspection::review(&db, &profile, object, patch, format, destination).await,
        Command::Metrics => output(
            &format!(
                "{}\n",
                serde_json::to_string_pretty(&rowboat_admin::metrics::sample(&db).await?)?
            ),
            None,
        ),
        Command::MetricSample { sql } => workbench::metric_sample(&db, &sql).await,
        Command::Activity => output(
            &format!(
                "{}\n",
                serde_json::to_string_pretty(&rowboat_admin::objects::server_activity(&db).await?)?
            ),
            None,
        ),
        Command::Info => output(
            &format!(
                "{}\n",
                serde_json::to_string_pretty(&rowboat_admin::objects::connection_info(&db).await?)?
            ),
            None,
        ),
        Command::Grants { account, host } | Command::AccountInfo { account, host } => {
            let principal = rowboat_admin::access_matrix::principal(profile.engine, account, host)?;
            let results = if grants_requested {
                rowboat_admin::access::inspect_grants(&db, &principal).await?
            } else {
                rowboat_admin::access::inspect_account(&db, &principal).await?
            };
            output(
                &format!("{}\n", serde_json::to_string_pretty(&results)?),
                None,
            )
        }
        Command::Constraints {
            table: name,
            schema,
        } => {
            let target = table(&profile, schema, name);
            output(
                &format!(
                    "{}\n",
                    serde_json::to_string_pretty(
                        &rowboat_admin::ddl::constraints(&db, &target).await?
                    )?
                ),
                None,
            )
        }
        Command::Principals { offset, limit } => output(
            &format!(
                "{}\n",
                serde_json::to_string_pretty(
                    &rowboat_admin::access_matrix::list_principals(
                        &db,
                        rowboat_admin::access_matrix::PageRequest { offset, limit }
                    )
                    .await?
                )?
            ),
            None,
        ),
        Command::Access(args) => workbench::access(&db, &profile, args).await,
        Command::AccessReview {
            access,
            change,
            format,
        } => workbench::access_review(&db, &profile, access, change, format).await,
        Command::Explain {
            sql,
            format,
            analyze,
        } => inspection::explain(&db, &sql, format, analyze).await,
        Command::CompleteSql {
            sql,
            caret,
            schema,
            limit,
        } => workbench::complete(&db, &profile, &sql, caret, schema, limit).await,
        Command::Complete { prefix } => output(
            &format!(
                "{}\n",
                serde_json::to_string_pretty(
                    &rowboat_admin::query::completion_candidates(&db, &prefix).await?
                )?
            ),
            None,
        ),
        Command::Structure {
            table: name,
            schema,
        } => output(
            &format!(
                "{}\n",
                serde_json::to_string_pretty(&db.structure(&table(&profile, schema, name)).await?)?
            ),
            None,
        ),
        Command::Export {
            table: name,
            schema,
            format,
            limit,
            offset,
            output: destination,
        } => {
            let table = table(&profile, schema, name);
            let result = db
                .browse(
                    &table,
                    &BrowseRequest {
                        limit,
                        offset,
                        ..Default::default()
                    },
                )
                .await?;
            output(
                &emit(&result, format, profile.engine, Some(&table))?,
                destination,
            )
        }
        Command::Import {
            table: name,
            schema,
            file,
            format,
            apply,
        } => {
            let target = table(&profile, schema, name);
            let input = std::fs::read_to_string(file)?;
            let rows = match format {
                ImportFormat::Csv => rowboat_services::import_csv(&input)?,
                ImportFormat::Json => rowboat_services::import_json(&input)?,
            };
            let changes = rows
                .into_iter()
                .map(|values| RowChange::Insert { values })
                .collect::<Vec<_>>();
            if apply {
                eprintln!(
                    "Imported {} rows",
                    db.apply_changes(&target, &changes).await?
                );
                Ok(())
            } else {
                output(
                    &rowboat_core::sql::preview_changes(profile.engine, &target, &changes)?,
                    None,
                )
            }
        }
        _ => unreachable!(),
    };
    cancellation.abort();
    result
}

fn main() {
    #[cfg(target_os = "macos")]
    if let Some(code) = rowboat_services::credential_helper_main() {
        std::process::exit(code);
    }
    let cli = skills::parse_cli();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("CLI runtime");
    if matches!(cli.command, Command::Query { .. } | Command::QueryStream(_)) {
        let code = runtime.block_on(execution::run(cli));
        // A cancelled file read/handshake can still own a blocking worker. Never
        // let runtime destruction turn a bounded query into an unbounded exit.
        runtime.shutdown_timeout(std::time::Duration::from_millis(100));
        std::process::exit(code);
    }
    if let Err(error) = runtime.block_on(run(cli)) {
        eprintln!("rowboatctl: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoded_url_and_ipv6() {
        let (profile, password) =
            parse_url("postgres://a%40b:p%3Ass@[::1]:15435/a%20db?sslmode=require").unwrap();
        assert_eq!(profile.username, "a@b");
        assert_eq!(password.as_deref(), Some("p:ss"));
        assert_eq!(profile.host, "::1");
        assert_eq!(profile.database, "a db");
        assert_eq!(profile.port, 15435);
        assert_eq!(profile.ssl_mode, "require");
    }
    #[test]
    fn unsupported_urls_fail_without_echoing_secrets() {
        let error = parse_url("redis://user:supersecret@localhost/db")
            .unwrap_err()
            .to_string();
        assert!(!error.contains("supersecret"));
        assert!(parse_url("mysql://user@localhost/").is_err());
        assert!(parse_url("postgres://user@localhost/db?unknown=1").is_err());
    }
}
