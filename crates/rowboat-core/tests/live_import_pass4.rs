//! Atomic import regressions on explicit disposable Podman databases only.
use anyhow::{Context, Result, ensure};
use rowboat_core::*;
use std::time::Duration;

fn profile(engine: Engine) -> ConnectionProfile {
    ConnectionProfile {
        engine,
        host: "127.0.0.1".into(),
        port: if engine == Engine::Postgres {
            15435
        } else {
            13309
        },
        database: "rowboat".into(),
        username: if engine == Engine::Postgres {
            "postgres"
        } else {
            "rowboat"
        }
        .into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    }
}
async fn connect(engine: Engine) -> Result<Database> {
    Database::connect(&profile(engine), "rowboat-test").await
}
fn text(v: impl ToString) -> Cell {
    Cell::Text(v.to_string())
}
fn input(id: i32) -> ImportRow {
    vec![
        ("id".into(), text(id)),
        ("label".into(), text(format!("row {id}"))),
    ]
}
fn options() -> RowImportOptions {
    RowImportOptions {
        batch_rows: 2,
        batch_bytes: 4096,
        max_cell_bytes: 2048,
        channel_capacity: 1,
        ..Default::default()
    }
}
async fn count(db: &Database, name: &str) -> Result<u64> {
    let result = db.query(&format!("SELECT COUNT(*) FROM {name}"), 1).await?;
    result
        .first()
        .and_then(|r| r.rows.first())
        .and_then(|r| r.first())
        .context("count result missing")?
        .display()
        .parse()
        .map_err(Into::into)
}
fn table(engine: Engine, prefix: &str) -> TableRef {
    TableRef {
        schema: if engine == Engine::Postgres {
            "public"
        } else {
            "rowboat"
        }
        .into(),
        name: format!(
            "pass12_core_{prefix}_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ),
    }
}

async fn mysql_fixture_trigger_ddl(sql: &str) -> Result<()> {
    // MySQL binary logging requires SUPER to create triggers. Use only the
    // explicitly labelled disposable container's documented fixture root login
    // for this unique test trigger; never change server/global trust settings.
    let owned = tokio::process::Command::new("podman")
        .args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"io.rowboat.test\"}}",
            "rowboat-test-mysql",
        ])
        .output()
        .await?;
    ensure!(
        owned.status.success() && String::from_utf8_lossy(&owned.stdout).trim() == "true",
        "Refusing trigger fixture setup in an unowned container"
    );
    let output = tokio::process::Command::new("podman")
        .args([
            "exec",
            "-e",
            "MYSQL_PWD=rowboat-test",
            "rowboat-test-mysql",
            "mysql",
            "-u",
            "root",
            "-D",
            "rowboat",
            "-e",
            sql,
        ])
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "Creating/removing fixture trigger: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn atomic_batches(engine: Engine) -> Result<()> {
    let db = connect(engine).await?;
    let observer = connect(engine).await?;
    let table = table(engine, "p4_atomic");
    let name = sql::qualified_table(engine, &table);
    let binary = if engine == Engine::Postgres {
        "bytea"
    } else {
        "blob"
    };
    db.query(&format!("CREATE TABLE {name}(id integer PRIMARY KEY,label varchar(128),amount decimal(20,4),payload {binary})"),0).await?;
    let result = async {
        let mut sink = db.import_rows(&table, options()).await?;
        let mut first = input(1);
        first.push(("label".into(), Cell::Null));
        // Duplicate identifiers must fail atomically, not let the last value win.
        sink.send(vec![input(10)]).await?;
        let _ = sink.send(vec![first]).await;
        ensure!(
            sink.finish().await.is_err(),
            "duplicate import fields accepted"
        );
        ensure!(
            count(&observer, &name).await? == 0,
            "invalid row committed an earlier chunk"
        );

        let mut sink = db.import_rows(&table, options()).await?;
        let first = vec![
            ("id".into(), text(1)),
            ("label".into(), text("'; DROP TABLE important; --")),
            ("amount".into(), text("1234567890123456.7890")),
            ("payload".into(), Cell::Binary(vec![0, 255])),
        ];
        sink.send(vec![first, input(2)]).await?;
        ensure!(
            count(&observer, &name).await? == 0,
            "uncommitted first batch visible externally"
        );
        let queued_db = db.clone();
        let queued_name = name.clone();
        let mut competing = tokio::spawn(async move { count(&queued_db, &queued_name).await });
        ensure!(
            tokio::time::timeout(Duration::from_millis(80), &mut competing)
                .await
                .is_err(),
            "competing query interleaved inside import"
        );
        sink.send(vec![input(3)]).await?;
        ensure!(sink.finish().await? == 3, "wrong committed count");
        ensure!(
            competing.await?? == 3,
            "queued query did not observe completed import"
        );
        let check = observer
            .query(&format!("SELECT amount,payload FROM {name} WHERE id=1"), 1)
            .await?;
        ensure!(
            check[0].rows[0] == vec![text("1234567890123456.7890"), Cell::Binary(vec![0, 255])],
            "typed import lost decimal/binary data"
        );

        let mut failing = db.import_rows(&table, options()).await?;
        failing.send(vec![input(4), input(5)]).await?;
        let _ = failing.send(vec![input(6), input(1)]).await;
        ensure!(
            failing.finish().await.is_err(),
            "late duplicate key did not fail"
        );
        ensure!(
            count(&observer, &name).await? == 3,
            "late chunk failure committed earlier rows"
        );
        let mut unfinished = db.import_rows(&table, options()).await?;
        unfinished.send(vec![input(7)]).await?;
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(unfinished);
        ensure!(
            tokio::time::timeout(Duration::from_secs(5), count(&db, &name)).await?? == 3,
            "dropped producer did not rollback/drain"
        );
        ensure!(!db.in_transaction().await?, "drop left a transaction open");

        let mut idle = db
            .import_rows(
                &table,
                RowImportOptions {
                    idle_timeout_ms: 100,
                    ..options()
                },
            )
            .await?;
        idle.send(vec![input(8)]).await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
        ensure!(
            idle.finish().await.is_err(),
            "idle producer implicitly committed"
        );
        ensure!(
            count(&observer, &name).await? == 3,
            "idle producer left rows"
        );

        // A blocked INSERT and a full producer queue must be interruptible.
        observer.begin().await?;
        observer
            .query(&format!("DELETE FROM {name} WHERE id=1"), 0)
            .await?;
        let mut blocked = db
            .import_rows(
                &table,
                RowImportOptions {
                    batch_rows: 1,
                    ..options()
                },
            )
            .await?;
        let producer = tokio::spawn(async move {
            for id in [11, 1, 12, 13] {
                blocked.send(vec![input(id)]).await?;
            }
            blocked.finish().await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        db.cancel().await?;
        let stopped = tokio::time::timeout(Duration::from_secs(5), producer).await??;
        observer.rollback().await?;
        ensure!(stopped.is_err(), "blocked producer committed after cancel");
        ensure!(
            count(&db, &name).await? == 3,
            "cancelled blocked import left rows"
        );

        // Whatever side wins a Finish/cancel race, report the actual durable result.
        for id in 20..25 {
            let mut sink = db.import_rows(&table, options()).await?;
            sink.send(vec![input(id)]).await?;
            let finish = tokio::spawn(sink.finish());
            tokio::task::yield_now().await;
            db.cancel().await?;
            let outcome = finish.await?;
            let exists = count(&observer, &format!("{name} WHERE id={id}")).await?;
            ensure!(
                matches!((outcome, exists), (Ok(1), 1) | (Err(_), 0)),
                "commit/cancel reported an outcome inconsistent with durable rows"
            );
        }
        Ok(())
    }
    .await;
    let _ = observer.rollback().await;
    db.query(&format!("DROP TABLE IF EXISTS {name}"), 0).await?;
    result
}

async fn preflight_and_startup(engine: Engine) -> Result<()> {
    let db = connect(engine).await?;
    let blocker = connect(engine).await?;
    let target = table(engine, "p4_preflight");
    let name = sql::qualified_table(engine, &target);
    let view = TableRef {
        schema: target.schema.clone(),
        name: format!("{}_view", target.name),
    };
    let view_name = sql::qualified_table(engine, &view);
    let audit = TableRef {
        schema: target.schema.clone(),
        name: format!("{}_audit", target.name),
    };
    let audit_name = sql::qualified_table(engine, &audit);
    let function = format!(
        "{}.{}",
        sql::quote_ident(engine, &target.schema),
        sql::quote_ident(engine, &format!("{}_fn", target.name))
    );
    let trigger = sql::quote_ident(engine, &format!("{}_trigger", target.name));
    db.query(&format!("CREATE TABLE {name}(id integer PRIMARY KEY,label varchar(128)); CREATE VIEW {view_name} AS SELECT * FROM {name}; CREATE TABLE {audit_name}(id integer) {}",if engine==Engine::MySql{"ENGINE=MyISAM"}else{""}),0).await?;
    let result=async {
        let readonly=Database::connect(&ConnectionProfile{read_only:true,..profile(engine)},"rowboat-test").await?;
        ensure!(readonly.import_rows(&target,options()).await.is_err(),"readonly import accepted");
        ensure!(db.import_rows(&view,options()).await.is_err(),"view import accepted");
        db.begin().await?;
        ensure!(db.import_rows(&target,options()).await.is_err(),"caller transaction accepted");
        ensure!(db.in_transaction().await?,"preflight destroyed caller transaction");
        let _=db.query("SELECT * FROM rowboat_p4_missing_preflight",0).await;
        ensure!(db.import_rows(&target,options()).await.is_err(),"failed/existing transaction accepted");
        ensure!(db.in_transaction().await?,"failed caller transaction was rolled back");
        db.rollback().await?;
        if engine==Engine::MySql {
            ensure!(db.import_rows(&audit,options()).await.is_err(),"MyISAM import accepted");
            db.query("SET autocommit=0",0).await?;
            ensure!(db.import_rows(&target,options()).await.is_err(),"manual-commit session accepted");
            db.query("SET autocommit=1",0).await?;
            mysql_fixture_trigger_ddl(&format!("CREATE TRIGGER {trigger} AFTER INSERT ON {name} FOR EACH ROW INSERT INTO {audit_name}(id) VALUES(NEW.id)")).await?;
        } else {
            db.query(&format!("CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO {audit_name}(id) VALUES(NEW.id); RETURN NEW; END $$; CREATE TRIGGER {trigger} AFTER INSERT ON {name} FOR EACH ROW EXECUTE FUNCTION {function}()"),0).await?;
        }
        ensure!(db.import_rows(&target,options()).await.is_err(),"trigger-bearing target accepted");
        ensure!(count(&blocker,&audit_name).await?==0,"trigger guard allowed side effects");
        if engine==Engine::Postgres {db.query(&format!("DROP TRIGGER {trigger} ON {name}; DROP FUNCTION {function}()"),0).await?;}
        else {mysql_fixture_trigger_ddl(&format!("DROP TRIGGER {trigger}")).await?;}

        // Cancelling import_rows BEFORE it returns its owned handle must signal
        // a queued worker without cancelling the predecessor's query.
        let slow=db.clone();let predecessor=tokio::spawn(async move {slow.query(if engine==Engine::Postgres{"SELECT pg_sleep(0.4)"}else{"SELECT sleep(0.4)"},1).await});
        tokio::time::sleep(Duration::from_millis(40)).await;
        ensure!(tokio::time::timeout(Duration::from_millis(50),db.import_rows(&target,options())).await.is_err(),"startup unexpectedly bypassed busy session");
        ensure!(predecessor.await?.is_ok(),"aborted startup cancelled predecessor");
        ensure!(!db.in_transaction().await?,"queued startup left a transaction");

        // Hold only our fixture table's lock, then drop the startup future while
        // its worker is blocked in LOCK/metadata work. Reuse before unlocking
        // proves cancellation cleanup, not merely eventual lock release.
        if engine==Engine::Postgres {blocker.begin().await?;blocker.query(&format!("LOCK TABLE {name} IN SHARE MODE"),0).await?;}
        else {blocker.query(&format!("LOCK TABLES {name} WRITE"),0).await?;}
        ensure!(tokio::time::timeout(Duration::from_millis(150),db.import_rows(&target,options())).await.is_err(),"startup did not block on fixture metadata lock");
        let reused=tokio::time::timeout(Duration::from_secs(5),db.query("SELECT 42",1)).await;
        if engine==Engine::Postgres {blocker.rollback().await?;} else {blocker.query("UNLOCK TABLES",0).await?;}
        ensure!(reused??[0].rows[0][0]==text(42),"cancelled startup left mutex/protocol busy");
        ensure!(!db.in_transaction().await? && count(&db,&name).await?==0,"cancelled startup changed transaction/data state");
        Ok(())
    }.await;
    if engine == Engine::MySql {
        let _ = blocker.query("UNLOCK TABLES", 0).await;
        let _ = db.query("SET autocommit=1", 0).await;
    } else {
        let _ = blocker.rollback().await;
    }
    let _ = db.rollback().await;
    db.query(&format!("DROP VIEW IF EXISTS {view_name}; DROP TABLE IF EXISTS {name}; DROP TABLE IF EXISTS {audit_name}"),0).await?;
    if engine == Engine::Postgres {
        db.query(&format!("DROP FUNCTION IF EXISTS {function}()"), 0)
            .await?;
    }
    result
}

#[tokio::test]
#[ignore = "Explicit task-owned PostgreSQL on 15435"]
async fn postgres_atomic_import_batches_and_cancel() -> Result<()> {
    atomic_batches(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Explicit task-owned MySQL on 13309"]
async fn mysql_atomic_import_batches_and_cancel() -> Result<()> {
    atomic_batches(Engine::MySql).await
}
#[tokio::test]
#[ignore = "Explicit task-owned PostgreSQL on 15435"]
async fn postgres_import_preflight_and_cancelled_startup() -> Result<()> {
    preflight_and_startup(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Explicit task-owned MySQL on 13309"]
async fn mysql_import_preflight_and_cancelled_startup() -> Result<()> {
    preflight_and_startup(Engine::MySql).await
}

#[tokio::test]
#[ignore = "Explicit task-owned PostgreSQL on 15435"]
async fn postgres_import_commit_wins_late_cancel() -> Result<()> {
    let db = connect(Engine::Postgres).await?;
    let blocker = connect(Engine::Postgres).await?;
    let observer = connect(Engine::Postgres).await?;
    let table = table(Engine::Postgres, "p4_commit");
    let name = sql::qualified_table(Engine::Postgres, &table);
    db.query(&format!("CREATE TABLE {name}(id integer PRIMARY KEY,label integer UNIQUE DEFERRABLE INITIALLY DEFERRED)"),0).await?;
    let result = async {
        let pid = db.query("SELECT pg_backend_pid()", 1).await?[0].rows[0][0].display();
        blocker.begin().await?;
        blocker
            .query(&format!("INSERT INTO {name} VALUES(1,7)"), 0)
            .await?;
        let mut sink = db.import_rows(&table, options()).await?;
        sink.send(vec![vec![
            ("id".into(), text(2)),
            ("label".into(), text(7)),
        ]])
        .await?;
        let mut finish = tokio::spawn(sink.finish());
        let mut committing = false;
        for _ in 0..100 {
            let state = observer
                .query(
                    &format!(
                        "SELECT query FROM pg_stat_activity WHERE pid={pid} AND state='active'"
                    ),
                    1,
                )
                .await?;
            if state
                .first()
                .is_some_and(|r| r.rows.iter().any(|r| r[0] == text("COMMIT")))
            {
                committing = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        ensure!(committing, "did not reach deferred-constraint COMMIT wait");
        ensure!(
            tokio::time::timeout(Duration::from_millis(50), &mut finish)
                .await
                .is_err(),
            "COMMIT was not genuinely blocked"
        );
        db.cancel().await?; // too late: COMMIT owns the cancellation gate's phase
        blocker.rollback().await?;
        ensure!(
            tokio::time::timeout(Duration::from_secs(5), finish).await??? == 1,
            "successful COMMIT was reported as cancelled/rolled back"
        );
        ensure!(
            count(&observer, &name).await? == 1,
            "commit outcome differs from durable rows"
        );
        Ok(())
    }
    .await;
    let _ = blocker.rollback().await;
    db.query(&format!("DROP TABLE IF EXISTS {name}"), 0).await?;
    result
}

#[tokio::test]
#[ignore = "Explicit task-owned MySQL on 13309"]
async fn mysql_import_rejects_effective_temporary_shadow() -> Result<()> {
    let db = connect(Engine::MySql).await?;
    let observer = connect(Engine::MySql).await?;
    let target = table(Engine::MySql, "p4_shadow");
    let name = sql::qualified_table(Engine::MySql, &target);
    db.query(
        &format!("CREATE TABLE {name}(id integer PRIMARY KEY,label varchar(128)) ENGINE=InnoDB"),
        0,
    )
    .await?;
    let result = async {
        for engine in ["MyISAM", "InnoDB"] {
            db.query(
                &format!("CREATE TEMPORARY TABLE {name}(id integer PRIMARY KEY,label varchar(128)) ENGINE={engine}"),
                0,
            ).await?;
            let startup = db.import_rows(&target, options()).await;
            // Exercise the original late-error path if the preflight regresses:
            // MyISAM would retain the first insert despite failed Finish/ROLLBACK.
            let rejected = match startup {
                Err(error) => {
                    ensure!(error.to_string().contains("temporary"), "unexpected preflight error: {error:#}");
                    true
                }
                Ok(mut sink) => {
                    let _ = sink.send(vec![input(1), input(1)]).await;
                    let _ = sink.finish().await;
                    false
                }
            };
            ensure!(count(&db, &name).await? == 0, "temporary {engine} table received non-atomic rows");
            ensure!(count(&observer, &name).await? == 0, "shadowed permanent table changed");
            ensure!(rejected, "temporary {engine} shadow accepted before sending rows");
            ensure!(!db.in_transaction().await?, "rejected shadow left a transaction");
            db.query(&format!("DROP TEMPORARY TABLE {name}"), 0).await?;
        }
        // The same connection remains usable once the shadow is removed.
        let mut sink = db.import_rows(&target, options()).await?;
        sink.send(vec![input(2)]).await?;
        ensure!(sink.finish().await? == 1, "permanent target failed after removing shadow");
        ensure!(count(&observer, &name).await? == 1, "permanent insert was not committed");
        Ok(())
    }.await;
    let _ = db
        .query(&format!("DROP TEMPORARY TABLE IF EXISTS {name}"), 0)
        .await;
    observer
        .query(&format!("DROP TABLE IF EXISTS {name}"), 0)
        .await?;
    result
}
