//! FIFO persistence workers with explicit shutdown independent of sender lifetimes.
use crate::session::{SessionState, SessionStore};
use rowboat_services::{AppState, Store};
use std::sync::{Arc, mpsc};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

#[derive(Debug)]
pub enum Write<T> {
    Save(u64, T),
    Shutdown,
}
pub enum Event {
    StateLoaded(Result<Box<AppState>, String>),
    RecoveryLoaded(Result<SessionState, String>),
    StateSaved(u64, Result<(), String>),
    RecoverySaved(u64, Result<(), String>),
    WriterStopped,
}
pub struct Writers {
    pub state: UnboundedSender<Write<AppState>>,
    pub recovery: UnboundedSender<Write<SessionState>>,
    pub events: mpsc::Receiver<Event>,
}
// Declared before each worker's receiver/store/lock locals, so its notification
// follows their destruction even on an initialization error or unwind. Captured
// receivers must move into locals: closure captures otherwise outlive this guard.
struct ExitNotice(mpsc::Sender<Event>);
impl Drop for ExitNotice {
    fn drop(&mut self) {
        let _ = self.0.send(Event::WriterStopped);
    }
}
impl Writers {
    pub fn spawn(runtime: &tokio::runtime::Handle) -> Self {
        Self::spawn_with_store(runtime, Store::open)
    }
    fn spawn_with_store(
        runtime: &tokio::runtime::Handle,
        open: impl FnOnce() -> anyhow::Result<Store> + Send + 'static,
    ) -> Self {
        let (events_tx, events) = mpsc::channel();
        let (state, state_rx) = unbounded_channel();
        let (recovery, recovery_rx) = unbounded_channel();
        let (lock_tx, lock_rx) =
            mpsc::channel::<Result<(Store, Option<Arc<SessionStore>>), String>>();
        let tx = events_tx.clone();
        runtime.spawn_blocking(move || {
            let _exit = ExitNotice(tx.clone());
            let mut state_rx = state_rx;
            let (store, recovery_guard) = match lock_rx.recv() {
                Ok(Ok(value)) => value,
                error => {
                    let detail = match error {
                        Ok(Err(e)) => e,
                        _ => "Recovery worker stopped during initialization".into(),
                    };
                    let _ = tx.send(Event::StateLoaded(Err(detail)));
                    return;
                }
            };
            match store.load() {
                Ok(value) => {
                    let _ = tx.send(Event::StateLoaded(Ok(Box::new(value))));
                }
                Err(e) => {
                    let _ = tx.send(Event::StateLoaded(Err(format!("{e:#}"))));
                    return;
                }
            }
            if recovery_guard.is_none() {
                return;
            }
            while let Some(command) = state_rx.blocking_recv() {
                match command {
                    Write::Save(revision, value) => {
                        let _ = tx.send(Event::StateSaved(
                            revision,
                            store.save(&value).map_err(|e| format!("{e:#}")),
                        ));
                    }
                    Write::Shutdown => break,
                }
            }
        });
        runtime.spawn_blocking(move || {
            let _exit = ExitNotice(events_tx.clone());
            let mut recovery_rx = recovery_rx;
            let store = match open() {
                Ok(store) => store,
                Err(e) => {
                    let error = format!("{e:#}");
                    let _ = lock_tx.send(Err(error.clone()));
                    let _ = events_tx.send(Event::RecoveryLoaded(Err(error)));
                    return;
                }
            };
            let recovery_store =
                match SessionStore::at(store.path()).and_then(|guard| Ok((guard.load()?, guard))) {
                    Ok((value, guard)) => {
                        let _ = events_tx.send(Event::RecoveryLoaded(Ok(value)));
                        Arc::new(guard)
                    }
                    Err(e) => {
                        let _ = lock_tx.send(Ok((store, None)));
                        let _ = events_tx.send(Event::RecoveryLoaded(Err(format!("{e:#}"))));
                        return;
                    }
                };
            let _ = lock_tx.send(Ok((store, Some(recovery_store.clone()))));
            while let Some(command) = recovery_rx.blocking_recv() {
                match command {
                    Write::Save(revision, value) => {
                        let _ = events_tx.send(Event::RecoverySaved(
                            revision,
                            recovery_store.save(&value).map_err(|e| format!("{e:#}")),
                        ));
                    }
                    Write::Shutdown => break,
                }
            }
        });
        Self {
            state,
            recovery,
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, Write, Writers};
    use crate::{
        app_runtime::AppRuntime,
        session::{SessionState, SessionStore, TabBuffer, WorkspaceBuffer},
    };
    use rowboat_services::{AppState, Snippet, Store};
    use std::time::Duration;

    fn receive(writers: &Writers) -> Event {
        writers
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("Persistence worker stopped responding")
    }
    fn loaded(writers: &Writers) {
        for _ in 0..2 {
            match receive(writers) {
                Event::StateLoaded(Ok(_)) | Event::RecoveryLoaded(Ok(_)) => {}
                _ => panic!("Unexpected persistence startup event"),
            }
        }
    }
    fn snapshot() -> SessionState {
        SessionState {
            version: 2,
            active_tab: 1,
            tabs: vec![
                TabBuffer {
                    id: 0,
                    connection_id: Some("pg".into()),
                    title: "Left".into(),
                    sql: "SELECT 'pg'".into(),
                },
                TabBuffer {
                    id: 1,
                    connection_id: Some("my".into()),
                    title: "Right".into(),
                    sql: "SELECT 'my'".into(),
                },
            ],
            workspaces: vec![
                WorkspaceBuffer {
                    documents: Default::default(),
                    staged: vec![],
                    connection_id: Some("pg".into()),
                    active_tab: 0,
                    split_tab: None,
                    focused_pane: 0,
                    open: true,
                },
                WorkspaceBuffer {
                    documents: Default::default(),
                    staged: vec![],
                    connection_id: Some("my".into()),
                    active_tab: 1,
                    split_tab: None,
                    focused_pane: 0,
                    open: true,
                },
            ],
        }
    }
    fn stop(writers: &Writers) {
        let _ = writers.state.send(Write::Shutdown);
        let _ = writers.recovery.send(Write::Shutdown);
    }
    fn stopped(writers: &Writers) {
        let mut count = 0;
        while count < 2 {
            match receive(writers) {
                Event::WriterStopped => count += 1,
                Event::StateSaved(_, Ok(())) | Event::RecoverySaved(_, Ok(())) => {}
                _ => panic!("Unexpected shutdown event"),
            }
        }
        assert!(writers.state.is_closed());
        assert!(writers.recovery.is_closed());
    }
    #[test]
    fn save_and_close_flushes_both_workers_and_unlocks_with_sender_clones_still_alive() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path()).unwrap();
        let runtime = AppRuntime::new().unwrap();
        let opened = store.clone();
        let writers = Writers::spawn_with_store(&runtime.handle(), move || Ok(opened));
        loaded(&writers);
        let parked_state_sender = writers.state.clone();
        let parked_recovery_sender = writers.recovery.clone();
        let mut state = AppState::default();
        let mut sql = snapshot();
        for revision in 1..=3 {
            state.snippets.push(Snippet {
                id: revision.to_string(),
                name: "Saved".into(),
                sql: format!("SELECT {revision}"),
            });
            sql.tabs[1].sql = format!("SELECT {revision}");
            writers
                .state
                .send(Write::Save(revision, state.clone()))
                .unwrap();
            writers
                .recovery
                .send(Write::Save(revision, sql.clone()))
                .unwrap();
        }
        // Shutdown is ordered after writes, even before their acknowledgements
        // are read and while UI/workspace sender clones still exist.
        stop(&writers);
        stopped(&writers);
        let reopened = SessionStore::at(dir.path())
            .expect("Both stopped workers must have released session.lock");
        assert_eq!(reopened.load().unwrap(), sql);
        assert_eq!(store.load().unwrap().snippets.len(), 3);
        assert!(parked_state_sender.send(Write::Save(4, state)).is_err());
        assert!(parked_recovery_sender.send(Write::Save(4, sql)).is_err());
        drop(runtime);
    }
    #[test]
    fn closing_one_workspace_keeps_the_shared_writers_alive_for_remaining_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path()).unwrap();
        let runtime = AppRuntime::new().unwrap();
        let opened = store.clone();
        let writers = Writers::spawn_with_store(&runtime.handle(), move || Ok(opened));
        loaded(&writers);
        let mut sql = snapshot();
        sql.workspaces[0].open = false;
        writers.recovery.send(Write::Save(1, sql.clone())).unwrap();
        assert!(matches!(receive(&writers), Event::RecoverySaved(1, Ok(()))));
        assert!(
            SessionStore::at(dir.path()).is_err(),
            "Closing a workspace must retain the application lock"
        );
        sql.tabs[1].sql = "SELECT 'still editing remaining workspace'".into();
        writers.recovery.send(Write::Save(2, sql.clone())).unwrap();
        writers
            .state
            .send(Write::Save(1, AppState::default()))
            .unwrap();
        stop(&writers);
        stopped(&writers);
        assert_eq!(SessionStore::at(dir.path()).unwrap().load().unwrap(), sql);
    }
    #[test]
    fn lock_unavailable_workers_stop_without_waiting_for_ui_senders_or_writing_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path()).unwrap();
        let first = SessionStore::at(dir.path()).unwrap();
        let runtime = AppRuntime::new().unwrap();
        let opened = store.clone();
        let writers = Writers::spawn_with_store(&runtime.handle(), move || Ok(opened));
        let mut exits = 0;
        let mut lock_rejected = false;
        while exits < 2 {
            match receive(&writers) {
                Event::WriterStopped => exits += 1,
                Event::RecoveryLoaded(Err(_)) => lock_rejected = true,
                Event::StateLoaded(Ok(_)) => {}
                _ => panic!("Unexpected failed-lock initialization event"),
            }
        }
        assert!(lock_rejected);
        assert!(writers.state.is_closed());
        assert!(writers.recovery.is_closed());
        assert!(!dir.path().join("state.json").exists());
        stop(&writers);
        drop(first);
        assert!(SessionStore::at(dir.path()).is_ok());
    }
    #[test]
    fn initialization_failure_closes_receivers_before_stop_notices() {
        let runtime = AppRuntime::new().unwrap();
        for _ in 0..64 {
            let writers = Writers::spawn_with_store(&runtime.handle(), || {
                anyhow::bail!("Deliberate store initialization failure")
            });
            let mut exits = 0;
            let mut failures = 0;
            while exits < 2 {
                match receive(&writers) {
                    Event::WriterStopped => exits += 1,
                    Event::StateLoaded(Err(_)) | Event::RecoveryLoaded(Err(_)) => failures += 1,
                    _ => panic!("Unexpected failed-store initialization event"),
                }
            }
            assert_eq!(failures, 2);
            assert!(writers.state.is_closed());
            assert!(writers.recovery.is_closed());
            assert!(
                writers
                    .state
                    .send(Write::Save(1, AppState::default()))
                    .is_err()
            );
            assert!(writers.recovery.send(Write::Save(1, snapshot())).is_err());
        }
    }
}
