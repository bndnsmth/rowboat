use rowboat_core::{Cell, ConnectionProfile, Filter, FilterExpr, FilterOp, TableRef};
use rowboat_services::*;

fn state() -> AppState {
    let profile = ConnectionProfile::default();
    AppState {
        saved_filters: vec![SavedFilter {
            id: "filter".into(),
            name: "Pending".into(),
            connection_id: profile.id.clone(),
            table: TableRef {
                schema: "public".into(),
                name: "items".into(),
            },
            expression: FilterExpr::Or(vec![FilterExpr::Leaf(Filter {
                column: "state".into(),
                op: FilterOp::Eq,
                value: Cell::Text("pending".into()),
            })]),
        }],
        dashboards: vec![Dashboard {
            id: "dashboard".into(),
            name: "Local health".into(),
            connection_id: profile.id.clone(),
            refresh_interval_secs: 5,
            columns: 2,
            cards: vec![DashboardCard {
                id: "connections".into(),
                title: "Connections".into(),
                metric: "numbackends".into(),
                column: 0,
                row: 0,
                column_span: 1,
            }],
        }],
        connections: vec![profile],
        ..Default::default()
    }
}

#[test]
fn preferences_and_owned_metadata_roundtrip_without_sample_persistence() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_at(directory.path()).unwrap();
    let mut original = state();
    original.preferences.editor_font_size = 18.;
    original.preferences.theme = "nord".into();
    original.preferences.grid_font_size = 16.;
    original.preferences.query_result_limit_mib = 384;
    original
        .preferences
        .set_shortcut("run_query", "ALT-CTRL-R")
        .unwrap();
    store.save(&original).unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded.preferences.editor_font_size, 18.);
    assert_eq!(loaded.preferences.theme, "nord");
    assert_eq!(loaded.preferences.grid_font_size, 16.);
    assert_eq!(loaded.preferences.query_result_limit_mib, 384);
    assert_eq!(loaded.preferences.query_row_limit, 10_000);
    assert_eq!(loaded.preferences.shortcuts["run_query"], "ctrl-alt-r");
    assert_eq!(loaded.saved_filters[0].table.name, "items");
    assert_eq!(loaded.dashboards[0].cards[0].metric, "numbackends");
    assert!(
        !std::fs::read_to_string(directory.path().join("state.json"))
            .unwrap()
            .contains("samples")
    );
    std::fs::write(directory.path().join("state.json"), "{}").unwrap();
    let legacy = store.load().unwrap();
    assert_eq!(legacy.preferences.page_size, 100);
    assert_eq!(legacy.preferences.theme, "system");
    assert_eq!(legacy.preferences.grid_font_size, 12.);
    assert_eq!(legacy.preferences.query_result_limit_mib, 256);
    assert!(legacy.dashboards.is_empty() && legacy.saved_filters.is_empty());
    std::fs::write(directory.path().join("state.json"), r#"{"preferences":{"editor_font_size":18,"grid_row_height":36,"page_size":250,"query_row_limit":5000,"shortcuts":{}}}"#).unwrap();
    let legacy = store.load().unwrap();
    assert_eq!(legacy.preferences.grid_font_size, 12.);
    assert_eq!(legacy.preferences.editor_font_size, 18.);
    assert_eq!(legacy.preferences.grid_row_height, 36.);
    assert_eq!(legacy.preferences.page_size, 250);
    assert_eq!(legacy.preferences.theme, "system");
    assert_eq!(legacy.preferences.query_row_limit, 5000);
    assert_eq!(legacy.preferences.query_result_limit_mib, 256);
}

#[test]
fn theme_ids_are_portable_forward_compatible_and_invalid_values_do_not_overwrite_state() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_at(directory.path()).unwrap();
    let mut state = AppState::default();
    state.preferences.theme = "future-palette-42".into();
    store.save(&state).unwrap();
    assert_eq!(store.load().unwrap().preferences.theme, "future-palette-42");
    for invalid in ["", "../theme", "Nord", "white\n", "file:///theme"] {
        state.preferences.theme = invalid.into();
        assert!(store.save(&state).is_err());
        assert_eq!(store.load().unwrap().preferences.theme, "future-palette-42");
    }
}

#[test]
fn query_result_budget_bounds_conversion_and_store_validation() {
    let mut state = AppState::default();
    assert_eq!(state.preferences.query_result_limit_mib, 256);
    assert_eq!(state.preferences.query_row_limit, 10_000);
    assert_eq!(
        state.preferences.query_result_limit_bytes().unwrap(),
        268_435_456
    );
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_at(directory.path()).unwrap();
    let path = directory.path().join("state.json");
    for mib in [16, 64, 256, 512] {
        state.preferences.query_result_limit_mib = mib;
        state.preferences.validate().unwrap();
        assert_eq!(
            state.preferences.query_result_limit_bytes().unwrap(),
            mib * 1_048_576
        );
        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.preferences.query_result_limit_mib, mib);
        assert_eq!(loaded.preferences.query_row_limit, 10_000);
    }
    let before = std::fs::read(&path).unwrap();
    for mib in [0, 15, 513, usize::MAX] {
        state.preferences.query_result_limit_mib = mib;
        assert!(state.preferences.query_result_limit_bytes().is_err());
        assert!(state.preferences.validate().is_err());
        assert!(store.save(&state).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    for value in ["0", "15", "513", "-1", "256.5", "null", "\"256\""] {
        let malformed = format!(r#"{{"preferences":{{"query_result_limit_mib":{value}}}}}"#);
        std::fs::write(&path, &malformed).unwrap();
        assert!(store.load().is_err(), "accepted {value}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), malformed);
    }
}

#[test]
fn invalid_preferences_and_conflicts_are_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_at(directory.path()).unwrap();
    store.save(&state()).unwrap();
    let path = directory.path().join("state.json");
    let before = std::fs::read(&path).unwrap();
    for mutate in [
        (|s: &mut AppState| s.preferences.editor_font_size = f32::NAN) as fn(&mut AppState),
        |s| s.preferences.editor_font_size = 40.,
        |s| s.preferences.grid_font_size = 9.99,
        |s| s.preferences.grid_font_size = 32.01,
        |s| s.preferences.grid_font_size = f32::NAN,
        |s| s.preferences.grid_font_size = f32::INFINITY,
        |s| s.preferences.grid_font_size = f32::NEG_INFINITY,
        |s| s.preferences.grid_row_height = 0.,
        |s| s.preferences.page_size = 0,
        |s| s.preferences.query_row_limit = 100_001,
        |s| {
            s.preferences
                .shortcuts
                .insert("run_query".into(), "ctrl-c".into());
        },
        |s| {
            s.preferences
                .shortcuts
                .insert("run_query".into(), "ctrl-p".into());
        },
        |s| {
            s.preferences
                .shortcuts
                .insert("unknown".into(), "ctrl-r".into());
        },
        |s| {
            s.preferences
                .shortcuts
                .insert("run_query".into(), "CTRL-R".into());
        },
    ] {
        let mut invalid = state();
        mutate(&mut invalid);
        assert!(store.save(&invalid).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    let mut preferences = Preferences::default();
    for size in [10., 12., 32.] {
        preferences.grid_font_size = size;
        preferences.validate().unwrap();
    }
    assert!(preferences.set_shortcut("run_query", "ctrl-p").is_err());
    assert!(preferences.shortcuts.is_empty());
    for chord in [
        "escape",
        "r",
        "shift-r",
        "ctrl-ctrl-r",
        "ctrl-r ctrl-s",
        "super-r",
        "alt-tab",
        "ctrl-v",
    ] {
        assert!(
            preferences.set_shortcut("run_query", chord).is_err(),
            "accepted {chord}"
        );
    }
}

#[test]
fn rejects_orphaned_invalid_filters_and_metric_layouts() {
    for mutate in [
        (|s: &mut AppState| s.saved_filters[0].connection_id = "other".into()) as fn(&mut AppState),
        |s| {
            s.saved_filters.push(s.saved_filters[0].clone());
        },
        |s| {
            s.saved_filters[0].expression = FilterExpr::Between {
                column: "x".into(),
                low: Cell::Null,
                high: Cell::Text("5".into()),
                negated: false,
            }
        },
        |s| s.dashboards[0].refresh_interval_secs = 0,
        |s| s.dashboards[0].cards[0].metric = "SELECT side_effect()".into(),
        |s| s.dashboards[0].cards[0].metric = "threads_connected".into(),
        |s| s.dashboards[0].cards[0].column_span = 255,
        |s| {
            let mut card = s.dashboards[0].cards[0].clone();
            card.id = "other".into();
            s.dashboards[0].cards.push(card);
        },
    ] {
        let mut invalid = state();
        mutate(&mut invalid);
        assert!(validate_app_state(&invalid).is_err());
    }
    let malformed = r#"{"preferences":{"page_size":0}}"#;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_at(directory.path()).unwrap();
    let path = directory.path().join("state.json");
    std::fs::write(&path, malformed).unwrap();
    assert!(store.load().is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), malformed);
}

#[test]
fn metric_history_is_bounded_and_not_silently_reordered() {
    let mut samples = MetricBuffer::new(3).unwrap();
    for timestamp_ms in 1..=10 {
        samples
            .push(MetricPoint {
                timestamp_ms,
                value: timestamp_ms * 2,
            })
            .unwrap();
    }
    assert_eq!(samples.points().len(), 3);
    assert_eq!(samples.points().front().unwrap().timestamp_ms, 8);
    assert!(
        samples
            .push(MetricPoint {
                timestamp_ms: 9,
                value: 999
            })
            .is_err()
    );
    assert_eq!(samples.points().back().unwrap().value, 20);
    assert!(MetricBuffer::new(3601).is_err());
}
