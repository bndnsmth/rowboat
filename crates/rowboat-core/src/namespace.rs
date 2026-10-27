//! Connection-affine namespace discovery and guarded MySQL context selection.
use super::*;

use adapters::catalog::namespace_snapshot;

#[derive(Debug, Clone)]
pub struct DatabaseCatalog {
    pub database: String,
    pub databases: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NamespaceCatalog {
    pub(super) completion_proof: Option<super::completion::CompletionProof>,
    /// Actual server database, never the requested explorer namespace.
    pub database: String,
    /// Actual PostgreSQL current_schema(); None for MySQL.
    pub current_schema: Option<String>,
    pub namespaces: Vec<String>,
    /// Objects in the requested namespace, or actual default when omitted.
    pub tables: Vec<TableInfo>,
}

// A dropped future/unknown protocol outcome must not return an uncertain socket
// to another operation, especially after sending USE.
struct NamespaceLease<'a> {
    session: tokio::sync::MutexGuard<'a, Session>,
    armed: bool,
}
impl Drop for NamespaceLease<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.session.conn = Conn::Closed;
        }
    }
}

impl Database {
    /// Discover connectable server databases on demand, independently of schema
    /// discovery. This never switches context or opens another connection.
    pub async fn database_catalog(&self) -> Result<DatabaseCatalog> {
        let mut lease = NamespaceLease {
            session: self.inner.session.lock().await,
            armed: true,
        };
        let _running = self.metadata_running().await;
        let result = adapters::catalog::database_snapshot(&mut lease.session.conn).await;
        lease.armed = result.as_ref().is_err_and(adapters::unknown_outcome);
        result.context("Loading database catalog")
    }

    /// One bounded, physical-session-locked snapshot. `Some` scopes discovery
    /// only: it never issues USE or changes PostgreSQL search_path.
    pub async fn namespace_catalog(&self, namespace: Option<&str>) -> Result<NamespaceCatalog> {
        let mut lease = NamespaceLease {
            session: self.inner.session.lock().await,
            armed: true,
        };
        let _running = self.running().await;
        let mut result =
            namespace_snapshot(&mut lease.session.conn, self.engine(), namespace).await;
        if let Ok(snapshot) = &mut result {
            self.prove_completion_snapshot(snapshot, namespace);
        }
        // Completed server/local validation errors are reusable; unknown wire
        // outcomes are retired instead of releasing an uncertain connection.
        lease.armed = result.as_ref().is_err_and(adapters::unknown_outcome);
        result.context("Loading namespace catalog")
    }

    /// MySQL-only session context change. Rejects busy sessions, manual commit
    /// mode, and active/unknown transactions without COMMIT/ROLLBACK. Allowed on
    /// read-only connections; no transaction or read-only settings are changed.
    pub async fn select_database(&self, name: &str) -> Result<()> {
        self.inner.generation.fetch_add(1, Ordering::AcqRel);
        ensure!(
            self.engine() == Engine::MySql,
            "Database selection requires MySQL; reconnect to change PostgreSQL databases"
        );
        validate_database_name(name)?;
        let mut lease = NamespaceLease {
            session: self
                .inner
                .session
                .try_lock()
                .map_err(|_| anyhow!("Database selection rejected: session is busy"))?,
            armed: true,
        };
        // Wait out prior cancellation and prohibit backend cancellation during
        // transaction probing/USE. No running operation can share this lease.
        let _gate = self.inner.cancel_gate.lock().await;
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            if adapters::manual_commit(&mut lease.session.conn).await? {
                lease.armed = false;
                bail!("Database selection requires MySQL autocommit=1");
            }
            if server_in_transaction(&mut lease.session.conn).await? {
                lease.armed = false;
                bail!("Database selection rejected: an existing server transaction is active");
            }
            let result = adapters::select_database(&mut lease.session.conn, name).await;
            // An acknowledged server rejection (missing DB/permissions) leaves
            // the old context intact. Transport errors have unknown outcomes.
            if result.is_ok()
                || result
                    .as_ref()
                    .is_err_and(|e| adapters::driver_error(e) && !adapters::unknown_outcome(e))
            {
                lease.armed = false;
            }
            result.context("Selecting MySQL database")
        })
        .await;
        result.context("Database selection timed out; session retired, reconnect required")?
    }
}

fn validate_database_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.chars().count() <= 64
            && !name.ends_with(' ')
            && !name
                .chars()
                .any(|c| c.is_control() || matches!(c, '/' | '\\' | '.') || c as u32 > 0xffff),
        "Invalid MySQL database name"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_names_preserve_quoted_identifiers_and_reject_invalid_names() {
        for valid in [
            "plain",
            "has`quote",
            "has'quote",
            "two words",
            "数据库",
            "x;SELECT 1",
        ] {
            assert!(validate_database_name(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            " ",
            "trailing ",
            "a.b",
            "a/b",
            "a\\b",
            "a\0b",
            "a\nb",
            "💥",
        ] {
            assert!(validate_database_name(invalid).is_err(), "{invalid}");
        }
        assert!(validate_database_name(&"a".repeat(65)).is_err());
    }
}
