use rowboat_core::{ConnectionProfile, Engine};
use rowboat_services::{AppState, Store, validate_app_state};
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt};

fn state() -> AppState {
    serde_json::from_value(json!({
        "connections": [ConnectionProfile { id: "primary".into(), ..Default::default() },
            ConnectionProfile { id: "other".into(), ..Default::default() }],
        "custom_metrics": [{"id":"count", "name":"Exact count", "connection_id":"primary",
            "sql":"SELECT 9007199254740993", "enabled":true}],
        "dashboards": [{"id":"dashboard", "name":"Custom", "connection_id":"primary",
            "refresh_interval_secs":5, "columns":1, "cards":[{"id":"card", "title":"Count",
                "metric":"custom:count", "column":0, "row":0, "column_span":1}]}]
    }))
    .unwrap()
}

#[test]
fn legacy_files_and_missing_enabled_remain_inert_and_roundtrip_without_samples() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_at(directory.path()).unwrap();
    fs::write(directory.path().join("state.json"), "{}").unwrap();
    assert!(store.load().unwrap().custom_metrics.is_empty());
    let mut json = serde_json::to_value(state()).unwrap();
    json["custom_metrics"][0]
        .as_object_mut()
        .unwrap()
        .remove("enabled");
    let original: AppState = serde_json::from_value(json).unwrap();
    assert!(!original.custom_metrics[0].enabled);
    assert_eq!(original.custom_metrics[0].metric_key(), "custom:count");
    store.save(&original).unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded.custom_metrics, original.custom_metrics);
    let path = directory.path().join("state.json");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let persisted: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let definition = persisted["custom_metrics"][0].as_object().unwrap();
    assert_eq!(definition.len(), 5);
    assert!(!definition.contains_key("value") && !definition.contains_key("samples"));
    assert_eq!(
        persisted["dashboards"][0]["cards"][0]["metric"],
        "custom:count"
    );
}

#[test]
fn metadata_failures_preserve_previous_file_and_reject_cross_connection_cards() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_at(directory.path()).unwrap();
    store.save(&state()).unwrap();
    let path = directory.path().join("state.json");
    let before = fs::read(&path).unwrap();
    for mutate in [
        (|s: &mut AppState| s.custom_metrics[0].connection_id = "missing".into())
            as fn(&mut AppState),
        |s| s.custom_metrics[0].connection_id = "other".into(),
        |s| s.custom_metrics[0].id.clear(),
        |s| s.custom_metrics[0].name = "\n".into(),
        |s| s.custom_metrics[0].name = "x".repeat(257),
        |s| s.custom_metrics[0].sql = "x".repeat(16_385),
        |s| s.custom_metrics[0].sql = "SELECT 1; SELECT 2".into(),
        |s| s.custom_metrics[0].sql = "SELECT 1\0".into(),
        |s| s.custom_metrics[0].sql = "DELETE FROM anything".into(),
        |s| s.custom_metrics[0].sql = "SELECT 'unterminated".into(),
        |s| s.custom_metrics.push(s.custom_metrics[0].clone()),
        |s| s.custom_metrics.clear(),
        |s| s.connections[1].id = "primary".into(),
    ] {
        let mut invalid = state();
        mutate(&mut invalid);
        assert!(store.save(&invalid).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn definition_limits_apply_even_to_disabled_and_unreferenced_metrics() {
    let mut state = state();
    state.custom_metrics[0].enabled = false;
    for i in 1..32 {
        let mut definition = state.custom_metrics[0].clone();
        definition.id = format!("metric{i}");
        state.custom_metrics.push(definition);
    }
    validate_app_state(&state).unwrap();
    let mut extra = state.custom_metrics[0].clone();
    extra.id = "extra".into();
    state.custom_metrics.push(extra);
    assert!(validate_app_state(&state).is_err());
    state.custom_metrics.last_mut().unwrap().connection_id = "other".into();
    validate_app_state(&state).unwrap();
    for i in 0..480 {
        let mut extra = state.custom_metrics[0].clone();
        extra.id = format!("over{i}");
        state.custom_metrics.push(extra);
    }
    assert_eq!(state.custom_metrics.len(), 513);
    assert!(validate_app_state(&state).is_err());
}

#[test]
fn invalid_persisted_definitions_are_preserved_and_sample_fields_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_at(directory.path()).unwrap();
    let path = directory.path().join("state.json");
    for field in [
        "sample",
        "samples",
        "value",
        "last_value",
        "sampled_at_unix_ms",
    ] {
        let mut value = serde_json::to_value(state()).unwrap();
        value["custom_metrics"][0][field] = json!(123);
        let original = serde_json::to_vec(&value).unwrap();
        fs::write(&path, &original).unwrap();
        assert!(store.load().is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}

#[test]
fn dialect_metadata_validation_preserves_literal_semicolons_and_unicode() {
    let mut state = state();
    for (engine, sql) in [
        (Engine::Postgres, "SELECT length($tag$λ;UPDATE x$tag$)"),
        (Engine::MySql, "SELECT length('λ;UPDATE x')"),
    ] {
        state.connections[0].engine = engine;
        state.custom_metrics[0].sql = sql.into();
        validate_app_state(&state).unwrap();
    }
}
