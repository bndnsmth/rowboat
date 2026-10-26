//! Identity evidence is collected on the same leased native session as the DDL.
use super::*;

pub(super) unsafe fn validate_single(ptr: *mut ffi::sqlite3, sql: &str) -> Result<()> {
    let sql = CString::new(sql)?;
    let mut cursor = sql.as_ptr();
    let end = unsafe { cursor.add(sql.as_bytes().len()) };
    let mut count = 0;
    while cursor < end {
        let mut statement = std::ptr::null_mut();
        let mut tail = std::ptr::null();
        let rc = unsafe {
            ffi::sqlite3_prepare_v3(
                ptr,
                cursor,
                end.offset_from(cursor) as i32,
                0,
                &mut statement,
                &mut tail,
            )
        };
        ensure!(rc == ffi::SQLITE_OK, "{}", unsafe { message(ptr) });
        ensure!(tail > cursor, "SQLite preparation made no progress");
        cursor = tail;
        if !statement.is_null() {
            let _statement = Statement(statement);
            count += 1;
            ensure!(
                count == 1,
                "Administrative SQLite SQL must be one native statement"
            );
        }
    }
    ensure!(
        count == 1,
        "Administrative SQLite SQL must contain one statement"
    );
    Ok(())
}

pub(super) unsafe fn file_unmoved(ptr: *mut ffi::sqlite3, schema: &str) -> Result<()> {
    let name = CString::new(schema)?;
    let mut moved: c_int = 0;
    let code = unsafe {
        ffi::sqlite3_file_control(
            ptr,
            name.as_ptr(),
            ffi::SQLITE_FCNTL_HAS_MOVED,
            (&mut moved as *mut c_int).cast(),
        )
    };
    ensure!(
        code == ffi::SQLITE_OK && moved == 0,
        "SQLite file was replaced/moved, or its VFS cannot prove file identity; reconnect and review again"
    );
    Ok(())
}

impl SqliteAdapter {
    pub(crate) async fn validate_administrative_statement(&self, sql: &str) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::ValidateSingle {
                sql: sql.into(),
                reply,
            })
            .context("SQLite worker closed")?;
        response
            .await
            .context("SQLite prepare worker stopped")?
            .map_err(anyhow::Error::msg)
    }
    pub(crate) async fn schema_identity(&mut self, schema: &str) -> Result<Vec<Cell>> {
        ensure!(
            !schema.is_empty() && !schema.contains('\0'),
            "Explicit SQLite namespace required"
        );
        let databases = self.databases().await?;
        let (_, file) = databases
            .iter()
            .find(|(name, _)| name == schema)
            .context("SQLite namespace is no longer attached")?;
        let mut identity = vec![
            Cell::Text(self.control.instance.clone()),
            Cell::Text(
                self.control
                    .namespace_epoch
                    .load(Ordering::Acquire)
                    .to_string(),
            ),
            Cell::Text(schema.into()),
            Cell::Text(file.clone()),
        ];
        if !file.is_empty() {
            let (reply, response) = oneshot::channel();
            self.tx
                .send(Command::FileUnmoved {
                    schema: schema.into(),
                    reply,
                })
                .context("SQLite worker closed")?;
            response
                .await
                .context("SQLite identity worker stopped")?
                .map_err(anyhow::Error::msg)?;
            let metadata = std::fs::metadata(file).context("Inspecting SQLite file identity")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                identity.push(Cell::Text(format!("{}:{}", metadata.dev(), metadata.ino())));
            }
            #[cfg(not(unix))]
            {
                let _ = metadata;
                bail!("SQLite reviewed schema changes require filesystem identity support");
            }
        }
        for pragma in ["schema_version", "journal_mode"] {
            let result = self
                .one(
                    &format!(
                        "PRAGMA {}.{pragma}",
                        sql::quote_ident(Engine::Sqlite, schema)
                    ),
                    &[],
                    1,
                )
                .await?;
            let row = result
                .rows
                .first()
                .context("SQLite identity pragma returned no row")?;
            if pragma == "journal_mode" {
                ensure!(
                    text(&row[0]) != "off",
                    "Reviewed SQLite DDL requires rollback journaling; journal_mode=OFF is unsupported"
                );
            }
            identity.extend(row.clone());
        }
        for pragma in [
            "foreign_keys",
            "legacy_alter_table",
            "writable_schema",
            "query_only",
        ] {
            let result = self.one(&format!("PRAGMA {pragma}"), &[], 1).await?;
            let row = result
                .rows
                .first()
                .context("SQLite safety pragma unavailable")?;
            if matches!(
                pragma,
                "legacy_alter_table" | "writable_schema" | "query_only"
            ) {
                ensure!(
                    text(&row[0]) == "0",
                    "SQLite reviewed DDL requires {pragma}=OFF; connection flags are never changed automatically"
                );
            }
            identity.extend(row.clone());
        }
        Ok(identity)
    }
}
