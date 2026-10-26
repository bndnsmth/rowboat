//! Pass-5 command adapters; execution/persistence contracts stay in shared crates.
use anyhow::{Context, Result, ensure};
use clap::{Args, ValueEnum};
use rowboat_admin::access_matrix::{AccessContext, AccessObject, AccessRequest};
use rowboat_core::{ConnectionProfile, Database};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Copy, ValueEnum)]
pub enum RoleContext {
    Principal,
    DefaultRoles,
    CurrentSession,
    AvailableRoles,
}
impl From<RoleContext> for AccessContext {
    fn from(value: RoleContext) -> Self {
        match value {
            RoleContext::Principal => Self::Principal,
            RoleContext::DefaultRoles => Self::DefaultRoles,
            RoleContext::CurrentSession => Self::CurrentSession,
            RoleContext::AvailableRoles => Self::AvailableRoles,
        }
    }
}

#[derive(Args)]
pub struct AccessArgs {
    account: String,
    /// Required for MySQL, including role accounts.
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    schema: Option<String>,
    #[arg(long)]
    table: Option<String>,
    #[arg(long, requires = "table")]
    column: Option<String>,
    /// Default roles models login role activation, not every available role.
    #[arg(long, value_enum, default_value = "default-roles")]
    context: RoleContext,
}
impl AccessArgs {
    fn request(self, profile: &ConnectionProfile) -> Result<AccessRequest> {
        let principal =
            rowboat_admin::access_matrix::principal(profile.engine, self.account, self.host)?;
        let table = super::table(profile, self.schema, self.table.clone().unwrap_or_default());
        let mut objects = vec![AccessObject::Namespace(table.schema.clone())];
        if self.table.is_some() {
            objects.push(AccessObject::Table(table.clone()));
            if let Some(column) = self.column {
                objects.push(AccessObject::Column { table, column });
            }
        }
        Ok(AccessRequest {
            principal,
            objects,
            context: self.context.into(),
        })
    }
}

pub async fn access(db: &Database, profile: &ConnectionProfile, args: AccessArgs) -> Result<()> {
    let report = rowboat_admin::access_matrix::inspect_access(db, &args.request(profile)?).await?;
    super::output(
        &format!("{}\n", serde_json::to_string_pretty(&report)?),
        None,
    )
}

pub async fn metric_sample(db: &Database, sql: &str) -> Result<()> {
    use rowboat_admin::metrics::CustomMetricState;
    let sample = rowboat_admin::metrics::sample_custom(db, sql).await?;
    let state = match sample.state {
        CustomMetricState::Value { decimal } => {
            serde_json::json!({"kind":"value", "decimal":decimal})
        }
        CustomMetricState::Empty => serde_json::json!({"kind":"empty"}),
        CustomMetricState::Invalid { reason } => {
            serde_json::json!({"kind":"invalid", "reason":reason})
        }
    };
    let report = serde_json::json!({
        "sampled_at_unix_ms":sample.sampled_at_unix_ms,
        "state":state,
    });
    super::output(
        &format!("{}\n", serde_json::to_string_pretty(&report)?),
        None,
    )
}
pub async fn access_review(
    db: &Database,
    profile: &ConnectionProfile,
    args: AccessArgs,
    change: PathBuf,
    format: super::inspection::ReviewFormat,
) -> Result<()> {
    let change = serde_json::from_reader(std::fs::File::open(change)?)
        .context("Read typed access change")?;
    let plan =
        rowboat_admin::access_matrix::preview_change(db, &args.request(profile)?, &change).await?;
    let text = match format {
        super::inspection::ReviewFormat::Json => serde_json::to_string_pretty(&plan)?,
        super::inspection::ReviewFormat::Sql => plan.sql(),
    };
    for warning in &plan.warnings {
        eprintln!("{warning}");
    }
    super::output(&format!("{text}\n"), None)?;
    eprintln!(
        "Review only: no GRANT/REVOKE executed; inherited access may remain after a direct revoke."
    );
    Ok(())
}

/// Fetch visible names in the actual search path and an optional extra namespace.
/// No per-table structure requests or automatic query execution for completion.
pub async fn complete(
    db: &Database,
    _profile: &ConnectionProfile,
    sql: &str,
    caret: Option<usize>,
    schema: Option<String>,
    limit: u16,
) -> Result<()> {
    let caret = caret.unwrap_or(sql.len());
    ensure!(
        sql.len() <= 256 * 1024 && sql.is_char_boundary(caret),
        "SQL/caret exceeds completion bounds"
    );
    let metadata = db
        .completion_catalog(&schema.into_iter().collect::<Vec<_>>())
        .await?;
    let search_path = metadata.search_path;
    let mut tables = metadata
        .tables
        .into_iter()
        .map(|info| rowboat_services::CatalogTable {
            table: info.table,
            columns: Vec::new(),
        })
        .collect::<Vec<_>>();
    let build_catalog = |tables| -> Result<_> {
        let mut catalog = rowboat_services::CompletionCatalog::new(db.engine(), tables)?;
        catalog.set_search_path(Some(search_path.clone()))?;
        Ok(catalog)
    };
    let mut catalog = build_catalog(tables.clone())?;
    let preliminary = rowboat_services::complete_sql(sql, caret, &catalog, usize::from(limit))?;
    if preliminary.context == rowboat_services::CompletionContext::Expression {
        // Conservative prefetch only: the shared analyzer still decides which
        // relations resolve in scope. Avoid loading every table's columns on a
        // large server, and never silently truncate the candidate set.
        let lower_sql = sql.to_lowercase();
        let candidates = tables
            .iter_mut()
            .filter(|entry| {
                lower_sql.contains(&entry.table.name.to_lowercase())
                    || lower_sql.contains(
                        &rowboat_core::sql::quote_ident(db.engine(), &entry.table.name)
                            .to_lowercase(),
                    )
            })
            .collect::<Vec<_>>();
        ensure!(
            candidates.len() <= 32,
            "Completion metadata exceeds the 32 candidate-table bound; narrow the SQL scope"
        );
        for entry in candidates {
            entry.columns = db.completion_column_names(&entry.table).await?;
        }
        catalog = build_catalog(tables)?;
    }
    let result = rowboat_services::complete_sql(sql, caret, &catalog, usize::from(limit))?;
    let result = serde_json::json!({
        "search_path": search_path,
        "replacement": result.replacement,
        "items": result.items.iter().map(|item| serde_json::json!({
            "label": item.label, "insert_text": item.insert_text,
            "kind": format!("{:?}", item.kind), "detail": item.detail,
        })).collect::<Vec<_>>(),
        "context": format!("{:?}", result.context),
        "truncated": result.truncated, "reason": result.reason,
    });
    super::output(
        &format!("{}\n", serde_json::to_string_pretty(&result)?),
        None,
    )
}

pub fn parse_value(value: &str) -> std::result::Result<(String, String), String> {
    let (key, value) = value.split_once('=').ok_or("Expected NAME=VALUE")?;
    if key.is_empty() {
        return Err("Placeholder name cannot be empty".into());
    }
    Ok((key.into(), value.into()))
}
pub fn snippet(name: &str, values: &[(String, String)]) -> Result<()> {
    let state = rowboat_services::Store::open()?.load()?;
    let matches: Vec<_> = state
        .snippet_templates
        .iter()
        .filter(|s| s.id == name || s.name == name || s.trigger == name)
        .collect();
    ensure!(
        matches.len() == 1,
        "Expected one matching snippet id, name or trigger; found {}",
        matches.len()
    );
    let mut supplied = BTreeMap::new();
    for (key, value) in values {
        ensure!(
            supplied.insert(key.clone(), value.clone()).is_none(),
            "Duplicate placeholder: {key}"
        );
    }
    let expansion = rowboat_services::expand_snippet(matches[0], &supplied)?;
    let expansion = serde_json::json!({
        "text": expansion.text, "final_caret": expansion.final_caret,
        "tabstops": expansion.tabstops.iter().map(|stop| serde_json::json!({
            "name": stop.name, "ranges": stop.ranges,
        })).collect::<Vec<_>>(),
    });
    super::output(
        &format!("{}\n", serde_json::to_string_pretty(&expansion)?),
        None,
    )
}
