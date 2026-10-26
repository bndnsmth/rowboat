//! Scoped schema-only reads for the relationship diagram.
//! Register as a child of `database` (see the UI module's integration contract).
use super::*;

/// Per-expansion work bound, not a promise that all incoming SQLite keys were read.
pub const DIAGRAM_SQLITE_SCAN_LIMIT: usize = 32;
const COLUMN_LIMIT: usize = 256;
const RELATIONSHIP_LIMIT: usize = 128;
const READ_DEADLINE: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct DiagramCancellation(tokio::sync::watch::Sender<bool>);
impl Default for DiagramCancellation {
    fn default() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }
}
impl DiagramCancellation {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        if !*receiver.borrow() {
            let _ = receiver.changed().await;
        }
    }
    fn check(&self) -> Result<()> {
        ensure!(!self.is_cancelled(), "Diagram metadata request cancelled");
        Ok(())
    }
}

#[derive(Debug)]
pub struct DiagramMetadata {
    pub table: TableRef,
    pub columns: Vec<Column>,
    pub relationships: Vec<ForeignKeyRelationship>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct DiagramReadError {
    pub message: String,
    /// A deadline/aborted wire read must not lend an uncertain connection out.
    pub session_retired: bool,
    pub cancelled: bool,
}
impl std::fmt::Display for DiagramReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for DiagramReadError {}

impl Database {
    /// Read one table's columns and declared FK neighborhood, never table rows.
    /// Cancellation between reads is immediate. An in-flight metadata read drains
    /// under this method's lease; the 20s deadline retires a stuck session. No
    /// session-wide cancel is sent that could affect a following user query.
    pub async fn diagram_metadata(
        &self,
        requested: &TableRef,
        cancellation: &DiagramCancellation,
    ) -> std::result::Result<DiagramMetadata, DiagramReadError> {
        let error = |message: String, session_retired| DiagramReadError {
            message,
            session_retired,
            cancelled: cancellation.is_cancelled(),
        };
        ensure_qualified(requested).map_err(|e| error(e.to_string(), false))?;
        let session = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(error("Diagram request cancelled before reading metadata".into(), false)),
            lock = tokio::time::timeout(READ_DEADLINE, self.inner.session.lock()) =>
                lock.map_err(|_| error("Diagram metadata queue timed out; no read was started".into(), false))?,
        };
        let mut lease = ImportLease {
            session,
            armed: true,
        };
        let result = tokio::time::timeout(READ_DEADLINE, async {
            cancellation.check()?;
            let _running = self.running().await;
            cancellation.check()?;
            let table = if let Conn::Sqlite(c) = &mut lease.conn {
                c.resolve(requested).await?.0
            } else {
                requested.clone()
            };
            cancellation.check()?;
            let mut columns = structure(&mut lease.conn, self.engine(), &table).await?.columns;
            cancellation.check()?;
            let mut warnings = Vec::new();
            let mut relationships = if let Conn::Sqlite(c) = &mut lease.conn {
                // The generic SQLite API inspects every table's full structure.
                // Here outgoing keys are always read first, and incoming discovery
                // scans at most 32 namespace-local tables. Undiscovered keys remain
                // explicitly unknown rather than being claimed absent.
                let mut edges = c.outgoing_relationships(&table, &columns).await?;
                cancellation.check()?;
                let candidates = c.tables(&table.schema).await?;
                let candidates: Vec<_> = candidates.into_iter()
                    .filter(|t| t.kind == "TABLE" && t.table != table).collect();
                if candidates.len() > DIAGRAM_SQLITE_SCAN_LIMIT {
                    warnings.push(format!("Partial SQLite incoming metadata: scanned the first {DIAGRAM_SQLITE_SCAN_LIMIT} of {} other tables in this namespace. Expand other tables explicitly; absence of an edge is not proof of no relationship.", candidates.len()));
                }
                for info in candidates.into_iter().take(DIAGRAM_SQLITE_SCAN_LIMIT) {
                    cancellation.check()?;
                    let read = async {
                        let cols = c.structure(&info.table).await?.columns;
                        cancellation.check()?;
                        c.outgoing_relationships(&info.table, &cols).await
                    }.await;
                    match read {
                        Ok(found) => edges.extend(found.into_iter().filter(|r| r.referenced == table)),
                        Err(e) => {
                            cancellation.check()?;
                            if warnings.len() < 8 {
                                warnings.push(format!("Incoming keys unavailable for {}.{}: {e:#}", info.table.schema, info.table.name));
                            }
                        }
                    }
                    if edges.len() > RELATIONSHIP_LIMIT {
                        warnings.push("Partial diagram metadata: more than 128 relationships; remaining incoming scan stopped".into());
                        break;
                    }
                }
                edges
            } else {
                adapters::catalog::relationships(&mut lease.conn, self.engine(), &table).await?
            };
            cancellation.check()?;
            if columns.len() > COLUMN_LIMIT {
                warnings.push(format!("Column metadata limited to {COLUMN_LIMIT} of {} columns", columns.len()));
                columns.truncate(COLUMN_LIMIT);
            }
            if relationships.len() > RELATIONSHIP_LIMIT {
                warnings.push(format!("Relationship metadata limited to {RELATIONSHIP_LIMIT} of {} keys", relationships.len()));
                relationships.truncate(RELATIONSHIP_LIMIT);
            }
            Ok::<_, anyhow::Error>(DiagramMetadata { table, columns, relationships, warnings })
        }).await;
        match result {
            Ok(result) => {
                // All started reads completed, including cooperative cancellation.
                lease.armed = false;
                result.map_err(|e| error(format!("{e:#}"), false))
            }
            Err(_) => Err(error("Diagram metadata exceeded 20 seconds. This connection was retired to avoid reusing an unfinished read; reconnect explicitly. No row query was executed.".into(), true)),
        }
    }
}

fn ensure_qualified(table: &TableRef) -> Result<()> {
    ensure!(
        !table.schema.is_empty() && !table.name.is_empty(),
        "Diagram tables require an exact namespace and table name"
    );
    Ok(())
}

#[cfg(test)]
#[path = "diagram_metadata_tests.rs"]
mod tests;
