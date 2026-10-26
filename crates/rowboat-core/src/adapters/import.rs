//! Engine target/trigger/transactional-storage evidence for atomic import.
use super::super::*;

pub(crate) enum Target {
    Network(TableRef),
    Sqlite(super::sqlite::Target),
}

pub(crate) async fn inspect(conn: &mut Conn, engine: Engine, table: &TableRef) -> Result<Target> {
    if let Conn::Sqlite(adapter) = conn {
        return adapter.import_target(table).await.map(Target::Sqlite);
    }
    inspect_import_target(conn, engine, table)
        .await
        .map(Target::Network)
}

pub(crate) async fn begin(conn: &mut Conn) -> Result<()> {
    match conn {
        Conn::Sqlite(adapter) => adapter.execute("BEGIN IMMEDIATE").await,
        _ => execute(conn, "START TRANSACTION").await,
    }
}

pub(crate) async fn lock_and_columns(
    conn: &mut Conn,
    engine: Engine,
    target: &Target,
) -> Result<Vec<Column>> {
    match target {
        Target::Sqlite(target) => {
            let Conn::Sqlite(adapter) = conn else {
                bail!("Import connection changed")
            };
            adapter.lock_import(target).await?;
            Ok(target.columns.clone())
        }
        Target::Network(table) => {
            lock_import(conn, engine, table).await?;
            inspect_import_target(conn, engine, table).await?;
            Ok(structure(conn, engine, table).await?.columns)
        }
    }
}

pub(crate) async fn insert(
    conn: &mut Conn,
    engine: Engine,
    target: &Target,
    columns: &[Column],
    values: crate::importer::ImportRow,
) -> Result<u64> {
    match target {
        Target::Sqlite(target) => {
            let Conn::Sqlite(adapter) = conn else {
                bail!("Import connection changed")
            };
            adapter.import_insert(target, &values).await
        }
        Target::Network(table) => {
            let (sql, params) =
                change_statements(engine, table, columns, &[RowChange::Insert { values }])?
                    .into_iter()
                    .next()
                    .context("Missing insert statement")?;
            Ok(bound_results(conn, &sql, &params, 0).await?.rows_affected)
        }
    }
}

pub(crate) async fn rollback(conn: &mut Conn) -> Result<()> {
    if let Conn::Sqlite(adapter) = conn {
        // SQLITE_INTERRUPT and ON CONFLICT ROLLBACK can already have ended it.
        if adapter.in_transaction().await? {
            adapter.execute("ROLLBACK").await?;
        }
        ensure!(
            !adapter.in_transaction().await?,
            "SQLite import rollback left a transaction active"
        );
        return Ok(());
    }
    execute(conn, "ROLLBACK").await
}

async fn inspect_import_target(
    conn: &mut Conn,
    engine: Engine,
    table: &TableRef,
) -> Result<TableRef> {
    let params = [
        Cell::Text(table.schema.clone()),
        Cell::Text(table.name.clone()),
    ];
    match engine {
        Engine::Sqlite => bail!("SQLite import requires its native adapter"),
        Engine::Postgres => {
            let result=bound_results(conn,r#"SELECT n.nspname,c.relname,c.relkind::text,c.relhasrules::text,
                EXISTS(SELECT 1 FROM pg_trigger t WHERE t.tgrelid=c.oid AND NOT t.tgisinternal)::text
                FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
                WHERE n.nspname=COALESCE(NULLIF($1::text,''),current_schema()) AND c.relname=$2::text"#,&params,1).await?;
            let row = result
                .rows
                .first()
                .context("Import target does not exist or is not visible")?;
            ensure!(
                text(&row[2]) == "r",
                "Atomic import supports ordinary PostgreSQL base tables, not views, foreign or partitioned targets"
            );
            ensure!(
                !truth(&row[3]),
                "Atomic import rejects tables with rewrite rules"
            );
            ensure!(
                !truth(&row[4]),
                "Atomic import rejects tables with user triggers; their effects cannot be proven safe"
            );
            Ok(TableRef {
                schema: text(&row[0]),
                name: text(&row[1]),
            })
        }
        Engine::MySql => {
            let result=bound_results(conn,"SELECT TABLE_SCHEMA,TABLE_NAME,TABLE_TYPE,ENGINE FROM information_schema.TABLES WHERE TABLE_SCHEMA=COALESCE(NULLIF(?,''),DATABASE()) AND TABLE_NAME=?",&params,1).await?;
            let row = result
                .rows
                .first()
                .context("Import target does not exist or is not visible")?;
            ensure!(
                text(&row[2]) == "BASE TABLE" && text(&row[3]).eq_ignore_ascii_case("InnoDB"),
                "Atomic import requires a MySQL InnoDB base table; views/nontransactional engines are rejected"
            );
            let resolved = TableRef {
                schema: text(&row[0]),
                name: text(&row[1]),
            };
            let effective = raw_results(
                conn,
                &format!("SHOW CREATE TABLE {}", qualified_table(engine, &resolved)),
                1,
            )
            .await?;
            let definition = effective
                .first()
                .and_then(|r| r.rows.first())
                .and_then(|r| r.get(1))
                .map(text)
                .context("Missing effective MySQL import target definition")?;
            let mut prefix = definition.split_whitespace();
            ensure!(
                prefix
                    .next()
                    .is_some_and(|w| w.eq_ignore_ascii_case("CREATE"))
                    && prefix
                        .next()
                        .is_some_and(|w| w.eq_ignore_ascii_case("TABLE")),
                "Atomic import requires a permanent MySQL InnoDB base table; temporary or shadowed targets are rejected"
            );
            let account = bound_results(conn, "SELECT CURRENT_USER()", &[], 1).await?;
            let account = text(
                &account
                    .rows
                    .first()
                    .context("Missing effective MySQL account")?[0],
            );
            let (user, host) = account
                .rsplit_once('@')
                .context("Unrecognized MySQL effective account")?;
            let grantee = Cell::Text(format!("'{user}'@'{host}'"));
            let visible=bound_results(conn,"SELECT '1' FROM information_schema.USER_PRIVILEGES WHERE CAST(GRANTEE AS BINARY)=CAST(? AS BINARY) AND PRIVILEGE_TYPE='TRIGGER' UNION ALL SELECT '1' FROM information_schema.SCHEMA_PRIVILEGES WHERE CAST(GRANTEE AS BINARY)=CAST(? AS BINARY) AND TABLE_SCHEMA=? AND PRIVILEGE_TYPE='TRIGGER' UNION ALL SELECT '1' FROM information_schema.TABLE_PRIVILEGES WHERE CAST(GRANTEE AS BINARY)=CAST(? AS BINARY) AND TABLE_SCHEMA=? AND TABLE_NAME=? AND PRIVILEGE_TYPE='TRIGGER' LIMIT 1",&[grantee.clone(),grantee.clone(),Cell::Text(resolved.schema.clone()),grantee,Cell::Text(resolved.schema.clone()),Cell::Text(resolved.name.clone())],1).await?;
            ensure!(
                !visible.rows.is_empty(),
                "Atomic import requires direct effective-account TRIGGER visibility to prove there are no hidden triggers (global, schema, or table grant; inherited-only grants are not inferred)"
            );
            let triggers=bound_results(conn,"SELECT TRIGGER_NAME FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA=? AND EVENT_OBJECT_TABLE=? LIMIT 1",&[Cell::Text(resolved.schema.clone()),Cell::Text(resolved.name.clone())],1).await?;
            ensure!(
                triggers.rows.is_empty(),
                "Atomic import rejects MySQL tables with triggers; transitive/nontransactional effects cannot be proven safe"
            );
            Ok(resolved)
        }
    }
}
async fn lock_import(conn: &mut Conn, engine: Engine, table: &TableRef) -> Result<()> {
    let target = qualified_table(engine, table);
    match engine {
        Engine::Postgres => execute(conn, &format!("LOCK TABLE {target} IN ROW EXCLUSIVE MODE"))
            .await
            .context("Locking import target against metadata changes"),
        Engine::MySql => {
            raw_results(conn, &format!("SELECT 1 FROM {target} LIMIT 0"), 0)
                .await
                .context("Locking MySQL import metadata")?;
            Ok(())
        }
        Engine::Sqlite => bail!("SQLite import requires its native adapter"),
    }
}
