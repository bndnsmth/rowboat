//! Typed metadata leases and locally verified namespace snapshot reuse.
use super::*;
pub(super) use adapters::completion::{catalog, column_names};
#[derive(Debug, Clone)]
pub struct CompletionCatalog {
    pub search_path: Vec<String>,
    pub tables: Vec<TableInfo>,
}
#[derive(Debug, Clone)]
pub(super) struct CompletionProof {
    owner: Weak<Inner>,
    generation: u64,
    database: String,
    namespaces: Vec<String>,
    tables: Vec<TableInfo>,
}
impl Database {
    pub async fn server_version(&self) -> Result<String> {
        let mut session = self.inner.session.lock().await;
        let _running = self.metadata_running().await;
        adapters::completion::server_version(&mut session.conn, self.engine()).await
    }
    pub async fn completion_catalog(
        &self,
        extra_namespaces: &[String],
    ) -> Result<CompletionCatalog> {
        let mut session = self.inner.session.lock().await;
        let _running = self.metadata_running().await;
        catalog(&mut session.conn, self.engine(), extra_namespaces).await
    }
    pub async fn completion_column_names(&self, table: &TableRef) -> Result<Vec<String>> {
        let mut session = self.inner.session.lock().await;
        let _running = self.metadata_running().await;
        column_names(&mut session.conn, self.engine(), table).await
    }
    pub async fn completion_catalog_from_snapshot(
        &self,
        snapshot: &NamespaceCatalog,
    ) -> Result<CompletionCatalog> {
        let session = self.inner.session.lock().await;
        ensure!(
            !matches!(session.conn, Conn::Closed),
            "Connection closed; reconnect required"
        );
        let proof = snapshot
            .completion_proof
            .as_ref()
            .context("Namespace snapshot is not a verified current MySQL catalog")?;
        ensure!(
            proof
                .owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, &self.inner))
                && proof.generation == self.inner.generation.load(Ordering::Acquire),
            "Completion snapshot is stale or belongs to another session"
        );
        ensure!(
            snapshot.database == proof.database
                && snapshot.current_schema.is_none()
                && snapshot.namespaces == proof.namespaces
                && snapshot.tables.len() == proof.tables.len()
                && snapshot
                    .tables
                    .iter()
                    .zip(&proof.tables)
                    .all(|(a, b)| a.table == b.table && a.kind == b.kind),
            "Completion snapshot was modified"
        );
        Ok(CompletionCatalog {
            search_path: vec![proof.database.clone()],
            tables: proof.tables.clone(),
        })
    }
    pub(super) fn prove_completion_snapshot(
        &self,
        snapshot: &mut NamespaceCatalog,
        namespace: Option<&str>,
    ) {
        if self.engine() == Engine::MySql && namespace.is_none_or(|n| n == snapshot.database) {
            snapshot.completion_proof = Some(CompletionProof {
                owner: Arc::downgrade(&self.inner),
                generation: self.inner.generation.load(Ordering::Acquire),
                database: snapshot.database.clone(),
                namespaces: snapshot.namespaces.clone(),
                tables: snapshot.tables.clone(),
            });
        }
    }
}
