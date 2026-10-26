use anyhow::{Context, Result, ensure};
use rowboat_admin::{
    ddl::*,
    live_diff::{NamespaceMapping, columns::*},
    object_editor::*,
    objects::ObjectKind,
};
use rowboat_core::{
    AdministrativeStatus, Cell, ConnectionProfile, Database, Engine, QuerySession, TableRef,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, timeout};
fn profile(engine: Engine) -> ConnectionProfile {
    ConnectionProfile {
        engine,
        host: "127.0.0.1".into(),
        port: if engine == Engine::Postgres {
            15435
        } else {
            13309
        },
        username: if engine == Engine::Postgres {
            "postgres"
        } else {
            "root"
        }
        .into(),
        database: "rowboat".into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    }
}
async fn connect(engine: Engine) -> Result<Database> {
    Database::connect(&profile(engine), "rowboat-test").await
}
fn unique() -> String {
    format!(
        "ko5a{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
async fn exec(db: &Database, sql: &str) -> Result<()> {
    db.query(sql, 10).await?;
    Ok(())
}
async fn scalar(db: &Database, sql: &str) -> Result<String> {
    let r = db.query(sql, 10).await?;
    match &r[0].rows[0][0] {
        Cell::Text(s) => Ok(s.clone()),
        Cell::Binary(b) => Ok(String::from_utf8(b.clone())?),
        _ => anyhow::bail!("NULL scalar"),
    }
}
async fn snapshot(db: &Database, ns: &str) -> Result<ObjectSnapshot> {
    let id = resolve_object(db, ns, ObjectKind::MaterializedView, "mv", None, None).await?;
    inspect_object(db, &id).await
}
async fn wait_blocked(observer: &Database, pid: &str) -> Result<()> {
    timeout(Duration::from_secs(5),async {loop{if scalar(observer,&format!("SELECT count(*) FROM pg_stat_activity WHERE pid={pid} AND wait_event='advisory'")).await?=="1"{return Ok::<_,anyhow::Error>(());}sleep(Duration::from_millis(10)).await;}}).await.context("CREATE never reached the advisory-lock barrier")?
}

#[tokio::test]
#[ignore = "Task-owned PG15435 fixtures"]
async fn postgres_text_numeric_materialized_replacement_recreates_only_owned_toast() -> Result<()> {
    let db = connect(Engine::Postgres).await?;
    let ns = unique();
    let query = |factor| {
        format!(
            "SELECT c.id,c.name,COALESCE(sum(p.amount),0)*{factor} AS total FROM {ns}.customers c LEFT JOIN {ns}.payments p ON p.customer_id=c.id GROUP BY c.id,c.name"
        )
    };
    exec(&db,&format!("CREATE SCHEMA {ns}; CREATE TABLE {ns}.customers(id INTEGER PRIMARY KEY,name TEXT); CREATE TABLE {ns}.payments(customer_id INTEGER,amount NUMERIC); INSERT INTO {ns}.customers VALUES(1,'Ada'),(2,'Bea'); INSERT INTO {ns}.payments VALUES(1,10.25),(1,4.75); CREATE MATERIALIZED VIEW {ns}.mv AS {}; CREATE UNIQUE INDEX mv_id ON {ns}.mv(id); CREATE INDEX mv_name ON {ns}.mv(name); COMMENT ON MATERIALIZED VIEW {ns}.mv IS 'keep view'; COMMENT ON COLUMN {ns}.mv.name IS 'keep text'; COMMENT ON COLUMN {ns}.mv.total IS 'keep numeric'; COMMENT ON INDEX {ns}.mv_name IS 'keep index'",query(1))).await?;
    let toast_sql = format!(
        "SELECT toast.oid::text FROM pg_class mv JOIN pg_class toast ON toast.oid=mv.reltoastrelid JOIN pg_depend d ON d.objid=toast.oid AND d.classid='pg_class'::regclass AND d.refclassid='pg_class'::regclass AND d.refobjid=mv.oid AND d.deptype='i' AND d.objsubid=0 AND d.refobjsubid=0 WHERE mv.oid='{ns}.mv'::regclass AND toast.relkind='t'"
    );
    let old_toast = scalar(&db, &toast_sql).await?;
    let metadata_sql = format!(
        "SELECT json_build_object('owner',pg_get_userbyid(c.relowner),'acl',c.relacl,'comment',obj_description(c.oid),'columns',(SELECT json_agg(json_build_object('name',a.attname,'type',format_type(a.atttypid,a.atttypmod),'comment',col_description(c.oid,a.attnum),'acl',a.attacl) ORDER BY a.attnum) FROM pg_attribute a WHERE a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped),'indexes',(SELECT json_agg(json_build_object('name',ic.relname,'owner',pg_get_userbyid(ic.relowner),'definition',pg_get_indexdef(ic.oid),'comment',obj_description(ic.oid)) ORDER BY ic.relname) FROM pg_index i JOIN pg_class ic ON ic.oid=i.indexrelid WHERE i.indrelid=c.oid))::text FROM pg_class c WHERE c.oid='{ns}.mv'::regclass"
    );
    let before = scalar(&db, &metadata_sql).await?;
    let original = snapshot(&db, &ns).await?;
    let edit = plan_edit(
        &original,
        ObjectPatch::MaterializedQuery { query: query(2) },
    )?;
    let result = apply_object_edit(&db, &edit).await?;
    ensure!(
        result.status == ObjectApplyStatus::Applied,
        "Text/numeric replacement failed: {result:?}"
    );
    ensure!(scalar(&db, &format!("SELECT total::text FROM {ns}.mv WHERE id=1")).await? == "30.00");
    ensure!(scalar(&db, &format!("SELECT total::text FROM {ns}.mv WHERE id=2")).await? == "0");
    ensure!(
        scalar(&db, &metadata_sql).await? == before,
        "Owner/default ACLs, column metadata, comments or indexes changed"
    );
    ensure!(
        scalar(&db, &toast_sql).await? != old_toast,
        "Replacement did not recreate native TOAST"
    );
    ensure!(
        scalar(
            &db,
            &format!("SELECT count(*) FROM pg_class WHERE oid={old_toast}::oid")
        )
        .await?
            == "0",
        "Old TOAST leaked"
    );
    // The TOAST exemption is specific to native storage; an external view's
    // rewrite dependency must still block replacement.
    exec(
        &db,
        &format!("CREATE VIEW {ns}.dependent AS SELECT name,total FROM {ns}.mv"),
    )
    .await?;
    let dependent = snapshot(&db, &ns).await?;
    ensure!(
        plan_edit(
            &dependent,
            ObjectPatch::MaterializedQuery { query: query(3) }
        )
        .is_err(),
        "External dependent view was ignored"
    );
    ensure!(
        scalar(
            &db,
            &format!("SELECT name FROM {ns}.dependent WHERE total=30")
        )
        .await?
            == "Ada"
    );
    exec(&db,&format!("DROP VIEW {ns}.dependent; ALTER MATERIALIZED VIEW {ns}.mv SET (toast.autovacuum_enabled=false)")).await?;
    ensure!(
        plan_edit(
            &snapshot(&db, &ns).await?,
            ObjectPatch::MaterializedQuery { query: query(3) }
        )
        .is_err(),
        "Custom TOAST options were silently discarded"
    );
    exec(
        &db,
        &format!("ALTER MATERIALIZED VIEW {ns}.mv RESET (toast.autovacuum_enabled)"),
    )
    .await?;
    ensure!(
        plan_edit(
            &snapshot(&db, &ns).await?,
            ObjectPatch::MaterializedQuery { query: query(3) }
        )
        .is_ok(),
        "Reset default TOAST options still block"
    );
    exec(&db, &format!("GRANT SELECT ON {ns}.mv TO PUBLIC")).await?;
    let granted = scalar(&db, &metadata_sql).await?;
    let protected = snapshot(&db, &ns).await?;
    ensure!(
        plan_edit(
            &protected,
            ObjectPatch::MaterializedQuery { query: query(3) }
        )
        .is_err(),
        "Explicit grants must still block rather than be lost"
    );
    ensure!(
        scalar(&db, &metadata_sql).await? == granted,
        "Rejected plan changed grants"
    );
    exec(&db, &format!("DROP SCHEMA {ns} CASCADE")).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "Task-owned PG15435/MySQL13309 fixtures"]
async fn public_apply_abort_and_cancel_restore_pg_before_queued_commit() -> Result<()> {
    let db = connect(Engine::Postgres).await?;
    let observer = connect(Engine::Postgres).await?;
    let ns = unique();
    let pid = scalar(&db, "SELECT pg_backend_pid()").await?;
    let key: i64 = pid.parse()?;
    exec(&db,&format!("CREATE SCHEMA {ns}; CREATE MATERIALIZED VIEW {ns}.mv AS SELECT 1 AS value; CREATE FUNCTION {ns}.barrier() RETURNS INTEGER LANGUAGE plpgsql AS $b$BEGIN PERFORM pg_advisory_xact_lock({key}); RETURN 2; END$b$")).await?;
    let initial = plan_edit(
        &snapshot(&db, &ns).await?,
        ObjectPatch::MaterializedQuery {
            query: "SELECT 7 AS value".into(),
        },
    )?;
    db.begin().await?;
    ensure!(db.query("SELECT 1/0", 1).await.is_err());
    ensure!(apply_object_edit(&db, &initial).await.is_err());
    ensure!(
        db.in_transaction().await?,
        "Rejected apply destroyed caller's failed transaction"
    );
    db.rollback().await?;
    for abort in [true, false] {
        exec(&observer, &format!("SELECT pg_advisory_lock({key})")).await?;
        let edit = plan_edit(
            &snapshot(&db, &ns).await?,
            ObjectPatch::MaterializedQuery {
                query: format!("SELECT {ns}.barrier() AS value"),
            },
        )?;
        let d = db.clone();
        let mut applying = tokio::spawn(async move { apply_object_edit(&d, &edit).await });
        wait_blocked(&observer, &pid).await?; // DROP acknowledged; CREATE is in flight
        let d = db.clone();
        let commit = tokio::spawn(async move { d.commit().await });
        let d = db.clone();
        let sql = format!("SELECT value FROM {ns}.mv");
        let query = tokio::spawn(async move { scalar(&d, &sql).await });
        sleep(Duration::from_millis(30)).await;
        ensure!(
            !commit.is_finished() && !query.is_finished(),
            "Same-session operation interleaved inside DROP/CREATE"
        );
        if abort {
            applying.abort();
            ensure!(applying.await.unwrap_err().is_cancelled());
        } else {
            db.cancel().await?;
            let result = timeout(Duration::from_secs(10), &mut applying).await???;
            ensure!(
                result.status == ObjectApplyStatus::Failed
                    && result.rolled_back
                    && result.presence == ObjectPresence::Present,
                "Cancellation outcome {result:?}"
            );
        }
        timeout(Duration::from_secs(10), commit).await???;
        ensure!(
            timeout(Duration::from_secs(10), query).await??? == "1",
            "Queued query did not see restored original"
        );
        ensure!(
            !db.in_transaction().await?,
            "Cancellation leaked transaction"
        );
        ensure!(
            scalar(&observer, &format!("SELECT value FROM {ns}.mv")).await? == "1",
            "Ordinary commit persisted partial DROP"
        );
        exec(&observer, &format!("SELECT pg_advisory_unlock({key})")).await?;
    }
    // Success remains a real committed outcome, observed by another connection.
    let edit = plan_edit(
        &snapshot(&db, &ns).await?,
        ObjectPatch::MaterializedQuery {
            query: "SELECT 3 AS value".into(),
        },
    )?;
    let result = apply_object_edit(&db, &edit).await?;
    ensure!(
        result.status == ObjectApplyStatus::Applied && !result.rolled_back,
        "{result:?}"
    );
    ensure!(scalar(&observer, &format!("SELECT value FROM {ns}.mv")).await? == "3");
    exec(&db, &format!("DROP SCHEMA {ns} CASCADE")).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "Task-owned PG15435 fixtures"]
async fn pg_commit_boundary_awaits_real_commit_and_reports_deferred_failure() -> Result<()> {
    let db = connect(Engine::Postgres).await?;
    let observer = connect(Engine::Postgres).await?;
    let ns = unique();
    let pid = scalar(&db, "SELECT pg_backend_pid()").await?;
    let key: i64 = pid.parse()?;
    exec(&db,&format!("CREATE SCHEMA {ns}; CREATE TABLE {ns}.events(id INTEGER); CREATE FUNCTION {ns}.enqueue() RETURNS INTEGER LANGUAGE plpgsql AS $b$BEGIN INSERT INTO {ns}.events VALUES(1); RETURN 1; END$b$; CREATE FUNCTION {ns}.on_commit() RETURNS trigger LANGUAGE plpgsql AS $b$BEGIN PERFORM pg_advisory_xact_lock({key}); RETURN NEW; END$b$; CREATE CONSTRAINT TRIGGER deferred_barrier AFTER INSERT ON {ns}.events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {ns}.on_commit()")).await?;
    exec(&observer, &format!("SELECT pg_advisory_lock({key})")).await?;
    let mut operation = db.administrative_operation(None).await?;
    let monitor = operation.monitor();
    operation
        .execute(&format!(
            "CREATE TABLE {ns}.committed AS SELECT {ns}.enqueue() AS id"
        ))
        .await?;
    let finishing = tokio::spawn(async move { operation.finish().await });
    wait_blocked(&observer, &pid).await?;
    ensure!(
        scalar(
            &observer,
            &format!("SELECT query FROM pg_stat_activity WHERE pid={pid}")
        )
        .await?
            == "COMMIT",
        "Barrier did not run inside COMMIT"
    );
    // Once COMMIT is sent, dropping the caller and Database::cancel must await
    // its real result rather than claiming rollback while the backend commits.
    finishing.abort();
    ensure!(finishing.await.unwrap_err().is_cancelled());
    db.cancel().await?;
    let d = db.clone();
    let queued = tokio::spawn(async move { d.commit().await });
    sleep(Duration::from_millis(30)).await;
    ensure!(!queued.is_finished());
    exec(&observer, &format!("SELECT pg_advisory_unlock({key})")).await?;
    let result = timeout(Duration::from_secs(10), monitor.wait()).await??;
    ensure!(
        result.status == AdministrativeStatus::Committed && !result.rolled_back,
        "{result:?}"
    );
    timeout(Duration::from_secs(10), queued).await???;
    ensure!(scalar(&observer, &format!("SELECT count(*) FROM {ns}.committed")).await? == "1");
    exec(&db,&format!("CREATE TABLE {ns}.parent(id INTEGER PRIMARY KEY); CREATE TABLE {ns}.child(id INTEGER REFERENCES {ns}.parent DEFERRABLE INITIALLY DEFERRED); CREATE FUNCTION {ns}.violate() RETURNS INTEGER LANGUAGE plpgsql AS $b$BEGIN INSERT INTO {ns}.child VALUES(100); RETURN 2; END$b$; CREATE MATERIALIZED VIEW {ns}.mv AS SELECT 1 AS value")).await?;
    let edit = plan_edit(
        &snapshot(&db, &ns).await?,
        ObjectPatch::MaterializedQuery {
            query: format!("SELECT {ns}.violate() AS value"),
        },
    )?;
    let result = apply_object_edit(&db, &edit).await?;
    ensure!(
        result.status == ObjectApplyStatus::Failed && result.rolled_back && result.error.is_some(),
        "Restricted materialized execution failure was misreported: {result:?}"
    );
    ensure!(
        scalar(&observer, &format!("SELECT value FROM {ns}.mv")).await? == "1",
        "Failed replacement lost original"
    );
    let mut operation = db.administrative_operation(None).await?;
    operation
        .execute(&format!(
            "CREATE TABLE {ns}.not_committed AS SELECT {ns}.violate() AS id"
        ))
        .await?;
    let result = operation.finish().await?;
    ensure!(
        result.status == AdministrativeStatus::OutcomeUnknown
            && !result.rolled_back
            && result.failed_step.is_none()
            && result.error.is_some(),
        "Deferred COMMIT failure was misreported: {result:?}"
    );
    ensure!(scalar(&observer,&format!("SELECT count(*) FROM information_schema.tables WHERE table_schema='{ns}' AND table_name='not_committed'")).await?=="0","Deferred COMMIT failure persisted new table");
    ensure!(!db.in_transaction().await?);
    exec(&db, &format!("DROP SCHEMA {ns} CASCADE")).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "Task-owned PG15435/MySQL13309 fixtures"]
async fn exclusive_primitive_drop_error_preconditions_and_mysql_partial_outcome() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let db = connect(engine).await?;
        let ns = unique();
        exec(
            &db,
            &format!("CREATE SCHEMA {ns}; CREATE TABLE {ns}.t(id INTEGER)"),
        )
        .await?;
        db.begin().await?;
        ensure!(
            db.administrative_operation(None).await.is_err(),
            "Adopted caller transaction"
        );
        ensure!(
            db.in_transaction().await?,
            "Preflight rolled caller transaction back"
        );
        db.rollback().await?;
        if engine == Engine::Postgres {
            db.begin().await?;
            ensure!(db.query("SELECT 1/0", 1).await.is_err());
            ensure!(db.administrative_operation(None).await.is_err());
            ensure!(
                db.in_transaction().await?,
                "Failed caller transaction was lost"
            );
            db.rollback().await?;
        } else {
            exec(&db, "SET autocommit=0").await?;
            ensure!(db.administrative_operation(None).await.is_err());
            exec(&db, "SET autocommit=1").await?;
        }
        let mut operation = db.administrative_operation(None).await?;
        let monitor = operation.monitor();
        operation.execute(&format!("DROP TABLE {ns}.t")).await?;
        // Pause exactly between statements. External COMMIT and SELECT cannot
        // observe or commit a half-finished operation, even with no query busy.
        let d = db.clone();
        let commit = tokio::spawn(async move { d.commit().await });
        let d = db.clone();
        let query = tokio::spawn(async move { d.query("SELECT 1", 1).await });
        sleep(Duration::from_millis(30)).await;
        ensure!(!commit.is_finished() && !query.is_finished());
        drop(operation);
        let result = timeout(Duration::from_secs(10), monitor.wait()).await??;
        ensure!(result.completed_steps == 1 && result.reusable, "{result:?}");
        if engine == Engine::Postgres {
            ensure!(
                result.status == AdministrativeStatus::RolledBack && result.rolled_back,
                "{result:?}"
            );
        } else {
            ensure!(
                result.status == AdministrativeStatus::FailedNonAtomic && !result.rolled_back,
                "{result:?}"
            );
        }
        timeout(Duration::from_secs(10), commit).await???;
        timeout(Duration::from_secs(10), query).await???;
        ensure!(!db.in_transaction().await?);
        let count=scalar(&db,&format!("SELECT count(*) FROM information_schema.tables WHERE table_schema='{ns}' AND table_name='t'")).await?;
        ensure!(
            count == if engine == Engine::Postgres { "1" } else { "0" },
            "Wrong partial DDL state"
        );
        if engine == Engine::MySql {
            exec(&db, &format!("CREATE TABLE {ns}.t(id INTEGER)")).await?;
        }
        let mut operation = db.administrative_operation(None).await?;
        operation
            .execute(&format!("ALTER TABLE {ns}.t ADD COLUMN value INTEGER"))
            .await?;
        ensure!(
            operation.execute("COMMIT").await.is_err(),
            "Primitive accepted caller transaction control"
        );
        let outcome = operation.finish().await?;
        ensure!(
            !matches!(
                outcome.status,
                AdministrativeStatus::Committed | AdministrativeStatus::CompletedNonAtomic
            ),
            "Failed transaction misreported committed: {outcome:?}"
        );
        if engine == Engine::Postgres {
            ensure!(outcome.rolled_back);
        }
        // A dropped request future, with its lease handle still alive, cancels
        // the owned transaction and unblocks ordinary session work.
        let operation = db.administrative_operation(None).await?;
        let monitor = operation.monitor();
        let sql = if engine == Engine::Postgres {
            "SELECT pg_sleep(30)"
        } else {
            "SELECT SLEEP(30)"
        };
        ensure!(
            timeout(Duration::from_millis(50), operation.query(sql, 1))
                .await
                .is_err()
        );
        let outcome = timeout(Duration::from_secs(10), monitor.wait()).await??;
        ensure!(outcome.reusable, "{outcome:?}");
        drop(operation);
        ensure!(!db.in_transaction().await?);
        ensure!(scalar(&db, "SELECT 1").await? == "1");
        exec(
            &db,
            &format!(
                "DROP SCHEMA {ns}{}",
                if engine == Engine::Postgres {
                    " CASCADE"
                } else {
                    ""
                }
            ),
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "Task-owned MySQL13309 fixtures"]
async fn mysql_binary_defaults_and_temporary_shadow_are_blocked_through_public_apis() -> Result<()>
{
    let db = connect(Engine::MySql).await?;
    let ns = unique();
    let source = format!("{ns}s");
    exec(&db,&format!("CREATE SCHEMA {ns}; CREATE SCHEMA {source}; CREATE TABLE {ns}.binary_default(v VARBINARY(20) DEFAULT X'00FF'); CREATE TABLE {ns}.t(v VARBINARY(20)); CREATE TABLE {source}.t(v VARBINARY(20))")).await?;
    let binary = TableRef {
        schema: ns.clone(),
        name: "binary_default".into(),
    };
    ensure!(
        mysql_column_definition(&db, &binary, "v").await.is_err(),
        "Legacy loader textified a binary default"
    );
    ensure!(
        inspect_mysql_column(&db, &binary, "v").await.is_err(),
        "Advanced loader textified a binary default"
    );
    exec(&db, &format!("INSERT INTO {ns}.binary_default() VALUES() ")).await?;
    ensure!(scalar(&db, &format!("SELECT HEX(v) FROM {ns}.binary_default")).await? == "00FF");
    let table = TableRef {
        schema: ns.clone(),
        name: "t".into(),
    };
    let original = inspect_mysql_column(&db, &table, "v").await?;
    let mut desired = original.definition().clone();
    desired.base.comment = "comment-only".into();
    let edit = plan_mysql_column_edit(&original, desired)?;
    let mapping = NamespaceMapping {
        source: source.clone(),
        destination: ns.clone(),
    };
    let comparison = compare_mysql_columns(&db, &db, &mapping, "t").await?;
    exec(
        &db,
        &format!(
            "CREATE TEMPORARY TABLE {ns}.t(v VARCHAR(20)); INSERT INTO {ns}.t VALUES('keep text')"
        ),
    )
    .await?;
    ensure!(
        mysql_column_definition(&db, &table, "v").await.is_err(),
        "Legacy loader accepted temporary shadow"
    );
    ensure!(
        inspect_mysql_column(&db, &table, "v").await.is_err(),
        "Advanced loader accepted mixed permanent/temporary metadata"
    );
    ensure!(
        mysql_column_is_current(&db, &edit).await.is_err(),
        "Stale check accepted temporary effective target"
    );
    ensure!(
        columns_destination_is_current(&db, &comparison)
            .await
            .is_err(),
        "Comparison freshness accepted temporary target"
    );
    ensure!(snapshot_columns(&db, &table).await.is_err());
    ensure!(
        compare_mysql_columns(&db, &db, &mapping, "t")
            .await
            .is_err()
    );
    ensure!(scalar(&db, &format!("SELECT v FROM {ns}.t")).await? == "keep text");
    exec(&db, &format!("DROP TEMPORARY TABLE {ns}.t")).await?;
    ensure!(mysql_column_is_current(&db, &edit).await?);
    exec(&db, &format!("DROP SCHEMA {ns}; DROP SCHEMA {source}")).await?;
    Ok(())
}
