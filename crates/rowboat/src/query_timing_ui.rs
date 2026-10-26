//! In-memory timings only. Never alter the core result used for execution proof.
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct QueryTiming {
    pub started: Instant,
    pub launch_ms: u64,
    pub first_schema_ms: Option<u64>,
    pub terminal_ms: Option<u64>,
    pub cleanup_capture_ms: u64,
    pub state_check_ms: u64,
    pub context_ms: Option<u64>,
    pub worker_ms: u64,
    pub total_ms: Option<u64>,
    pub result_fetch_ms: u64,
    pub core: Option<rowboat_core::QueryTimings>,
}
impl QueryTiming {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            launch_ms: 0,
            first_schema_ms: None,
            terminal_ms: None,
            cleanup_capture_ms: 0,
            state_check_ms: 0,
            context_ms: None,
            worker_ms: 0,
            total_ms: None,
            result_fetch_ms: 0,
            core: None,
        }
    }
    pub fn elapsed(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
    pub fn detail(&self) -> String {
        let total = self.total_ms.map_or_else(
            || "context verification pending".into(),
            |ms| format!("{ms} ms total to ready"),
        );
        let first = self.first_schema_ms.map_or_else(
            || "no result schema received".into(),
            |ms| format!("{ms} ms to first result schema (queue + setup + initial SQL/read)"),
        );
        if let Some(core) = self.core {
            return format!(
                "{total}\n{first}\nCore: {} ms queue · {} ms source proof\n{} ms execution/streaming · {} ms cleanup\n{} ms core total · {} ms worker total\n{} ms transaction check · {}\n{} ms SQL/result fetch reported by ResultEnd. Execution includes network/read/backpressure, not server-only time. Total also includes GUI handoff. No SQL is replayed for timing.",
                core.queue_wait_ms,
                core.source_capture_ms,
                core.execution_ms,
                core.cleanup_ms,
                core.total_ms,
                self.worker_ms,
                self.state_check_ms,
                self.context_ms.map_or_else(
                    || "context check pending".into(),
                    |ms| format!("{ms} ms namespace verification")
                ),
                self.result_fetch_ms
            );
        }
        let terminal = self.terminal_ms.map_or_else(
            || "no terminal event".into(),
            |ms| format!("{ms} ms to stream terminal (includes source-proof cleanup)"),
        );
        format!(
            "{total}\n{} ms SQL/result fetch (core ResultEnd; not server-only)\n{first}\n{terminal}\n{} ms handle launch · {} ms final capture/close\n{} ms transaction check · {}\nTotal includes UI handoff; milestones overlap, do not sum them.",
            self.result_fetch_ms,
            self.launch_ms,
            self.cleanup_capture_ms,
            self.state_check_ms,
            self.context_ms.map_or_else(
                || "context check pending".into(),
                |ms| format!("{ms} ms namespace verification")
            )
        )
    }
}
