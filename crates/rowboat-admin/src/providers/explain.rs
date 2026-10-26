//! Estimate-only EXPLAIN and explicit executing profiles. Native reports survive
//! normalization, including unknown fields and MySQL TREE numeric spelling.
use super::{Operation, ServerKind, operation_support, server};
use anyhow::{Result, ensure};
use rowboat_core::{Cell, Engine, QueryResult, ToolProvider, ToolSupport};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn prefix(engine: Engine, analyze: bool) -> Result<&'static str> {
    let operation = if analyze {
        Operation::Analyze
    } else {
        Operation::Explain
    };
    Ok(match (provider(engine, operation)?, analyze) {
        (ToolProvider::Postgres, false) => "EXPLAIN (FORMAT JSON)",
        (ToolProvider::MySql, false) => "EXPLAIN FORMAT=JSON",
        (ToolProvider::Sqlite, false) => "EXPLAIN QUERY PLAN",
        (ToolProvider::Postgres, true) => "EXPLAIN (ANALYZE TRUE, FORMAT JSON)",
        (ToolProvider::MySql, true) => "EXPLAIN ANALYZE FORMAT=TREE",
        (ToolProvider::Sqlite, true) => unreachable!("SQLite Analyze rejected by provider"),
    })
}

/// Tool dispatch is independent of the deliberately server-only admin helper.
fn provider(engine: Engine, operation: Operation) -> Result<ToolProvider> {
    operation_support(engine, operation)?;
    match engine.descriptor().tools.explain {
        ToolSupport::Supported(provider) => {
            if provider == ToolProvider::Sqlite && operation == Operation::Analyze {
                return Err(super::UnsupportedOperation {
                    engine,
                    operation,
                    reason: "SQLite ANALYZE gathers statistics; it is not query runtime profiling",
                }
                .into());
            }
            Ok(provider)
        }
        ToolSupport::Unsupported(reason) => Err(super::UnsupportedOperation {
            engine,
            operation,
            reason,
        }
        .into()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExactNumber(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalyzeIntent {
    ExecuteQuery,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportMode {
    Explain,
    Analyze,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RawFormat {
    Json,
    Tree,
    SqliteQueryPlan,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanNode {
    pub operation: String,
    pub relation: Option<String>,
    pub access: Option<String>,
    /// Native metric names retain their units/meaning; no inferred costs or rows.
    pub estimates: BTreeMap<String, ExactNumber>,
    pub actual: BTreeMap<String, ExactNumber>,
    pub predicates: BTreeMap<String, String>,
    pub children: Vec<PlanNode>,
    pub raw: Value,
}
#[derive(Debug, Clone, Serialize)]
pub struct ExplainReport {
    pub engine: Engine,
    pub mode: ReportMode,
    pub raw_format: RawFormat,
    /// Original JSON/TREE, or a lossless JSON array of native SQLite plan rows.
    pub raw_report: String,
    /// Compatibility field: original server JSON; empty for native TREE/SQLite.
    pub raw_json: String,
    /// Native report-level timing in milliseconds; never synthesized from nodes.
    pub timing: BTreeMap<String, ExactNumber>,
    pub roots: Vec<PlanNode>,
    pub warnings: Vec<String>,
}

pub fn decode_analyze(engine: Engine, raw: &str) -> Result<ExplainReport> {
    provider(engine, Operation::Analyze)?;
    match server(engine, Operation::Analyze)? {
        ServerKind::Postgres => {
            let mut report = decode(engine, raw)?;
            report.mode = ReportMode::Analyze;
            Ok(report)
        }
        ServerKind::MySql => mysql_tree(raw),
    }
}

pub fn decode(engine: Engine, json: &str) -> Result<ExplainReport> {
    let provider = provider(engine, Operation::Explain)?;
    if provider == ToolProvider::Sqlite {
        ensure!(
            json.len() <= 8 * 1024 * 1024,
            "SQLite plan JSON exceeds 8 MiB decoder limit"
        );
        return decode_sqlite_rows(&serde_json::from_str::<Vec<SqlitePlanRow>>(json)?);
    }
    let raw: Value = serde_json::from_str(json)?;
    let mut report = ExplainReport {
        engine,
        mode: ReportMode::Explain,
        raw_format: RawFormat::Json,
        raw_report: json.into(),
        raw_json: json.into(),
        timing: BTreeMap::new(),
        roots: vec![],
        warnings: vec![],
    };
    match provider {
        ToolProvider::Postgres => {
            if let Some(plans) = raw.as_array() {
                for entry in plans {
                    for key in ["Planning Time", "Execution Time"] {
                        if let Some(n) = numeric(entry, key, &mut report.warnings) {
                            report.timing.insert(key.into(), n);
                        }
                    }
                    if let Some(plan) = entry.get("Plan").filter(|v| v.is_object()) {
                        report.roots.push(pg_node(plan, &mut report.warnings));
                    } else {
                        report.warnings.push(
                            "PostgreSQL entry has no object-valued Plan; retained raw document"
                                .into(),
                        );
                    }
                }
            } else {
                report
                    .warnings
                    .push("Unsupported PostgreSQL JSON root (expected array)".into());
            }
        }
        ToolProvider::MySql => {
            if let Some(block) = raw.get("query_block").filter(|v| v.is_object()) {
                report
                    .roots
                    .push(mysql_node("query_block", block, &mut report.warnings));
            } else {
                report.warnings.push(
                    "Unsupported MySQL JSON shape (expected query_block); retained raw document"
                        .into(),
                );
            }
        }
        ToolProvider::Sqlite => unreachable!("SQLite rows decoded above"),
    }
    if report.roots.is_empty() {
        report
            .warnings
            .push("No supported plan tree decoded; use raw JSON".into());
    }
    Ok(report)
}

pub(crate) fn decode_results(engine: Engine, results: &[QueryResult]) -> Result<ExplainReport> {
    let provider = provider(engine, Operation::Explain)?;
    ensure!(
        results.len() == 1 && !results[0].truncated,
        "Expected one complete EXPLAIN result"
    );
    let result = &results[0];
    if provider == ToolProvider::Sqlite {
        return decode_sqlite(result);
    }
    ensure!(
        result.rows.len() == 1 && result.columns.len() == 1 && result.rows[0].len() == 1,
        "Expected one complete JSON EXPLAIN document"
    );
    let json = crate::objects::cell_text(&result.rows[0], 0)?
        .ok_or_else(|| anyhow::anyhow!("EXPLAIN returned NULL"))?;
    decode(engine, &json)
}

/// SQLite's four EXPLAIN QUERY PLAN columns, without parsing names or metrics
/// from the version-dependent detail text. Row order and native IDs are retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlitePlanRow {
    pub id: i64,
    pub parent: i64,
    pub aux: i64,
    pub detail: String,
}

const SQLITE_MAX_ROWS: usize = 1000;
const SQLITE_MAX_DEPTH: usize = 128;
const SQLITE_MAX_DETAIL_BYTES: usize = 1024 * 1024;

pub fn decode_sqlite(result: &QueryResult) -> Result<ExplainReport> {
    ensure!(
        !result.truncated,
        "SQLite EXPLAIN QUERY PLAN was truncated; complete plan unavailable"
    );
    ensure!(
        result.columns.len() == 4,
        "Expected SQLite EXPLAIN QUERY PLAN's four native columns"
    );
    ensure!(
        result.columns[0].name == "id"
            && result.columns[1].name == "parent"
            && matches!(result.columns[2].name.as_str(), "notused" | "aux")
            && result.columns[3].name == "detail",
        "Unrecognized SQLite EXPLAIN QUERY PLAN column layout"
    );
    ensure!(
        result.rows.len() <= SQLITE_MAX_ROWS,
        "SQLite plan exceeds 1000 rows"
    );
    fn text(cell: &Cell) -> Result<&str> {
        match cell {
            Cell::Text(text) => Ok(text),
            _ => anyhow::bail!("SQLite plan columns must contain non-NULL native integers/text"),
        }
    }
    let mut rows = Vec::with_capacity(result.rows.len());
    let mut bytes = 0usize;
    for row in &result.rows {
        ensure!(
            row.len() == 4,
            "Expected four cells in each SQLite plan row"
        );
        let detail = text(&row[3])?;
        bytes = bytes.saturating_add(detail.len());
        ensure!(
            bytes <= SQLITE_MAX_DETAIL_BYTES,
            "SQLite plan details exceed 1 MiB"
        );
        rows.push(SqlitePlanRow {
            id: text(&row[0])?.parse()?,
            parent: text(&row[1])?.parse()?,
            aux: text(&row[2])?.parse()?,
            detail: detail.into(),
        });
    }
    decode_sqlite_rows(&rows)
}

/// Validate the whole graph before constructing nested nodes. Iterative walks
/// reject cycles/depth overflow before recursive consumers (including Drop and
/// serializers) can see an unbounded tree. Unknown detail text is valid.
pub fn decode_sqlite_rows(rows: &[SqlitePlanRow]) -> Result<ExplainReport> {
    ensure!(
        rows.len() <= SQLITE_MAX_ROWS,
        "SQLite plan exceeds 1000 rows"
    );
    ensure!(
        rows.iter()
            .fold(0usize, |n, r| n.saturating_add(r.detail.len()))
            <= SQLITE_MAX_DETAIL_BYTES,
        "SQLite plan details exceed 1 MiB"
    );
    let mut ids = BTreeMap::new();
    for (index, row) in rows.iter().enumerate() {
        ensure!(
            row.id >= 0 && row.parent >= 0,
            "SQLite plan IDs must be nonnegative"
        );
        ensure!(
            ids.insert(row.id, index).is_none(),
            "Duplicate SQLite plan ID {}",
            row.id
        );
    }
    let mut parents = Vec::with_capacity(rows.len());
    for row in rows {
        // Zero is SQLite's root sentinel, not a synthetic normalized node ID.
        let parent = if row.parent == 0 {
            None
        } else {
            Some(*ids.get(&row.parent).ok_or_else(|| {
                anyhow::anyhow!(
                    "SQLite plan ID {} refers to missing parent {}",
                    row.id,
                    row.parent
                )
            })?)
        };
        parents.push(parent);
    }
    let mut depths = Vec::with_capacity(rows.len());
    for index in 0..rows.len() {
        let mut seen = std::collections::BTreeSet::new();
        let mut current = Some(index);
        while let Some(i) = current {
            ensure!(seen.insert(i), "Cycle in SQLite plan at ID {}", rows[i].id);
            ensure!(
                seen.len() <= SQLITE_MAX_DEPTH,
                "SQLite plan exceeds depth 128"
            );
            current = parents[i];
        }
        depths.push(seen.len());
    }
    let mut children = vec![Vec::new(); rows.len()];
    let mut root_indices = Vec::new();
    for (index, parent) in parents.iter().enumerate() {
        if let Some(parent) = parent {
            children[*parent].push(index);
        } else {
            root_indices.push(index);
        }
    }
    // Build bottom-up. Siblings and roots use source order, not sorted IDs.
    let mut order: Vec<_> = (0..rows.len()).collect();
    order.sort_by_key(|i| std::cmp::Reverse(depths[*i]));
    let mut nodes: Vec<Option<PlanNode>> = vec![None; rows.len()];
    for index in order {
        let row = &rows[index];
        let mut node = base(row.detail.clone(), &serde_json::to_value(row)?);
        node.access = ["SCAN", "SEARCH"]
            .into_iter()
            .find(|token| {
                row.detail
                    .strip_prefix(token)
                    .is_some_and(|tail| tail.starts_with(' '))
            })
            .map(str::to_owned);
        node.children = children[index]
            .iter()
            .map(|i| nodes[*i].take().expect("child built first"))
            .collect();
        nodes[index] = Some(node);
    }
    Ok(ExplainReport {
        engine: Engine::Sqlite,
        mode: ReportMode::Explain,
        raw_format: RawFormat::SqliteQueryPlan,
        raw_report: serde_json::to_string_pretty(rows)?,
        raw_json: String::new(),
        timing: BTreeMap::new(),
        roots: root_indices
            .into_iter()
            .map(|i| nodes[i].take().expect("root built"))
            .collect(),
        warnings: if rows.is_empty() {
            vec!["SQLite returned no query-plan rows for this statement; no runtime measurements are available".into()]
        } else {
            vec![]
        },
    })
}
fn numeric(v: &Value, key: &str, warnings: &mut Vec<String>) -> Option<ExactNumber> {
    let value = v.get(key)?;
    let text = match value {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => {
            warnings.push(format!("Non-numeric plan field {key}; retained raw value"));
            return None;
        }
    };
    // Parse only to validate JSON numeric syntax, never convert to floating point.
    if !matches!(serde_json::from_str::<Value>(&text), Ok(Value::Number(_))) {
        warnings.push(format!(
            "Invalid numeric plan field {key}; retained raw value"
        ));
        return None;
    }
    Some(ExactNumber(text))
}
fn base(operation: String, v: &Value) -> PlanNode {
    PlanNode {
        operation,
        relation: None,
        access: None,
        estimates: BTreeMap::new(),
        actual: BTreeMap::new(),
        predicates: BTreeMap::new(),
        children: vec![],
        raw: v.clone(),
    }
}
fn pg_node(v: &Value, w: &mut Vec<String>) -> PlanNode {
    let operation = v
        .get("Node Type")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            w.push("Plan node has no textual Node Type".into());
            "Unknown"
        })
        .to_owned();
    let mut node = base(operation, v);
    node.relation = v
        .get("Relation Name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    node.access = v
        .get("Index Name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    for key in ["Startup Cost", "Total Cost", "Plan Rows", "Plan Width"] {
        if let Some(n) = numeric(v, key, w) {
            node.estimates.insert(key.into(), n);
        }
    }
    for key in [
        "Actual Startup Time",
        "Actual Total Time",
        "Actual Rows",
        "Actual Loops",
        "Rows Removed by Filter",
        "Rows Removed by Join Filter",
        "Heap Fetches",
    ] {
        if let Some(n) = numeric(v, key, w) {
            node.actual.insert(key.into(), n);
        }
    }
    for key in [
        "Filter",
        "Index Cond",
        "Recheck Cond",
        "Join Filter",
        "Hash Cond",
        "Merge Cond",
    ] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            node.predicates.insert(key.into(), s.into());
        }
    }
    if let Some(children) = v.get("Plans") {
        if let Some(children) = children.as_array() {
            for child in children {
                if child.is_object() {
                    node.children.push(pg_node(child, w));
                } else {
                    w.push("Non-object PostgreSQL child plan retained in raw".into());
                }
            }
        } else {
            w.push("PostgreSQL Plans is not an array".into());
        }
    }
    node
}
fn mysql_node(operation: &str, v: &Value, w: &mut Vec<String>) -> PlanNode {
    let mut node = base(operation.into(), v);
    node.relation = v
        .get("table_name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    node.access = v
        .get("access_type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    for key in [
        "rows_examined_per_scan",
        "rows_produced_per_join",
        "filtered",
    ] {
        if let Some(n) = numeric(v, key, w) {
            node.estimates.insert(key.into(), n);
        }
    }
    if let Some(cost) = v.get("cost_info") {
        if cost.is_object() {
            for key in [
                "query_cost",
                "read_cost",
                "eval_cost",
                "prefix_cost",
                "sort_cost",
            ] {
                if let Some(n) = numeric(cost, key, w) {
                    node.estimates.insert(key.into(), n);
                }
            }
        } else {
            w.push("MySQL cost_info is not an object".into());
        }
    }
    for key in ["attached_condition", "index_condition", "having_condition"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            node.predicates.insert(key.into(), s.into());
        }
    }
    // These are actual tree containers in classic FORMAT=JSON, not arbitrary
    // objects such as cost_info/used_columns. Unknown containers are retained.
    const CHILDREN: &[&str] = &[
        "query_block",
        "table",
        "nested_loop",
        "insert_from",
        "ordering_operation",
        "grouping_operation",
        "duplicates_removal",
        "windowing",
        "buffer_result",
        "union_result",
        "query_specifications",
        "materialized_from_subquery",
        "attached_subqueries",
        "optimized_away_subqueries",
        "select_list_subqueries",
    ];
    if let Some(map) = v.as_object() {
        for (key, value) in map {
            if CHILDREN.contains(&key.as_str()) {
                match value {
                    Value::Object(_) => node.children.push(mysql_node(key, value, w)),
                    Value::Array(items) => {
                        for item in items {
                            if item.is_object() {
                                node.children.push(mysql_node(key, item, w));
                            } else {
                                w.push(format!("Non-object MySQL {key} child retained in raw"));
                            }
                        }
                    }
                    _ => w.push(format!("Unsupported MySQL {key} container")),
                }
            } else if (value.is_object() || value.is_array())
                && !matches!(
                    key.as_str(),
                    "cost_info"
                        | "used_columns"
                        | "possible_keys"
                        | "used_key_parts"
                        | "ref"
                        | "partitions"
                )
            {
                w.push(format!("Unnormalized MySQL field {key} retained in raw"));
            }
        }
    }
    node
}

/// MySQL 8.4 TREE uses indentation rather than explicit child containers. Keep
/// an ancestor stack (not a guessed fixed indentation width), closing siblings
/// before attaching their following nodes. Numeric annotations are parsed only
/// at the end of an arrow line, never out of predicates or quoted identifiers.
fn mysql_tree(raw: &str) -> Result<ExplainReport> {
    ensure!(
        raw.len() <= 1024 * 1024,
        "MySQL profile exceeds 1 MiB decoder limit"
    );
    let mut report = ExplainReport {
        engine: Engine::MySql,
        mode: ReportMode::Analyze,
        raw_format: RawFormat::Tree,
        raw_report: raw.into(),
        raw_json: String::new(),
        timing: BTreeMap::new(),
        roots: vec![],
        warnings: vec![],
    };
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut nodes = 0;
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim_start_matches(' ');
        let Some(body) = trimmed.strip_prefix("-> ") else {
            report.warnings.push(format!(
                "Unnormalized MySQL TREE line {}; retained raw report",
                index + 1
            ));
            continue;
        };
        nodes += 1;
        ensure!(nodes <= 4096, "MySQL profile exceeds 4096 nodes");
        let indent = line.len() - trimmed.len();
        while stack
            .last()
            .is_some_and(|(parent_indent, _)| *parent_indent >= indent)
        {
            attach_tree_node(&mut stack, &mut report.roots);
        }
        ensure!(stack.len() < 128, "MySQL profile exceeds depth 128");
        stack.push((indent, mysql_tree_node(body, line, &mut report.warnings)));
    }
    while !stack.is_empty() {
        attach_tree_node(&mut stack, &mut report.roots);
    }
    if report.roots.is_empty() {
        report
            .warnings
            .push("No supported MySQL plan tree decoded; use raw report".into());
    }
    Ok(report)
}

fn attach_tree_node(stack: &mut Vec<(usize, PlanNode)>, roots: &mut Vec<PlanNode>) {
    let (_, node) = stack.pop().expect("nonempty ancestor stack");
    if let Some((_, parent)) = stack.last_mut() {
        parent.children.push(node);
    } else {
        roots.push(node);
    }
}

fn mysql_tree_node(body: &str, line: &str, warnings: &mut Vec<String>) -> PlanNode {
    let mut node = base(body.into(), &Value::String(line.into()));
    let mut operation = body.trim_end();
    if let Some(prefix) = operation.strip_suffix(" (never executed)") {
        operation = prefix.trim_end();
        node.predicates
            .insert("Execution".into(), "never executed".into());
    } else if let Some((prefix, metrics)) = tree_suffix(operation, "actual time=") {
        operation = prefix;
        tree_metrics(metrics, true, &mut node.actual, warnings);
    } else {
        warnings.push("MySQL TREE node has no recognized actual metrics; retained raw line".into());
    }
    if let Some((prefix, metrics)) = tree_suffix(operation, "cost=") {
        operation = prefix;
        tree_metrics(metrics, false, &mut node.estimates, warnings);
    }
    node.operation = operation.into();
    node
}

fn tree_suffix<'a>(line: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let line = line.strip_suffix(')')?;
    let (operation, metrics) = line.rsplit_once(" (")?;
    metrics
        .starts_with(prefix)
        .then_some((operation.trim_end(), metrics))
}

fn tree_metrics(
    metrics: &str,
    actual: bool,
    output: &mut BTreeMap<String, ExactNumber>,
    warnings: &mut Vec<String>,
) {
    let metrics = if actual {
        metrics.strip_prefix("actual ").unwrap_or(metrics)
    } else {
        metrics
    };
    for field in metrics.split_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            warnings.push(format!(
                "Unknown MySQL TREE metric {field}; retained raw line"
            ));
            continue;
        };
        let names = match (actual, key) {
            (true, "time") => Some(("Actual Startup Time", "Actual Total Time")),
            (false, "cost") => Some(("Startup Cost", "Total Cost")),
            _ => None,
        };
        if let Some((start, end)) = names {
            if let Some((first, last)) = value.split_once("..") {
                insert_tree_number(output, start, first, warnings);
                insert_tree_number(output, end, last, warnings);
            } else if !actual {
                insert_tree_number(output, end, value, warnings);
            } else {
                warnings.push(format!(
                    "Unknown MySQL actual time range {value}; retained raw line"
                ));
            }
        } else {
            match (actual, key) {
                (true, "rows") => insert_tree_number(output, "Actual Rows", value, warnings),
                (true, "loops") => insert_tree_number(output, "Actual Loops", value, warnings),
                (false, "rows") => insert_tree_number(output, "Plan Rows", value, warnings),
                _ => warnings.push(format!(
                    "Unknown MySQL TREE metric {key}; retained raw line"
                )),
            }
        }
    }
}

fn insert_tree_number(
    output: &mut BTreeMap<String, ExactNumber>,
    key: &str,
    value: &str,
    warnings: &mut Vec<String>,
) {
    // Native TREE uses decimal and engineering/scientific notation (e.g. 50e-6).
    // Validate lexically, never round through f64 or reserialize the number.
    let unsigned = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .unwrap_or(value);
    let mut parts = unsigned.split(['e', 'E']);
    let mantissa = parts.next().unwrap_or("");
    let exponent = parts.next();
    let mut decimal = mantissa.split('.');
    let whole = decimal.next().unwrap_or("");
    let fraction = decimal.next();
    let digits = |s: &str| s.bytes().all(|byte| byte.is_ascii_digit());
    let valid = parts.next().is_none()
        && decimal.next().is_none()
        && digits(whole)
        && fraction.is_none_or(digits)
        && (!whole.is_empty() || fraction.is_some_and(|s| !s.is_empty()))
        && exponent.is_none_or(|s| {
            let s = s.strip_prefix(['+', '-']).unwrap_or(s);
            !s.is_empty() && digits(s)
        });
    if valid {
        output.insert(key.into(), ExactNumber(value.into()));
    } else {
        warnings.push(format!(
            "Invalid MySQL TREE metric {key}={value}; retained raw line"
        ));
    }
}
