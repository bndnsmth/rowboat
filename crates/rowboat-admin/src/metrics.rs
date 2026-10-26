//! Point-in-time real server counters. Sampling cadence/rate calculation belongs
//! to callers; counters can reset, and a single sample is never a rate.
use anyhow::Result;
use rowboat_core::{Cell, Database, Engine, QueryResult, QuerySession};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricKind {
    Counter,
    Gauge,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub value: u64,
    pub unit: String,
    pub kind: MetricKind,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSample {
    pub engine: Engine,
    pub sampled_at_unix_ms: u64,
    pub scope: String,
    /// PostgreSQL stats_reset timestamp, when visible. MySQL exposes uptime as
    /// a metric so consumers can detect restarts between samples.
    pub counters_reset_at: Option<String>,
    pub metrics: Vec<Metric>,
    pub raw: Vec<QueryResult>,
}
pub fn metrics_sql(engine: Engine) -> Result<&'static str> {
    crate::providers::metrics::sql(engine)
}
pub async fn sample(db: &impl QuerySession) -> Result<MetricsSample> {
    let raw = db.query(metrics_sql(db.engine())?, 1000).await?;
    decode_sample(
        db.engine(),
        raw,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .try_into()?,
    )
}

/// Public decoder supports deterministic consumers/tests and never invents a
/// value for missing/null server counters.
pub fn decode_sample(
    engine: Engine,
    raw: Vec<QueryResult>,
    sampled_at_unix_ms: u64,
) -> Result<MetricsSample> {
    let values = crate::providers::metrics::decode(engine, &raw)?;
    let metrics = values
        .counters
        .into_iter()
        .map(|(name, value)| {
            let kind = if matches!(
                name.as_str(),
                "numbackends"
                    | "database_bytes"
                    | "threads_connected"
                    | "threads_running"
                    | "uptime"
                    | "uptime_since_flush_status"
            ) {
                MetricKind::Gauge
            } else {
                MetricKind::Counter
            };
            let unit = if name.contains("bytes") {
                "bytes"
            } else if name == "uptime" || name == "uptime_since_flush_status" {
                "seconds"
            } else if matches!(
                name.as_str(),
                "numbackends"
                    | "threads_connected"
                    | "threads_running"
                    | "connections"
                    | "aborted_connects"
            ) {
                "connections"
            } else {
                "count"
            };
            Metric {
                name,
                value,
                kind,
                unit: unit.into(),
            }
        })
        .collect();
    Ok(MetricsSample {
        engine,
        sampled_at_unix_ms,
        scope: values.scope.into(),
        counters_reset_at: values.reset,
        metrics,
        raw,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardSample {
    pub origin: serde_json::Value,
    pub sample: MetricsSample,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DeltaState {
    Gauge,
    Counter,
    MissingPrevious,
    Reset,
    Incompatible,
    InvalidInterval,
}
#[derive(Debug, Clone, Serialize)]
pub struct MetricDelta {
    pub name: String,
    pub unit: String,
    pub current: u64,
    pub state: DeltaState,
    /// Exact numerator. Rate per second = delta * 1000 / elapsed_ms; callers
    /// choose presentation precision without losing the source integers.
    pub delta: Option<u64>,
}
#[derive(Debug, Clone, Serialize)]
pub struct DashboardDelta {
    pub elapsed_ms: Option<u64>,
    pub metrics: Vec<MetricDelta>,
    pub missing_current: Vec<String>,
    pub warnings: Vec<String>,
}
pub async fn sample_dashboard(db: &impl QuerySession) -> Result<DashboardSample> {
    crate::providers::operation_support(db.engine(), crate::providers::Operation::ServerMetrics)?;
    Ok(DashboardSample {
        origin: crate::object_editor::connection_origin(db).await?,
        sample: sample(db).await?,
    })
}
pub fn dashboard_delta(previous: &DashboardSample, current: &DashboardSample) -> DashboardDelta {
    let a = &previous.sample;
    let b = &current.sample;
    let elapsed = b
        .sampled_at_unix_ms
        .checked_sub(a.sampled_at_unix_ms)
        .filter(|n| *n > 0);
    let compatible =
        previous.origin == current.origin && a.engine == b.engine && a.scope == b.scope;
    let old: BTreeMap<_, _> = a.metrics.iter().map(|m| (m.name.as_str(), m)).collect();
    let new: BTreeMap<_, _> = b.metrics.iter().map(|m| (m.name.as_str(), m)).collect();
    let reset = a.counters_reset_at != b.counters_reset_at
        || ["uptime", "uptime_since_flush_status"].iter().any(|name| {
            old.get(name)
                .zip(new.get(name))
                .is_some_and(|(a, b)| b.value < a.value)
        });
    let mut warnings = vec![];
    if !compatible {
        warnings.push(
            "Connection identity, engine or metric scope changed; counter deltas suppressed".into(),
        );
    }
    if reset {
        warnings.push("Server statistics reset/restart detected; counter deltas suppressed".into());
    }
    if elapsed.is_none() {
        warnings
            .push("Non-increasing wall-clock sample timestamps; counter deltas suppressed".into());
    }
    let metrics = b
        .metrics
        .iter()
        .map(|m| {
            let (state, delta) = if !compatible {
                (DeltaState::Incompatible, None)
            } else if m.kind == MetricKind::Gauge {
                (DeltaState::Gauge, None)
            } else if elapsed.is_none() {
                (DeltaState::InvalidInterval, None)
            } else if reset {
                (DeltaState::Reset, None)
            } else {
                match old.get(m.name.as_str()) {
                    None => (DeltaState::MissingPrevious, None),
                    Some(p) if p.kind != m.kind || p.unit != m.unit => {
                        (DeltaState::Incompatible, None)
                    }
                    Some(p) => match m.value.checked_sub(p.value) {
                        Some(d) => (DeltaState::Counter, Some(d)),
                        None => (DeltaState::Reset, None),
                    },
                }
            };
            MetricDelta {
                name: m.name.clone(),
                unit: m.unit.clone(),
                current: m.value,
                state,
                delta,
            }
        })
        .collect();
    DashboardDelta {
        elapsed_ms: elapsed,
        metrics,
        missing_current: old
            .keys()
            .filter(|name| !new.contains_key(**name))
            .map(|s| (*s).to_owned())
            .collect(),
        warnings,
    }
}

/// A custom scalar is exact decimal text, never a lossy floating-point value.
/// Samples are deliberately separate from persisted application definitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomMetricState {
    Value { decimal: String },
    Empty,
    Invalid { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomMetricSample {
    pub sampled_at_unix_ms: u64,
    pub state: CustomMetricState,
}

/// Explicit single attempt, on the existing physical session. Core owns busy/
/// transaction checks, native SQL validation, server READ ONLY, bounded execution
/// and cancellation-safe rollback. Never call this from automatic refresh/replay.
/// A SELECT function is not a sandbox for external or session-level side effects.
pub async fn sample_custom(db: &Database, sql: &str) -> Result<CustomMetricSample> {
    crate::providers::operation_support(db.engine(), crate::providers::Operation::ServerMetrics)?;
    let raw = db.sample_readonly(sql).await?;
    Ok(decode_custom_sample(
        &raw,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .try_into()?,
    ))
}

/// Empty and invalid results never masquerade as a fresh zero. A numeric SQL
/// column or numeric text expression is accepted, preserving its source digits.
pub fn decode_custom_sample(raw: &[QueryResult], sampled_at_unix_ms: u64) -> CustomMetricSample {
    let invalid = |reason: &str| CustomMetricState::Invalid {
        reason: reason.into(),
    };
    let state = if raw.len() != 1 {
        invalid("Custom metric requires exactly one result set")
    } else {
        let result = &raw[0];
        if result.truncated {
            invalid("Custom metric result exceeded sampling limits")
        } else if result.columns.len() != 1 {
            invalid("Custom metric requires exactly one column")
        } else if result.rows.is_empty() {
            CustomMetricState::Empty
        } else if result.rows.len() != 1 || result.rows[0].len() != 1 {
            invalid("Custom metric requires exactly one scalar row")
        } else {
            match &result.rows[0][0] {
                Cell::Null => invalid("Custom metric returned NULL"),
                Cell::Binary(_) => invalid("Custom metric returned binary data"),
                Cell::Text(value) if finite_decimal(value) => CustomMetricState::Value {
                    decimal: value.clone(),
                },
                Cell::Text(_) => invalid(
                    "Custom metric requires a finite decimal (at most 1024 bytes, exponent magnitude at most 100000)",
                ),
            }
        }
    };
    CustomMetricSample {
        sampled_at_unix_ms,
        state,
    }
}

fn finite_decimal(value: &str) -> bool {
    // Never parse through f64: large precise integers/decimals and tiny nonzero
    // exponents must not become rounded numbers, infinity, or zero.
    if value.is_empty() || value.len() > 1024 {
        return false;
    }
    let bytes = value.as_bytes();
    let mut i = usize::from(matches!(bytes[0], b'+' | b'-'));
    let mut digits = 0;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
        digits += 1;
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return false;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        let mut exponent = 0u32;
        while let Some(digit) = bytes.get(i).filter(|b| b.is_ascii_digit()) {
            exponent = exponent * 10 + u32::from(*digit - b'0');
            if exponent > 100_000 {
                return false;
            }
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == bytes.len()
}
