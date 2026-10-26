use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use rowboat_admin::{
    explain::{ExplainReport, PlanNode},
    object_editor,
};
use rowboat_core::{ConnectionProfile, Database};
use std::{fmt::Write, path::PathBuf};

#[derive(Args)]
pub struct ObjectArgs {
    #[arg(value_enum)]
    kind: super::ObjectType,
    name: String,
    /// Defaults to the engine's table namespace.
    #[arg(long)]
    schema: Option<String>,
    /// Exact PostgreSQL identity argument string reported by the catalog.
    #[arg(long)]
    signature: Option<String>,
    /// Parent table of a trigger (required when its name is ambiguous).
    #[arg(long)]
    table: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ReviewFormat {
    Json,
    Sql,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum PlanFormat {
    /// Original result-array format, retained for existing clients.
    Raw,
    /// Normalized tree and complete native plan, with exact numeric strings.
    Json,
    /// Human-readable plan tree (SQLite strategy has no runtime measurements).
    Tree,
}

async fn inspect(
    db: &Database,
    profile: &ConnectionProfile,
    args: ObjectArgs,
) -> Result<object_editor::ObjectSnapshot> {
    let schema = super::table(profile, args.schema, String::new()).schema;
    let identity = object_editor::resolve_object(
        db,
        &schema,
        args.kind.into(),
        &args.name,
        args.signature.as_deref(),
        args.table.as_deref(),
    )
    .await?;
    object_editor::inspect_object(db, &identity).await
}

pub async fn definition(
    db: &Database,
    profile: &ConnectionProfile,
    args: ObjectArgs,
    destination: Option<PathBuf>,
) -> Result<()> {
    let snapshot = inspect(db, profile, args).await?;
    super::output(
        &format!("{}\n", serde_json::to_string_pretty(&snapshot)?),
        destination,
    )
}

pub async fn review(
    db: &Database,
    profile: &ConnectionProfile,
    args: ObjectArgs,
    patch: PathBuf,
    format: ReviewFormat,
    destination: Option<PathBuf>,
) -> Result<()> {
    let patch: object_editor::ObjectPatch = serde_json::from_reader(std::fs::File::open(patch)?)
        .context("Read typed object patch JSON")?;
    let snapshot = inspect(db, profile, args).await?;
    let edit = object_editor::plan_edit(&snapshot, patch)?;
    for warning in &edit.plan().warnings {
        eprintln!("{warning}");
    }
    let text = match format {
        ReviewFormat::Json => serde_json::to_string_pretty(&edit)?,
        ReviewFormat::Sql => edit.plan().sql(),
    };
    super::output(&format!("{text}\n"), destination)?;
    eprintln!(
        "Review only: no object changes executed; this snapshot does not lock concurrent DDL"
    );
    Ok(())
}

pub async fn explain(db: &Database, sql: &str, format: PlanFormat, analyze: bool) -> Result<()> {
    if analyze {
        eprintln!(
            "EXPLAIN ANALYZE executes the query once; function side effects are not automatically rolled back."
        );
        let report = rowboat_admin::explain::analyze(
            db,
            sql,
            rowboat_admin::explain::AnalyzeIntent::ExecuteQuery,
        )
        .await?;
        let text = match format {
            PlanFormat::Raw => report.raw_report.clone(),
            PlanFormat::Json => serde_json::to_string_pretty(&report)?,
            PlanFormat::Tree => plan_tree(&report),
        };
        return super::output(&format!("{text}\n"), None);
    }
    let text = match format {
        PlanFormat::Raw => {
            serde_json::to_string_pretty(&rowboat_admin::query::explain(db, sql).await?)?
        }
        PlanFormat::Json => {
            serde_json::to_string_pretty(&rowboat_admin::explain::execute(db, sql).await?)?
        }
        PlanFormat::Tree => plan_tree(&rowboat_admin::explain::execute(db, sql).await?),
    };
    super::output(&format!("{text}\n"), None)
}

fn display(value: &str) -> String {
    value.escape_debug().to_string()
}

fn plan_tree(report: &ExplainReport) -> String {
    let mut text = String::from(
        if report.mode == rowboat_admin::explain::ReportMode::Analyze {
            "Actual query profile (EXPLAIN ANALYZE; query executed)\n"
        } else if report.raw_format == rowboat_admin::explain::RawFormat::SqliteQueryPlan {
            "SQLite query strategy (EXPLAIN QUERY PLAN; no runtime measurements)\n"
        } else {
            "Estimated query plan (EXPLAIN only; no ANALYZE)\n"
        },
    );
    for (name, value) in &report.timing {
        let _ = writeln!(text, "{}: {} ms", display(name), value.0);
    }
    for warning in &report.warnings {
        let _ = writeln!(text, "Warning: {}", display(warning));
    }
    let mut pending: Vec<_> = report.roots.iter().rev().map(|node| (node, 0)).collect();
    while let Some((node, depth)) = pending.pop() {
        write_node(&mut text, node, depth);
        pending.extend(node.children.iter().rev().map(|child| (child, depth + 1)));
    }
    if report.roots.is_empty() {
        if report.raw_format == rowboat_admin::explain::RawFormat::SqliteQueryPlan {
            text.push_str("SQLite returned no query-plan rows for this statement.\n");
        } else {
            text.push_str(
                "No supported tree decoded. Use --format json to inspect the original plan.\n",
            );
        }
    }
    text.trim_end().to_owned()
}

fn write_node(text: &mut String, node: &PlanNode, depth: usize) {
    let indent = "  ".repeat(depth);
    let _ = write!(text, "{indent}- {}", display(&node.operation));
    if let Some(relation) = &node.relation {
        let _ = write!(text, " on {}", display(relation));
    }
    if let Some(access) = &node.access {
        let _ = write!(text, " [{}]", display(access));
    }
    text.push('\n');
    for (name, value) in &node.estimates {
        let _ = writeln!(text, "{indent}  estimate {}: {}", display(name), value.0);
    }
    for (name, value) in &node.actual {
        let _ = writeln!(text, "{indent}  actual {}: {}", display(name), value.0);
    }
    for (name, value) in &node.predicates {
        let _ = writeln!(text, "{indent}  {}: {}", display(name), display(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tree_preserves_large_estimates_and_nested_operations_without_terminal_controls() {
        let report = rowboat_admin::explain::decode(rowboat_core::Engine::Postgres, r#"[{"Plan":{"Node Type":"Aggregate","Plan Rows":9007199254740993,"Plans":[{"Node Type":"Seq Scan","Relation Name":"table\u001b[2J","Total Cost":12.345678901234567890,"Filter":"(id > 0)"}]}}]"#).unwrap();
        let text = plan_tree(&report);
        assert!(text.contains("9007199254740993"));
        assert!(text.contains("12.345678901234567890"));
        assert!(text.contains("\n  - Seq Scan"));
        assert!(text.contains("(id > 0)"));
        assert!(!text.contains('\u{1b}'));
    }
    #[test]
    fn unsupported_plan_stays_explicit_in_text_output() {
        let report = rowboat_admin::explain::decode(
            rowboat_core::Engine::MySql,
            r#"{"new_plan":{"cost":1}}"#,
        )
        .unwrap();
        assert!(plan_tree(&report).contains("No supported tree decoded"));
        assert!(report.raw_json.contains("new_plan"));
    }
}
