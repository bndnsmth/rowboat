//! One-producer bounded import protocol. Only an explicit Finish message can commit.
use crate::{
    Cell,
    stream::{QueryStreamOptions, StreamControl},
};
use anyhow::{Result, anyhow, ensure};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

pub type ImportRow = Vec<(String, Cell)>;
pub type ImportBatch = Vec<ImportRow>;

#[derive(Debug, Clone)]
pub struct RowImportOptions {
    pub batch_rows: usize,
    pub batch_bytes: usize,
    pub channel_capacity: usize,
    pub max_cell_bytes: usize,
    pub idle_timeout_ms: u64,
}
impl Default for RowImportOptions {
    fn default() -> Self {
        Self {
            batch_rows: 256,
            batch_bytes: 1024 * 1024,
            channel_capacity: 2,
            max_cell_bytes: 1024 * 1024,
            idle_timeout_ms: 30_000,
        }
    }
}
impl RowImportOptions {
    pub fn validate(&self) -> Result<()> {
        QueryStreamOptions {
            batch_rows: self.batch_rows,
            batch_bytes: self.batch_bytes,
            channel_capacity: self.channel_capacity,
            max_cell_bytes: self.max_cell_bytes,
            stall_timeout_ms: self.idle_timeout_ms,
            ..Default::default()
        }
        .validate()
    }
    pub(crate) fn validate_batch(&self, batch: &ImportBatch) -> Result<()> {
        ensure!(
            batch.len() <= self.batch_rows,
            "Import batch exceeds {} rows",
            self.batch_rows
        );
        let mut size = std::mem::size_of::<ImportBatch>();
        for row in batch {
            size = size.saturating_add(std::mem::size_of::<ImportRow>());
            for (name, cell) in row {
                let bytes = match cell {
                    Cell::Null => 0,
                    Cell::Text(s) => s.len(),
                    Cell::Binary(b) => b.len(),
                };
                ensure!(
                    bytes <= self.max_cell_bytes,
                    "Import cell exceeds {} bytes",
                    self.max_cell_bytes
                );
                size = size
                    .saturating_add(std::mem::size_of::<(String, Cell)>())
                    .saturating_add(name.len())
                    .saturating_add(bytes);
                ensure!(
                    size <= self.batch_bytes,
                    "Import batch exceeds {} bytes",
                    self.batch_bytes
                );
            }
        }
        ensure!(
            size <= self.batch_bytes,
            "Import batch exceeds {} bytes",
            self.batch_bytes
        );
        Ok(())
    }
}

pub(crate) enum ImportCommand {
    Rows(ImportBatch),
    Finish,
}
#[derive(Clone)]
pub(crate) struct ImportCompletion {
    pub result: std::result::Result<u64, String>,
    pub reusable: bool,
}

pub struct RowImport {
    tx: Option<mpsc::Sender<ImportCommand>>,
    control: Arc<StreamControl>,
    completion: watch::Receiver<Option<ImportCompletion>>,
    options: RowImportOptions,
}
impl RowImport {
    pub(crate) fn new(
        tx: mpsc::Sender<ImportCommand>,
        control: Arc<StreamControl>,
        completion: watch::Receiver<Option<ImportCompletion>>,
        options: RowImportOptions,
    ) -> Self {
        Self {
            tx: Some(tx),
            control,
            completion,
            options,
        }
    }
    pub async fn send(&mut self, rows: ImportBatch) -> Result<()> {
        if let Err(error) = self.options.validate_batch(&rows) {
            self.control.fail(&format!("{error:#}"));
            return Err(error);
        }
        self.send_command(ImportCommand::Rows(rows)).await
    }
    async fn send_command(&mut self, command: ImportCommand) -> Result<()> {
        self.control.check()?;
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| anyhow!("Import producer is closed"))?;
        tokio::select! {
            biased;
            _=self.control.cancelled()=>self.control.check(),
            sent=tx.send(command)=>{
                if sent.is_ok(){Ok(())}
                else {let done=self.wait_completion().await?;Err(anyhow!(done.result.err().unwrap_or_else(||"Import is already finished".into())))}
            },
        }
    }
    async fn wait_completion(&mut self) -> Result<ImportCompletion> {
        loop {
            if let Some(done) = self.completion.borrow().clone() {
                return Ok(done);
            }
            self.completion
                .changed()
                .await
                .map_err(|_| anyhow!("Import worker stopped before reporting completion"))?;
        }
    }
    pub async fn finish(mut self) -> Result<u64> {
        // Even if enqueue observes a concurrent error/cancel, await the worker's
        // authoritative outcome and completed rollback rather than guessing.
        let _ = self.send_command(ImportCommand::Finish).await;
        self.tx.take();
        let done = self.wait_completion().await?;
        done.result.map_err(|message| anyhow!(message))
    }
    pub fn cancel(&self) {
        self.control.stop("Import cancelled");
    }
    pub async fn close(mut self) -> Result<()> {
        self.control.stop("Import producer closed before Finish");
        self.tx.take();
        let done = self.wait_completion().await?;
        ensure!(
            done.reusable,
            "Import cleanup could not preserve the connection; reconnect required"
        );
        Ok(())
    }
}
impl Drop for RowImport {
    fn drop(&mut self) {
        self.control.stop("Import producer dropped before Finish");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn import_batches_count_names_overhead_and_cell_bytes() {
        let options = RowImportOptions {
            batch_rows: 2,
            batch_bytes: 256,
            max_cell_bytes: 128,
            ..Default::default()
        };
        assert!(options.validate().is_ok());
        assert!(
            options
                .validate_batch(&vec![vec![("id".into(), Cell::Text("1".into()))]])
                .is_ok()
        );
        assert!(options.validate_batch(&vec![vec![]; 3]).is_err());
        assert!(
            options
                .validate_batch(&vec![vec![("x".repeat(256), Cell::Null)]])
                .is_err()
        );
        assert!(
            options
                .validate_batch(&vec![vec![("x".into(), Cell::Binary(vec![0; 129]))]])
                .is_err()
        );
    }
}
