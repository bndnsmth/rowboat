//! Workspace-local, memory-only pins. Integration contract:
//! - `Workbench.results: ResultsState`, initialized with Default.
//! - capture query/table origins at launch; forget_query on tab close.
//! - poll_result_comparison in the existing UI poll; reset on connection change.
//! - available_results_bytes(Some(replaced_tab)) before streaming; additionally
//!   result_frame_fits at GUI ingress (core byte_limit excludes schema/capacity).
//! - render_results_tools above results; render_results_comparison replaces the
//!   ordinary grid when Some. No ordinary grid edit/copy actions target pins.
use crate::workspace_model::Route;
use rowboat_core::{
    QueryResult,
    result_compare::{Comparison, Mapping, comparison_bytes, result_bytes},
};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

pub const MAX_PINS: usize = 8;
/// Bound the synchronous immutable copy from the existing non-Arc live model.
pub const MAX_PIN_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_NAME_BYTES: usize = 128;

#[derive(Clone, Debug)]
pub struct ResultOrigin {
    pub route: Route,
    pub connection_id: String,
    pub connection_name: String,
    pub database: String,
    pub engine: rowboat_core::Engine,
    pub started_at: String,
    /// Executed SQL for query results; qualified table/page/filter descriptor for
    /// table results (generated SQL is not exposed by the browse adapter).
    pub source: String,
    pub table_page: bool,
}
impl ResultOrigin {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.connection_id.capacity()
            + self.connection_name.capacity()
            + self.database.capacity()
            + self.started_at.capacity()
            + self.source.capacity()
    }
}

#[derive(Debug)]
pub struct Snapshot {
    pub id: u64,
    pub result: QueryResult,
    pub origin: ResultOrigin,
    pub result_index: usize,
    pub captured_at: String,
    pub completeness: String,
    pub partial: bool,
    pub bytes: usize,
}
pub struct Pin {
    pub name: String,
    pub snapshot: Arc<Snapshot>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResultsView {
    #[default]
    Live,
    Pin(u64),
    Comparison,
}
pub struct Compared {
    pub left: Arc<Snapshot>,
    pub right: Arc<Snapshot>,
    pub mapping: Mapping,
    pub diff: Comparison,
    pub bytes: usize,
}
pub struct CompareWorker {
    request: u64,
    route: Route,
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Result<Comparison, String>>,
    left: Arc<Snapshot>,
    right: Arc<Snapshot>,
    mapping: Mapping,
    bytes: usize,
}
impl Drop for CompareWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
#[derive(Default)]
pub struct ResultsState {
    pub viewport_scroll: gpui_kit::ScrollHandle,
    pub horizontal_scroll: gpui_kit::ScrollHandle,
    pub pins: Vec<Pin>,
    pub view: ResultsView,
    pub selected_row: usize,
    pub selected_column: usize,
    pub left: Option<u64>,
    pub right: Option<u64>,
    pub mapping: Mapping,
    pub compared: Option<Compared>,
    pub error: Option<String>,
    pub show_mapping: bool,
    pub show_source: bool,
    pub rename: Option<(u64, gpui_kit::Entity<gpui_kit::base::input::InputState>)>,
    pub query_origins: HashMap<usize, ResultOrigin>,
    pub table_origin: Option<ResultOrigin>,
    /// Conservative, launch-scoped GUI ingress allowance. Charged incrementally
    /// so streaming never rescans every previously retained cell per batch.
    pub stream_remaining: Option<(usize, usize)>,
    next_id: u64,
    request: u64,
    worker: Option<CompareWorker>,
}
impl ResultsState {
    pub fn pin(&self, id: u64) -> Option<&Pin> {
        self.pins.iter().find(|p| p.snapshot.id == id)
    }
    pub fn selected_snapshot(&self) -> Option<&Snapshot> {
        match self.view {
            ResultsView::Pin(id) => self.pin(id).map(|p| p.snapshot.as_ref()),
            _ => None,
        }
    }
    pub fn pending(&self) -> bool {
        self.worker.is_some()
    }
    pub fn forget_query(&mut self, tab: usize) {
        self.query_origins.remove(&tab);
    }
    /// Includes orphaned input Arcs until a cancelled worker acknowledges exit.
    /// Closing pins must not permit memory to be reused while that CPU owns them.
    pub fn retained_bytes(&self) -> usize {
        let mut ids = Vec::with_capacity(MAX_PINS + 4);
        let mut bytes = 0usize;
        let mut add = |s: &Snapshot| {
            if !ids.contains(&s.id) {
                ids.push(s.id);
                bytes = bytes.saturating_add(s.bytes);
            }
        };
        for p in &self.pins {
            add(&p.snapshot);
        }
        if let Some(w) = &self.worker {
            add(&w.left);
            add(&w.right);
        }
        if let Some(c) = &self.compared {
            add(&c.left);
            add(&c.right);
        }
        bytes = bytes
            .saturating_add(self.worker.as_ref().map_or(0, |w| w.bytes))
            .saturating_add(self.compared.as_ref().map_or(0, |c| c.bytes));
        for origin in self.query_origins.values().chain(self.table_origin.iter()) {
            bytes = bytes.saturating_add(origin.bytes());
        }
        bytes
            .saturating_add(self.pins.capacity() * std::mem::size_of::<Pin>())
            .saturating_add(
                self.query_origins.capacity() * (std::mem::size_of::<(usize, ResultOrigin)>() + 16),
            )
            .saturating_add(self.pins.iter().map(|p| p.name.capacity()).sum::<usize>())
            .saturating_add(
                (self.mapping.columns.capacity() + self.mapping.keys.capacity())
                    * std::mem::size_of::<rowboat_core::result_compare::ColumnPair>(),
            )
    }
    pub fn pin_result(
        &mut self,
        result: &QueryResult,
        origin: ResultOrigin,
        index: usize,
        completeness: String,
        partial: bool,
        available: usize,
    ) -> Result<u64, String> {
        if self.pins.len() >= MAX_PINS {
            return Err(format!(
                "At most {MAX_PINS} pins per workspace; close a pin first"
            ));
        }
        let bytes = result_bytes(result)
            .saturating_add(origin.bytes())
            .saturating_add(completeness.len())
            .saturating_add(std::mem::size_of::<Snapshot>() + MAX_NAME_BYTES + 256);
        if bytes > MAX_PIN_BYTES {
            return Err("This result exceeds the 32 MiB pin-copy bound; retain fewer rows or smaller columns".into());
        }
        if bytes > available {
            return Err(format!(
                "Pin needs {bytes} bytes; {available} bytes available in the shared result budget"
            ));
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let partial = partial || origin.table_page;
        self.pins.push(Pin {
            name: format!("Snapshot {id}"),
            snapshot: Arc::new(Snapshot {
                id,
                result: result.clone(),
                origin,
                result_index: index,
                captured_at: chrono::Utc::now().to_rfc3339(),
                completeness,
                partial,
                bytes,
            }),
        });
        if self.left.is_none() {
            self.left = Some(id);
        } else if self.right.is_none() {
            self.right = Some(id);
        }
        self.view = ResultsView::Pin(id);
        self.selected_row = 0;
        self.selected_column = 0;
        self.error = None;
        Ok(id)
    }
    pub fn rename_pin(&mut self, id: u64, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() || name.len() > MAX_NAME_BYTES {
            return Err(format!(
                "Snapshot name must contain 1–{MAX_NAME_BYTES} UTF-8 bytes"
            ));
        }
        let pin = self
            .pins
            .iter_mut()
            .find(|p| p.snapshot.id == id)
            .ok_or("Snapshot was closed")?;
        pin.name = name.into();
        self.rename = None;
        Ok(())
    }
    pub fn close_pin(&mut self, id: u64) {
        if self
            .worker
            .as_ref()
            .is_some_and(|w| w.left.id == id || w.right.id == id)
        {
            self.cancel();
        }
        if self
            .compared
            .as_ref()
            .is_some_and(|c| c.left.id == id || c.right.id == id)
        {
            self.compared = None;
        }
        self.pins.retain(|p| p.snapshot.id != id);
        if self.left == Some(id) {
            self.left = None;
        }
        if self.right == Some(id) {
            self.right = None;
        }
        if self.rename.as_ref().is_some_and(|(i, _)| *i == id) {
            self.rename = None;
        }
        if self.view == ResultsView::Pin(id)
            || (self.view == ResultsView::Comparison && self.compared.is_none())
        {
            self.view = ResultsView::Live;
        }
    }
    pub fn cancel(&mut self) {
        self.request = self.request.wrapping_add(1);
        if let Some(w) = &self.worker {
            w.cancel.store(true, Ordering::Relaxed);
        }
    }
    pub fn close_comparison(&mut self) {
        self.cancel();
        self.compared = None;
        self.error = None;
        if self.view == ResultsView::Comparison {
            self.view = ResultsView::Live;
        }
    }
    pub fn reset(&mut self) {
        self.cancel();
        self.pins.clear();
        self.compared = None;
        self.mapping = Mapping::default();
        self.query_origins.clear();
        self.table_origin = None;
        self.stream_remaining = None;
        self.left = None;
        self.right = None;
        self.rename = None;
        self.error = None;
        self.view = ResultsView::Live;
        self.show_mapping = false;
        self.show_source = false;
        self.selected_row = 0;
        self.selected_column = 0;
        // Keep the worker and its reservation until poll receives acknowledgement.
    }
    pub fn start(
        &mut self,
        route: Route,
        available: usize,
        runtime: &tokio::runtime::Handle,
    ) -> Result<(), String> {
        if self.worker.is_some() {
            return Err("Wait for the previous comparison to finish cancelling".into());
        }
        let left = self
            .left
            .and_then(|id| self.pin(id))
            .ok_or("Choose a left snapshot")?
            .snapshot
            .clone();
        let right = self
            .right
            .and_then(|id| self.pin(id))
            .ok_or("Choose a right snapshot")?
            .snapshot
            .clone();
        let bytes = comparison_bytes(left.result.rows.len(), right.result.rows.len())
            .saturating_add(
                (self.mapping.keys.len() + self.mapping.columns.len())
                    * std::mem::size_of::<rowboat_core::result_compare::ColumnPair>()
                    * 2,
            );
        if bytes > available {
            return Err(format!(
                "Comparison needs {bytes} bytes; {available} bytes available in shared result budget"
            ));
        }
        self.request = self.request.wrapping_add(1);
        self.compared = None;
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, receiver) = mpsc::sync_channel(1);
        let (l, r, m, c) = (
            left.clone(),
            right.clone(),
            self.mapping.clone(),
            cancel.clone(),
        );
        runtime.spawn_blocking(move || {
            let result = rowboat_core::result_compare::compare(
                &l.result,
                &r.result,
                &m,
                [l.partial, r.partial],
                bytes,
                &c,
            )
            .map_err(|e| e.to_string());
            // A delivery is the storage-release acknowledgement, not merely a
            // cancellation request. Drop worker input ownership before sending.
            drop((l, r, m, c));
            let _ = tx.send(result);
        });
        self.worker = Some(CompareWorker {
            request: self.request,
            route,
            cancel,
            receiver,
            left,
            right,
            mapping: self.mapping.clone(),
            bytes,
        });
        self.error = None;
        self.view = ResultsView::Comparison;
        self.selected_row = 0;
        self.selected_column = 0;
        Ok(())
    }
    pub fn poll(&mut self, route: Route) -> bool {
        let Some(w) = &self.worker else {
            return false;
        };
        let result = match w.receiver.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Err("Comparison worker stopped".into()),
        };
        let w = self.worker.take().unwrap();
        if w.route != route || w.request != self.request || w.cancel.load(Ordering::Relaxed) {
            return true;
        }
        match result {
            Ok(diff) => {
                self.compared = Some(Compared {
                    left: w.left.clone(),
                    right: w.right.clone(),
                    mapping: w.mapping.clone(),
                    diff,
                    bytes: w.bytes,
                })
            }
            Err(error) => self.error = Some(error),
        }
        true
    }
}

#[cfg(test)]
#[path = "result_snapshots_tests.rs"]
mod tests;
