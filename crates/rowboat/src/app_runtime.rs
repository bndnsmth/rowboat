//! The UI owns handles only; destroying an entity must never join Tokio threads.
pub struct AppRuntime(Option<tokio::runtime::Runtime>);
impl AppRuntime {
    pub fn new() -> std::io::Result<Self> {
        tokio::runtime::Runtime::new().map(|runtime| Self(Some(runtime)))
    }
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.0.as_ref().unwrap().handle().clone()
    }
}
impl Drop for AppRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AppRuntime;
    use std::{sync::mpsc, thread, time::Duration};
    #[test]
    fn dropping_runtime_owner_never_waits_for_a_blocking_worker_or_surviving_handle() {
        let (release, blocked) = mpsc::channel();
        let (dropped, returned) = mpsc::channel();
        let owner = thread::spawn(move || {
            let runtime = AppRuntime::new().unwrap();
            let handle = runtime.handle();
            let (started, ready) = mpsc::channel();
            handle.spawn_blocking(move || {
                started.send(()).unwrap();
                let _ = blocked.recv();
            });
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(runtime);
            dropped.send(()).unwrap();
            drop(handle);
        });
        let completed = returned.recv_timeout(Duration::from_secs(5));
        // Always release the worker before asserting, so a regression cannot
        // strand the test harness in Runtime::drop's blocking-pool join.
        let _ = release.send(());
        owner.join().unwrap();
        assert!(
            completed.is_ok(),
            "Runtime destruction waited for a blocking worker"
        );
    }
}
