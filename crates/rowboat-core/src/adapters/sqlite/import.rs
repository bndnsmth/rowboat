//! Atomic import evidence and native binding. The shared worker owns the bounded
//! producer protocol; this adapter owns SQLite namespace/transaction semantics.
use super::*;

impl SqliteAdapter {
    pub(crate) async fn import_target(&mut self, requested: &TableRef) -> Result<Target> {
        ensure!(!self.read_only, "This SQLite connection is read-only");
        let target = self.inspect(requested).await?;
        // Catalog type is authoritative, including FTS shadow tables and virtual
        // declarations with unusual whitespace/comments.
        let listed = self
            .one(
                "SELECT type FROM pragma_table_list WHERE schema=? AND name=?",
                &[
                    Cell::Text(target.table.schema.clone()),
                    Cell::Text(target.table.name.clone()),
                ],
                2,
            )
            .await?;
        ensure!(
            listed.rows.len() == 1 && text(&listed.rows[0][0]) == "table",
            "Atomic SQLite import requires an ordinary base table, not a view, virtual or shadow table"
        );
        let journal = self
            .one(
                &format!(
                    "PRAGMA {}.journal_mode",
                    sql::quote_ident(Engine::Sqlite, &target.table.schema)
                ),
                &[],
                1,
            )
            .await?;
        ensure!(
            journal
                .rows
                .first()
                .and_then(|row| row.first())
                .is_some_and(|cell| !text(cell).eq_ignore_ascii_case("off")),
            "Atomic SQLite import requires rollback journaling; journal_mode=OFF is unsupported"
        );
        // Reject user triggers as in the network providers. TEMP triggers can
        // target other namespaces; same-name ambiguity is conservatively rejected.
        for schema in [target.table.schema.as_str(), "temp"] {
            let triggers = self.one(
                &format!("SELECT name FROM {}.sqlite_schema WHERE type='trigger' AND tbl_name=? COLLATE NOCASE LIMIT 1", sql::quote_ident(Engine::Sqlite, schema)),
                &[Cell::Text(target.table.name.clone())], 1,
            ).await?;
            ensure!(
                triggers.rows.is_empty(),
                "Atomic SQLite import rejects tables with triggers"
            );
        }
        Ok(target)
    }

    pub(crate) async fn lock_import(&mut self, expected: &Target) -> Result<()> {
        ensure!(
            self.in_transaction().await?,
            "SQLite import requires its owned transaction"
        );
        // BEGIN IMMEDIATE reserves the write transaction before this reinspection.
        // All later INSERTs use this exact qualified name under the session lease.
        let current = self.import_target(&expected.table).await?;
        ensure!(
            current.table == expected.table
                && current.identity == expected.identity
                && current.signature == expected.signature,
            "SQLite import target identity or schema changed; review the target again"
        );
        Ok(())
    }

    pub(crate) async fn import_insert(
        &mut self,
        target: &Target,
        values: &crate::importer::ImportRow,
    ) -> Result<u64> {
        let mut names = std::collections::HashSet::new();
        let mut params = Vec::with_capacity(values.len());
        for (name, value) in values {
            let index = target
                .columns
                .iter()
                .position(|c| c.name == *name)
                .context("Unknown SQLite import column")?;
            ensure!(
                target.editable[index],
                "SQLite generated/hidden columns cannot be imported"
            );
            ensure!(names.insert(name), "Duplicate SQLite import column");
            // Never infer a storage class from a display string or CAST a value
            // through floating point. SQLite applies the target's own affinity.
            params.push(Value::from_cell(value));
        }
        let table = sql::qualified_table(Engine::Sqlite, &target.table);
        // Insert-only Apply must override schema conflict policies: REPLACE can
        // delete existing rows (and cascade children) while reporting one change,
        // or substitute a default for explicit NULL. ABORT preserves the error so
        // the worker can roll back every earlier row and batch.
        let statement = if values.is_empty() {
            format!("INSERT OR ABORT INTO {table} DEFAULT VALUES")
        } else {
            format!(
                "INSERT OR ABORT INTO {table} ({}) VALUES ({})",
                values
                    .iter()
                    .map(|(name, _)| sql::quote_ident(Engine::Sqlite, name))
                    .collect::<Vec<_>>()
                    .join(","),
                vec!["?"; values.len()].join(",")
            )
        };
        let result = self.query_native(&statement, params, 0).await?;
        ensure!(result.len() == 1, "Missing SQLite import result");
        Ok(result[0].rows_affected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // Exercise the actual pre-BEGIN/post-BEGIN identity boundary deterministically.
    // Facade tests separately prove that a ready producer owns the session lease.
    #[tokio::test]
    async fn replaced_attachment_and_changed_schema_fail_import_revalidation() -> Result<()> {
        let fixture = Fixture(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.local")
                .join(format!(
                    "pass14_import_identity_{}",
                    uuid::Uuid::new_v4().simple()
                )),
        );
        std::fs::create_dir_all(&fixture.0)?;
        let mut adapter = SqliteAdapter::connect(&ConnectionProfile {
            engine: Engine::Sqlite,
            file: Some(SqliteFileOptions {
                path: fixture.0.join("main.sqlite"),
                mode: SqliteOpenMode::CreateIfMissing,
            }),
            ..Default::default()
        })
        .await?;
        for memory in [true, false] {
            let file = fixture.0.join("attached.sqlite");
            let source = if memory {
                ":memory:".to_owned()
            } else {
                file.to_string_lossy().replace('\'', "''")
            };
            let attach = format!("ATTACH '{source}' AS aux");
            adapter.execute(&attach).await?;
            adapter
                .execute("CREATE TABLE aux.t(id INTEGER PRIMARY KEY)")
                .await?;
            let expected = adapter
                .import_target(&TableRef {
                    schema: "aux".into(),
                    name: "t".into(),
                })
                .await?;
            adapter.execute("DETACH aux").await?;
            if !memory {
                std::fs::rename(&file, file.with_extension("old"))?;
            }
            adapter.execute(&attach).await?;
            adapter
                .execute("CREATE TABLE aux.t(id INTEGER PRIMARY KEY)")
                .await?;
            adapter.execute("BEGIN IMMEDIATE").await?;
            assert!(
                adapter
                    .lock_import(&expected)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("identity")
            );
            adapter.execute("ROLLBACK").await?;
            let fresh = adapter.import_target(&expected.table).await?;
            adapter.execute("BEGIN IMMEDIATE").await?;
            adapter.lock_import(&fresh).await?;
            adapter.execute("ROLLBACK").await?;
            adapter.execute("DETACH aux").await?;
        }
        adapter
            .execute("CREATE TABLE t(id INTEGER PRIMARY KEY)")
            .await?;
        let expected = adapter
            .import_target(&TableRef {
                schema: "main".into(),
                name: "t".into(),
            })
            .await?;
        adapter
            .execute("ALTER TABLE t ADD COLUMN note TEXT")
            .await?;
        adapter.execute("BEGIN IMMEDIATE").await?;
        assert!(adapter.lock_import(&expected).await.is_err());
        adapter.execute("ROLLBACK").await?;
        Ok(())
    }
}
