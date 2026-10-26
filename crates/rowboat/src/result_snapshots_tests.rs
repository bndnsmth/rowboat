use super::{MAX_PINS, ResultOrigin, ResultsState, ResultsView};
use crate::workspace_model::Route;
use rowboat_core::{
    Cell, Column, QueryResult,
    result_compare::{ColumnPair, Mapping},
};

fn route() -> Route {
    Route {
        workspace: 19,
        generation: 2,
    }
}
fn origin() -> ResultOrigin {
    ResultOrigin {
        route: route(),
        connection_id: "test-id".into(),
        connection_name: "test".into(),
        database: "db".into(),
        engine: rowboat_core::Engine::Sqlite,
        started_at: "2026-10-26T00:00:00Z".into(),
        source: "SELECT id, value FROM test".into(),
        table_page: false,
    }
}
fn result() -> QueryResult {
    QueryResult {
        columns: ["id", "value"]
            .iter()
            .map(|name| Column {
                name: (*name).into(),
                data_type: "text".into(),
                nullable: true,
                primary_key: *name == "id",
                default_value: None,
            })
            .collect(),
        rows: vec![vec![
            Cell::Text("1".into()),
            Cell::Text("9007199254740993.00000000001".into()),
        ]],
        ..Default::default()
    }
}
fn pin(state: &mut ResultsState) -> u64 {
    state
        .pin_result(&result(), origin(), 0, "Complete".into(), false, usize::MAX)
        .unwrap()
}
fn ready() -> ResultsState {
    let mut state = ResultsState::default();
    pin(&mut state);
    pin(&mut state);
    state.mapping = Mapping {
        keys: vec![ColumnPair { left: 0, right: 0 }],
        columns: vec![ColumnPair { left: 1, right: 1 }],
    };
    state
}
fn finish(state: &mut ResultsState, route: Route) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while state.pending() {
        state.poll(route);
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
}

#[test]
fn immutable_pin_has_exact_raw_values_and_stable_named_identity() {
    let mut state = ResultsState::default();
    let mut live = result();
    let id = state
        .pin_result(&live, origin(), 3, "Interrupted".into(), true, usize::MAX)
        .unwrap();
    live.rows[0][1] = Cell::Null;
    state.rename_pin(id, " Before update ").unwrap();
    let pin = state.pin(id).unwrap();
    assert_eq!(pin.name, "Before update");
    assert_eq!(
        pin.snapshot.result.rows[0][1],
        Cell::Text("9007199254740993.00000000001".into())
    );
    assert!(pin.snapshot.partial);
    assert_eq!(pin.snapshot.result_index, 3);
    assert_eq!(pin.snapshot.origin.source, "SELECT id, value FROM test");
    assert!(state.rename_pin(id, "").is_err());
    assert!(state.rename_pin(id, &"a".repeat(129)).is_err());
}
#[test]
fn pin_admission_bound_and_close_release_storage() {
    let mut state = ResultsState::default();
    assert!(
        state
            .pin_result(&result(), origin(), 0, "Complete".into(), false, 1)
            .is_err()
    );
    assert!(state.pins.is_empty());
    for _ in 0..MAX_PINS {
        pin(&mut state);
    }
    assert!(
        state
            .pin_result(&result(), origin(), 0, "Complete".into(), false, usize::MAX)
            .is_err()
    );
    let retained = state.retained_bytes();
    let id = state.pins[0].snapshot.id;
    state.close_pin(id);
    assert!(state.retained_bytes() < retained);
    assert!(state.pin(id).is_none());
}
#[test]
fn cpu_job_delivers_typed_result_and_rejects_stale_workspace_generation() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut state = ready();
    state.start(route(), usize::MAX, runtime.handle()).unwrap();
    finish(&mut state, route());
    assert_eq!(state.compared.as_ref().unwrap().diff.unchanged, 1);
    let compared_bytes = state.retained_bytes();
    state.close_comparison();
    assert!(state.compared.is_none());
    assert_eq!(state.pins.len(), 2);
    assert!(state.retained_bytes() < compared_bytes);
    let mut stale = ready();
    stale.start(route(), usize::MAX, runtime.handle()).unwrap();
    finish(
        &mut stale,
        Route {
            generation: 3,
            ..route()
        },
    );
    assert!(stale.compared.is_none());
    assert!(stale.error.is_none());
}
#[test]
fn cancelled_worker_keeps_reservation_and_closed_input_until_acknowledged() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut state = ready();
    state.start(route(), usize::MAX, runtime.handle()).unwrap();
    let before = state.retained_bytes();
    let ids: Vec<_> = state.pins.iter().map(|p| p.snapshot.id).collect();
    for id in ids {
        state.close_pin(id);
    }
    assert!(state.pending());
    assert!(state.retained_bytes() >= before - 256);
    assert!(state.start(route(), usize::MAX, runtime.handle()).is_err());
    finish(&mut state, route());
    assert!(state.compared.is_none());
    assert!(state.retained_bytes() < before);
    assert_eq!(state.view, ResultsView::Live);
}
#[test]
fn reset_invalidates_request_without_reusing_worker_memory_early() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut state = ready();
    state.start(route(), usize::MAX, runtime.handle()).unwrap();
    state.reset();
    assert!(state.pins.is_empty());
    assert!(state.pending());
    assert!(state.retained_bytes() > 0);
    finish(&mut state, route());
    assert!(state.compared.is_none());
    assert!(state.retained_bytes() < 1024);
}
#[test]
fn comparison_budget_rejection_does_not_start_cpu_job() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut state = ready();
    assert!(state.start(route(), 0, runtime.handle()).is_err());
    assert!(!state.pending());
}
