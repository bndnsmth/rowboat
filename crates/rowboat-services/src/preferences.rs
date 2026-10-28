use anyhow::{Result, ensure};
use rowboat_core::{Engine, FilterExpr, TableRef};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet, VecDeque};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    /// "system" follows the desktop; other values are portable embedded palette IDs.
    pub theme: String,
    pub editor_font_size: f32,
    pub grid_font_size: f32,
    pub grid_row_height: f32,
    pub page_size: u32,
    pub query_row_limit: usize,
    /// Retained query-result data per execution, independent of the row limit.
    pub query_result_limit_mib: usize,
    pub shortcuts: BTreeMap<String, String>,
    pub ai: crate::ai::AiPreferences,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            editor_font_size: 14.,
            grid_font_size: 12.,
            grid_row_height: 28.,
            page_size: 100,
            query_row_limit: 10_000,
            query_result_limit_mib: 256,
            shortcuts: BTreeMap::new(),
            ai: Default::default(),
        }
    }
}

pub fn default_shortcuts() -> BTreeMap<String, String> {
    [
        ("run_query", "ctrl-enter"),
        ("new_query", "ctrl-t"),
        ("format_query", "ctrl-shift-f"),
        ("quick_open", "ctrl-p"),
        ("new_connection", "ctrl-n"),
        ("copy_cell", "ctrl-shift-c"),
        ("complete_sql", "ctrl-space"),
        ("close_query", "ctrl-w"),
        ("command_palette", "ctrl-shift-p"),
        ("preferences", "ctrl-,"),
    ]
    .into_iter()
    .map(|(a, k)| (a.into(), k.into()))
    .collect()
}

pub fn canonical_key_chord(input: &str) -> Result<String> {
    ensure!(
        input.len() <= 64 && !input.chars().any(char::is_control),
        "Invalid shortcut length/control character"
    );
    let input = input.trim().to_ascii_lowercase();
    let parts: Vec<_> = input.split('-').collect();
    ensure!(
        (2..=5).contains(&parts.len()),
        "Shortcut requires modifiers and one key"
    );
    let key = parts[parts.len() - 1];
    let mut modifiers = HashSet::new();
    for modifier in &parts[..parts.len() - 1] {
        ensure!(
            ["ctrl", "alt", "shift", "super"].contains(modifier) && modifiers.insert(*modifier),
            "Invalid or duplicate shortcut modifier"
        );
    }
    ensure!(
        modifiers.contains("ctrl") || modifiers.contains("alt") || modifiers.contains("super"),
        "Shortcut requires Ctrl, Alt, or Super"
    );
    let function_key = key
        .strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=12).contains(&n) && key == format!("f{n}"));
    ensure!(
        key.len() == 1
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b",./;[]=\\".contains(&b))
            || function_key
            || [
                "enter",
                "space",
                "tab",
                "backspace",
                "delete",
                "home",
                "end",
                "pageup",
                "pagedown",
                "up",
                "down",
                "left",
                "right"
            ]
            .contains(&key),
        "Unsupported shortcut key"
    );
    let mut canonical: Vec<_> = ["ctrl", "alt", "shift", "super"]
        .into_iter()
        .filter(|m| modifiers.contains(m))
        .collect();
    canonical.push(key);
    Ok(canonical.join("-"))
}

impl Preferences {
    pub fn query_result_limit_bytes(&self) -> Result<usize> {
        ensure!(
            (16..=512).contains(&self.query_result_limit_mib),
            "Query result data limit must be 16–512 MiB"
        );
        self.query_result_limit_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| {
                anyhow::anyhow!("Query result data limit exceeds the supported byte size")
            })
    }
    pub fn validate(&self) -> Result<()> {
        // Keep unknown well-formed IDs forward compatible. The GUI resolves an
        // unavailable palette to System without discarding the saved preference.
        ensure!(
            !self.theme.is_empty()
                && self.theme.len() <= 80
                && self
                    .theme
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "Theme must be a portable palette ID"
        );
        ensure!(
            self.editor_font_size.is_finite() && (10. ..=32.).contains(&self.editor_font_size),
            "Editor font size must be 10–32"
        );
        ensure!(
            self.grid_font_size.is_finite() && (10. ..=32.).contains(&self.grid_font_size),
            "Grid font size must be 10–32"
        );
        ensure!(
            self.grid_row_height.is_finite() && (20. ..=64.).contains(&self.grid_row_height),
            "Grid row height must be 20–64"
        );
        ensure!(
            (1..=10_000).contains(&self.page_size),
            "Page size must be 1–10000"
        );
        ensure!(
            (1..=100_000).contains(&self.query_row_limit),
            "Query row limit must be 1–100000"
        );
        self.query_result_limit_bytes()?;
        self.effective_shortcuts()?;
        self.ai.validate()?;
        Ok(())
    }
    pub fn effective_shortcuts(&self) -> Result<BTreeMap<String, String>> {
        let mut effective = default_shortcuts();
        ensure!(
            self.shortcuts.len() <= effective.len(),
            "Too many shortcut overrides"
        );
        for (action, chord) in &self.shortcuts {
            ensure!(effective.contains_key(action), "Unknown shortcut action");
            let canonical = canonical_key_chord(chord)?;
            ensure!(
                &canonical == chord,
                "Shortcut must use canonical lowercase modifier order"
            );
            ensure!(
                !canonical.split('-').any(|part| part == "super")
                    && ![
                        "ctrl-c",
                        "ctrl-v",
                        "ctrl-x",
                        "ctrl-a",
                        "ctrl-z",
                        "ctrl-y",
                        "ctrl-shift-z",
                        "alt-tab",
                        "alt-shift-tab",
                        "alt-f4"
                    ]
                    .contains(&canonical.as_str()),
                "Shortcut is reserved for text editing or the window system"
            );
            effective.insert(action.clone(), canonical);
        }
        let mut used = HashSet::new();
        ensure!(
            effective.values().all(|chord| used.insert(chord)),
            "Shortcut conflicts with another effective action"
        );
        Ok(effective)
    }
    pub fn set_shortcut(&mut self, action: &str, chord: &str) -> Result<()> {
        let mut next = self.clone();
        next.shortcuts
            .insert(action.into(), canonical_key_chord(chord)?);
        next.validate()?;
        *self = next;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedFilter {
    pub id: String,
    pub name: String,
    pub connection_id: String,
    pub table: TableRef,
    pub expression: FilterExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dashboard {
    pub id: String,
    pub name: String,
    pub connection_id: String,
    pub refresh_interval_secs: u32,
    pub columns: u8,
    pub cards: Vec<DashboardCard>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardCard {
    pub id: String,
    pub title: String,
    pub metric: String,
    pub column: u8,
    pub row: u16,
    pub column_span: u8,
}

/// Connection-scoped SQL definition, never a container for sampled values.
/// Execution is explicit and must use the core-owned read-only sampling guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomMetricDefinition {
    pub id: String,
    pub name: String,
    pub connection_id: String,
    pub sql: String,
    #[serde(default)]
    pub enabled: bool,
}

impl CustomMetricDefinition {
    pub fn metric_key(&self) -> String {
        format!("custom:{}", self.id)
    }

    /// Metadata validation only; the sampling guard validates native SQL policy
    /// under exclusive session ownership. This is not execution authorization.
    pub fn validate(&self, engine: Engine) -> Result<()> {
        for value in [&self.id, &self.name, &self.connection_id] {
            label(value)?;
        }
        ensure!(
            !self.sql.trim().is_empty() && self.sql.len() <= 16_384,
            "Custom metric SQL must be nonempty and at most 16384 bytes"
        );
        let statements = rowboat_core::sql::split_statements(engine, &self.sql)?;
        ensure!(
            statements.len() == 1,
            "Custom metric SQL must contain exactly one statement"
        );
        let head = self
            .sql
            .trim_start()
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("");
        ensure!(
            head.eq_ignore_ascii_case("SELECT"),
            "Custom metric SQL must start with SELECT (no leading comments or CTEs)"
        );
        Ok(())
    }
}

pub fn builtin_metrics(engine: Engine) -> &'static [&'static str] {
    match engine.descriptor().tools.metrics {
        rowboat_core::ToolSupport::Supported(rowboat_core::ToolProvider::Postgres) => &[
            "numbackends",
            "xact_commit",
            "xact_rollback",
            "blks_read",
            "blks_hit",
            "tup_returned",
            "tup_fetched",
            "tup_inserted",
            "tup_updated",
            "tup_deleted",
            "conflicts",
            "temp_files",
            "temp_bytes",
            "deadlocks",
            "database_bytes",
        ],
        rowboat_core::ToolSupport::Supported(rowboat_core::ToolProvider::MySql) => &[
            "uptime",
            "questions",
            "queries",
            "connections",
            "threads_connected",
            "threads_running",
            "bytes_received",
            "bytes_sent",
            "slow_queries",
            "aborted_connects",
            "innodb_buffer_pool_read_requests",
            "innodb_buffer_pool_reads",
            "innodb_rows_read",
            "innodb_rows_inserted",
            "innodb_rows_updated",
            "innodb_rows_deleted",
        ],
        rowboat_core::ToolSupport::Supported(rowboat_core::ToolProvider::Sqlite)
        | rowboat_core::ToolSupport::Unsupported(_) => &[],
    }
}

fn label(value: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "Metadata names/IDs must be nonempty, control-free, and at most 256 bytes"
    );
    Ok(())
}

pub fn validate_app_state(state: &crate::AppState) -> Result<()> {
    crate::snippets::validate_templates(&state.snippet_templates)?;
    state.preferences.validate()?;
    ensure!(
        state.saved_filters.len() <= 512
            && state.dashboards.len() <= 128
            && state.custom_metrics.len() <= 512,
        "Saved metadata limit exceeded"
    );
    let profiles: BTreeMap<_, _> = state
        .connections
        .iter()
        .map(|p| (&p.id, p.engine))
        .collect();
    ensure!(
        (state.saved_filters.is_empty()
            && state.dashboards.is_empty()
            && state.custom_metrics.is_empty())
            || profiles.len() == state.connections.len(),
        "Saved metadata requires unambiguous connection IDs"
    );
    let mut ids = HashSet::new();
    let mut custom_metrics = BTreeMap::new();
    let mut custom_counts = BTreeMap::new();
    for metric in &state.custom_metrics {
        let engine = profiles
            .get(&metric.connection_id)
            .ok_or_else(|| anyhow::anyhow!("Custom metric references a missing connection"))?;
        metric.validate(*engine)?;
        ensure!(
            custom_metrics.insert(metric.id.as_str(), metric).is_none(),
            "Duplicate custom metric ID"
        );
        let count = custom_counts.entry(&metric.connection_id).or_insert(0);
        *count += 1;
        ensure!(*count <= 32, "At most 32 custom metrics per connection");
    }
    for saved in &state.saved_filters {
        for s in [
            &saved.id,
            &saved.name,
            &saved.connection_id,
            &saved.table.name,
        ] {
            label(s)?;
        }
        ensure!(
            saved.table.schema.len() <= 256 && !saved.table.schema.chars().any(char::is_control),
            "Invalid saved-filter schema"
        );
        ensure!(
            profiles.contains_key(&saved.connection_id),
            "Saved filter references a missing connection"
        );
        ensure!(ids.insert(&saved.id), "Duplicate saved-filter ID");
        saved.expression.validate()?;
    }
    ids.clear();
    for dashboard in &state.dashboards {
        for s in [&dashboard.id, &dashboard.name, &dashboard.connection_id] {
            label(s)?;
        }
        let engine = profiles
            .get(&dashboard.connection_id)
            .ok_or_else(|| anyhow::anyhow!("Dashboard references a missing connection"))?;
        ensure!(ids.insert(&dashboard.id), "Duplicate dashboard ID");
        ensure!(
            (2..=3600).contains(&dashboard.refresh_interval_secs),
            "Dashboard refresh interval must be 2–3600 seconds"
        );
        ensure!(
            (1..=4).contains(&dashboard.columns) && dashboard.cards.len() <= 32,
            "Dashboard layout/card limit exceeded"
        );
        let mut card_ids = HashSet::new();
        let mut occupied = HashSet::new();
        for card in &dashboard.cards {
            label(&card.id)?;
            label(&card.title)?;
            ensure!(card_ids.insert(&card.id), "Duplicate dashboard card ID");
            if let Some(id) = card.metric.strip_prefix("custom:") {
                let metric = custom_metrics.get(id).ok_or_else(|| {
                    anyhow::anyhow!("Dashboard references a missing custom metric")
                })?;
                ensure!(
                    metric.connection_id == dashboard.connection_id,
                    "Dashboard custom metric belongs to another connection"
                );
            } else {
                ensure!(
                    builtin_metrics(*engine).contains(&card.metric.as_str()),
                    "Unknown metric for dashboard engine"
                );
            }
            ensure!(
                card.row < 64
                    && card.column_span > 0
                    && u16::from(card.column) + u16::from(card.column_span)
                        <= u16::from(dashboard.columns),
                "Dashboard card is outside the layout"
            );
            for column in card.column..card.column + card.column_span {
                ensure!(
                    occupied.insert((card.row, column)),
                    "Dashboard cards overlap"
                );
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricPoint {
    pub timestamp_ms: u64,
    pub value: u64,
}

#[derive(Debug, Clone)]
pub struct MetricBuffer {
    capacity: usize,
    points: VecDeque<MetricPoint>,
}
impl MetricBuffer {
    pub fn new(capacity: usize) -> Result<Self> {
        ensure!(
            (1..=3600).contains(&capacity),
            "Metric buffer capacity must be 1–3600"
        );
        Ok(Self {
            capacity,
            points: VecDeque::new(),
        })
    }
    pub fn push(&mut self, point: MetricPoint) -> Result<()> {
        ensure!(
            self.points
                .back()
                .is_none_or(|last| point.timestamp_ms > last.timestamp_ms),
            "Metric timestamps must increase"
        );
        if self.points.len() == self.capacity {
            self.points.pop_front();
        }
        self.points.push_back(point);
        Ok(())
    }
    pub fn points(&self) -> &VecDeque<MetricPoint> {
        &self.points
    }
}
