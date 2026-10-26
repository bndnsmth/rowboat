//! Shared EXPLAIN execution boundary. Native decoding is provider-owned.
pub use crate::providers::explain::*;
use anyhow::{Result, ensure};
use rowboat_core::QuerySession;

pub async fn execute(db: &impl QuerySession, sql: &str) -> Result<ExplainReport> {
    let results = crate::query::explain(db, sql).await?;
    decode_results(db.engine(), &results)
}

/// Explicit execution, exactly once. No probe, rollback sandbox, retry, or
/// fallback on decoding failure. Intent must come from the user's Analyze action.
pub async fn analyze(
    db: &impl QuerySession,
    sql: &str,
    intent: AnalyzeIntent,
) -> Result<ExplainReport> {
    let AnalyzeIntent::ExecuteQuery = intent;
    let results = db
        .query(&crate::query::analyze_sql(db.engine(), sql)?, 1)
        .await?;
    // Native prepare metadata can omit headings; validate the executed row.
    ensure!(
        results.len() == 1
            && !results[0].truncated
            && results[0].rows.len() == 1
            && results[0].rows[0].len() == 1,
        "Expected one complete native EXPLAIN ANALYZE document; query was executed, not retried (result shapes: {:?})",
        results
            .iter()
            .map(|r| (r.columns.len(), r.rows.len(), r.truncated))
            .collect::<Vec<_>>()
    );
    let raw = crate::objects::cell_text(&results[0].rows[0], 0)?
        .ok_or_else(|| anyhow::anyhow!("EXPLAIN ANALYZE returned NULL; query was executed"))?;
    decode_analyze(db.engine(), &raw)
}
