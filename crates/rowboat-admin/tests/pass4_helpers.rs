use rowboat_admin::{explain, metrics::*};
use rowboat_core::Engine;
use serde_json::json;

#[test]
fn postgres_plan_numbers_are_exact_and_actuals_stay_separate() {
    let report=explain::decode(Engine::Postgres,r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"t","Total Cost":12345678901234567890.1234567890123456789,"Plan Rows":9007199254740993,"Filter":"id > 0","Actual Rows":2,"Actual Loops":3,"Future":{"precise":99999999999999999999999999999}}}]"#).unwrap();
    let n = &report.roots[0];
    assert_eq!(
        n.estimates["Total Cost"].0,
        "12345678901234567890.1234567890123456789"
    );
    assert_eq!(n.estimates["Plan Rows"].0, "9007199254740993");
    assert_eq!(n.actual["Actual Rows"].0, "2");
    assert!(!n.estimates.contains_key("Actual Rows"));
    assert_eq!(
        n.raw["Future"]["precise"].to_string(),
        "99999999999999999999999999999"
    );
}
#[test]
fn mysql_tree_preserves_wrapper_structure_unknown_fields_and_decimal_costs() {
    let report=explain::decode(Engine::MySql,r#"{"query_block":{"cost_info":{"query_cost":"0.000000000000000000001"},"nested_loop":[{"table":{"table_name":"a","access_type":"ref","rows_examined_per_scan":3,"attached_condition":"a.id > 1"}},{"table":{"table_name":"b","access_type":"ALL","future_access":{"mode":"new"}}}]}}"#).unwrap();
    let root = &report.roots[0];
    assert_eq!(root.estimates["query_cost"].0, "0.000000000000000000001");
    assert_eq!(root.children.len(), 2);
    assert_eq!(root.children[0].children[0].relation.as_deref(), Some("a"));
    assert!(root.actual.is_empty());
    assert!(report.warnings.iter().any(|w| w.contains("future_access")));
}
#[test]
fn unsupported_and_malformed_plans_never_invent_values() {
    assert!(explain::decode(Engine::Postgres, "{not json").is_err());
    for engine in [Engine::Postgres, Engine::MySql] {
        let report = explain::decode(engine, "{\"future_root\":{\"x\":1}}").unwrap();
        assert!(report.roots.is_empty());
        assert!(!report.warnings.is_empty());
        assert!(report.raw_json.contains("future_root"));
    }
    let report=explain::decode(Engine::Postgres,r#"[{"Plan":{"Node Type":"New node","Plan Rows":"unknown","Total Cost":null,"Plans":[false]}}]"#).unwrap();
    assert!(report.roots[0].estimates.is_empty());
    assert_eq!(report.warnings.len(), 3);
}
fn sample(time: u64, counter: u64) -> DashboardSample {
    DashboardSample {
        origin: json!({"server":"local","database":"test"}),
        sample: MetricsSample {
            engine: Engine::Postgres,
            sampled_at_unix_ms: time,
            scope: "test".into(),
            counters_reset_at: Some("epoch".into()),
            metrics: vec![
                Metric {
                    name: "counter".into(),
                    value: counter,
                    unit: "count".into(),
                    kind: MetricKind::Counter,
                },
                Metric {
                    name: "gauge".into(),
                    value: 3,
                    unit: "connections".into(),
                    kind: MetricKind::Gauge,
                },
            ],
            raw: vec![],
        },
    }
}
#[test]
fn dashboard_deltas_are_exact_and_never_treat_gauges_as_rates() {
    let a = sample(1000, 9007199254740993);
    let b = sample(2500, 9007199254740998);
    let d = dashboard_delta(&a, &b);
    assert_eq!(d.elapsed_ms, Some(1500));
    assert_eq!(d.metrics[0].delta, Some(5));
    assert_eq!(d.metrics[0].state, DeltaState::Counter);
    assert_eq!(d.metrics[1].delta, None);
    assert_eq!(d.metrics[1].state, DeltaState::Gauge);
}
#[test]
fn dashboard_reset_identity_clock_and_missing_samples_are_explicit() {
    let a = sample(1000, 10);
    let mut b = sample(2000, 9);
    assert_eq!(dashboard_delta(&a, &b).metrics[0].state, DeltaState::Reset);
    b.sample.metrics[0].value = 30;
    b.sample.counters_reset_at = Some("new epoch".into());
    assert_eq!(dashboard_delta(&a, &b).metrics[0].state, DeltaState::Reset);
    b.sample.counters_reset_at = a.sample.counters_reset_at.clone();
    b.sample.sampled_at_unix_ms = 1000;
    assert_eq!(
        dashboard_delta(&a, &b).metrics[0].state,
        DeltaState::InvalidInterval
    );
    b.sample.sampled_at_unix_ms = 2000;
    b.origin = json!({"server":"other"});
    assert_eq!(
        dashboard_delta(&a, &b).metrics[0].state,
        DeltaState::Incompatible
    );
    b.origin = a.origin.clone();
    b.sample.metrics[0].name = "new_counter".into();
    let d = dashboard_delta(&a, &b);
    assert_eq!(d.metrics[0].state, DeltaState::MissingPrevious);
    assert_eq!(d.missing_current, vec!["counter"]);
}
#[test]
fn mysql_uptime_and_flush_reset_suppress_counter_deltas() {
    let mut a = sample(1000, 10);
    a.sample.engine = Engine::MySql;
    a.sample.metrics.push(Metric {
        name: "uptime_since_flush_status".into(),
        value: 100,
        unit: "seconds".into(),
        kind: MetricKind::Gauge,
    });
    let mut b = a.clone();
    b.sample.sampled_at_unix_ms = 2000;
    b.sample.metrics[0].value = 30;
    b.sample.metrics[2].value = 1;
    assert_eq!(dashboard_delta(&a, &b).metrics[0].state, DeltaState::Reset);
}
