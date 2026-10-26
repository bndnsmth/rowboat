//! Explicit task-owned Rowboat base fixtures only; no discovered connections.
use anyhow::{Result, ensure};
use rowboat_core::{ConnectionProfile, Database, Engine};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
    task::JoinSet,
    time::{sleep, timeout},
};

async fn connect(engine: Engine, read_only: bool) -> Result<Database> {
    Database::connect(
        &ConnectionProfile {
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
            read_only,
            ..Default::default()
        },
        "rowboat-test",
    )
    .await
}
async fn scalar(db: &Database, sql: &str) -> Result<String> {
    Ok(db.query(sql, 1).await?[0].rows[0][0].display())
}
async fn reusable(db: &Database) -> Result<()> {
    ensure!(timeout(Duration::from_secs(10), scalar(db, "SELECT 42")).await?? == "42");
    ensure!(!db.in_transaction().await?);
    Ok(())
}
fn unique() -> String {
    format!(
        "pass12_core_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[tokio::test]
#[ignore = "Rowboat PostgreSQL15435/MySQL13309"]
async fn bounded_prepared_results_are_complete_or_errors() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        for read_only in [false, true] {
            let db = connect(engine, read_only).await?;
            let results = db
                .sample_readonly(
                    "WITH n AS (SELECT 12345678901234567890.123456789 AS n) SELECT n FROM n",
                )
                .await?;
            ensure!(results.len() == 1 && results[0].columns.len() == 1);
            ensure!(
                results[0].rows[0][0].display().trim_end_matches('0')
                    == "12345678901234567890.123456789"
            );
            ensure!(!results[0].truncated);
            let empty = db.sample_readonly("SELECT 1 AS n WHERE 1=0").await?;
            ensure!(empty[0].columns.len() == 1 && empty[0].rows.is_empty() && !empty[0].truncated);
            ensure!(
                db.sample_readonly("SELECT 1 UNION ALL SELECT 2").await?[0]
                    .rows
                    .len()
                    == 2
            );
            for (sql, message) in [
                ("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3", "two rows"),
                ("SELECT 1, 2", "one column"),
                ("SELECT repeat('x',65537)", "64 KiB"),
                (
                    "SELECT repeat('x',33000) UNION ALL SELECT repeat('y',33000)",
                    "64 KiB",
                ),
            ] {
                let error = db.sample_readonly(sql).await.unwrap_err();
                ensure!(
                    format!("{error:#}").contains(message),
                    "{engine:?}: {error:#}"
                );
                reusable(&db).await?;
            }
            ensure!(
                db.sample_readonly("SELECT repeat('x',65000) AS n").await?[0].rows[0][0]
                    .display()
                    .len()
                    == 65000
            );
            if engine == Engine::MySql {
                // The server can interrupt SLEEP with a successful row (1).
                // A shorter caller hint deterministically exercises that path.
                let error = db
                    .sample_readonly("SELECT /*+ MAX_EXECUTION_TIME(1) */ SLEEP(1)")
                    .await
                    .unwrap_err();
                ensure!(format!("{error:#}").contains("timeout"), "{error:#}");
            }
            reusable(&db).await?;
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "Rowboat PostgreSQL15435/MySQL13309"]
async fn sampling_rejects_controls_and_preserves_actual_transactions() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let db = connect(engine, false).await?;
        for sql in [
            "",
            "BEGIN",
            "SHOW VARIABLES",
            "EXPLAIN SELECT 1",
            "SELECT 1; SELECT 2",
            "SELECT 1 -- hidden\n; COMMIT",
            "SELECT 1; START TRANSACTION READ WRITE",
            "SELECT 1 INTO ignored",
            "SELECT 1 FOR UPDATE",
            "WITH n AS (SELECT 1) DELETE FROM ignored",
            "SELECT 1 /* unterminated",
            "SELECT 1\0;COMMIT",
        ] {
            ensure!(db.sample_readonly(sql).await.is_err(), "{engine:?}: {sql}");
        }
        if engine == Engine::MySql {
            ensure!(
                db.sample_readonly("SELECT 1 /*!80000 INTO @escaped */")
                    .await
                    .is_err()
            );
            db.query("SET SESSION autocommit=0", 0).await?;
            let error = db.sample_readonly("SELECT 1").await.unwrap_err();
            ensure!(format!("{error:#}").contains("autocommit=1"));
            ensure!(scalar(&db, "SELECT @@session.autocommit").await? == "0");
            db.query("ROLLBACK; SET SESSION autocommit=1", 0).await?;
        }
        db.query("START TRANSACTION", 0).await?;
        ensure!(
            db.sample_readonly("SELECT 1")
                .await
                .unwrap_err()
                .to_string()
                .contains("transaction")
        );
        ensure!(db.in_transaction().await?);
        ensure!(scalar(&db, "SELECT 42").await? == "42");
        if engine == Engine::Postgres {
            ensure!(db.query("SELECT 1/0", 1).await.is_err());
            ensure!(
                db.sample_readonly("SELECT 1")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("transaction")
            );
            ensure!(db.in_transaction().await?);
            ensure!(db.query("SELECT 42", 1).await.is_err());
        }
        db.rollback().await?;
        reusable(&db).await?;
        ensure!(
            db.sample_readonly("SELECT 'COMMIT; SET x=1' AS n /* harmless */")
                .await?[0]
                .rows
                .len()
                == 1
        );
    }
    Ok(())
}

async fn wait_sampling(observer: &Database, engine: Engine, id: &str) -> Result<()> {
    let sql = if engine == Engine::Postgres {
        format!(
            "SELECT count(*) FROM pg_stat_activity WHERE pid={id} AND state='active' AND query LIKE 'SELECT pg_sleep%'"
        )
    } else {
        format!(
            "SELECT count(*) FROM information_schema.PROCESSLIST WHERE ID={id} AND INFO LIKE 'SELECT SLEEP%'"
        )
    };
    timeout(Duration::from_secs(3), async {
        loop {
            if scalar(observer, &sql).await? == "1" {
                return Ok::<(), anyhow::Error>(());
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await??;
    Ok(())
}

#[tokio::test]
#[ignore = "Rowboat PostgreSQL15435/MySQL13309"]
async fn busy_drop_cancel_and_timeout_keep_exclusive_cleanup() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let db = connect(engine, false).await?;
        let observer = connect(engine, false).await?;
        let lease = db.administrative_operation(None).await?;
        let started = Instant::now();
        let error = timeout(Duration::from_millis(100), db.sample_readonly("SELECT 1"))
            .await?
            .unwrap_err();
        ensure!(
            error.to_string().contains("busy") && started.elapsed() < Duration::from_millis(100)
        );
        lease.finish().await?;
        let id = scalar(
            &db,
            if engine == Engine::Postgres {
                "SELECT pg_backend_pid()"
            } else {
                "SELECT CONNECTION_ID()"
            },
        )
        .await?;
        let sql = if engine == Engine::Postgres {
            "SELECT pg_sleep(20)"
        } else {
            "SELECT SLEEP(20)"
        };
        for cancel in [false, true] {
            let worker = db.clone();
            let task = tokio::spawn(async move { worker.sample_readonly(sql).await });
            wait_sampling(&observer, engine, &id).await?;
            let error = timeout(Duration::from_millis(100), db.sample_readonly("SELECT 1"))
                .await?
                .unwrap_err();
            ensure!(error.to_string().contains("busy"));
            if cancel {
                db.cancel().await?;
                ensure!(timeout(Duration::from_secs(10), task).await??.is_err());
            } else {
                task.abort();
                ensure!(task.await.unwrap_err().is_cancelled());
            }
            reusable(&db).await?;
        }
        let start = Instant::now();
        let error = timeout(Duration::from_secs(12), db.sample_readonly(sql))
            .await?
            .unwrap_err();
        ensure!(
            start.elapsed() >= Duration::from_secs(4) && start.elapsed() < Duration::from_secs(10)
        );
        ensure!(
            format!("{error:#}").contains("five seconds")
                || format!("{error:#}").contains("timeout"),
            "{error:#}"
        );
        reusable(&db).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "Rowboat PostgreSQL15435/MySQL13309"]
async fn server_readonly_blocks_function_writes_and_restores_session_defaults() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let db = connect(engine, false).await?;
        let name = unique();
        db.query(&format!("CREATE SCHEMA {name}"), 0).await?;
        let result: Result<()> = async {
            db.query(&format!("CREATE TABLE {name}.counter (n int)"), 0).await?;
            let function = if engine == Engine::Postgres {
                format!("CREATE FUNCTION {name}.write_attempt() RETURNS int LANGUAGE plpgsql AS $$ BEGIN INSERT INTO {name}.counter VALUES(1); RETURN 1; END $$")
            } else {
                format!("CREATE FUNCTION {name}.write_attempt() RETURNS int DETERMINISTIC MODIFIES SQL DATA BEGIN INSERT INTO {name}.counter VALUES(1); RETURN 1; END")
            };
            db.query(&function, 0).await?;
            ensure!(db.sample_readonly(&format!("SELECT {name}.write_attempt()")).await.is_err());
            ensure!(scalar(&db, &format!("SELECT count(*) FROM {name}.counter")).await? == "0");
            // A writable profile must remain writable after both error and success.
            db.query(&format!("INSERT INTO {name}.counter VALUES(2)"), 0).await?;
            let settings = if engine == Engine::Postgres {
                "SELECT current_setting('default_transaction_read_only') || ':' || current_setting('standard_conforming_strings') || ':' || current_setting('statement_timeout') || ':' || current_setting('bytea_output')"
            } else {
                "SELECT concat(@@session.transaction_read_only,':',@@session.sql_mode,':',@@session.max_execution_time,':',@@session.completion_type)"
            };
            if engine == Engine::Postgres {
                db.query("SET standard_conforming_strings=off; SET bytea_output='escape'; SET statement_timeout='7s'", 0).await?;
                db.query(&format!("CREATE FUNCTION {name}.mode_attempt() RETURNS int LANGUAGE plpgsql AS $$ BEGIN PERFORM set_config('standard_conforming_strings','off',false); PERFORM set_config('default_transaction_read_only','on',false); RETURN 1; END $$"), 0).await?;
            } else {
                db.query("SET SESSION sql_mode='NO_BACKSLASH_ESCAPES,ANSI_QUOTES'; SET SESSION max_execution_time=7000; SET SESSION completion_type=1", 0).await?;
                db.query(&format!("CREATE FUNCTION {name}.mode_attempt() RETURNS int NO SQL BEGIN SET SESSION max_execution_time=9000; RETURN 1; END"), 0).await?;
            }
            let before = scalar(&db, settings).await?;
            ensure!(db.sample_readonly(&format!("SELECT {name}.mode_attempt()")).await?[0].rows[0][0].display() == "1");
            ensure!(scalar(&db, settings).await? == before);
            let literal = r"SELECT '\n' AS n";
            ensure!(db.sample_readonly(literal).await?[0].rows[0][0].display() == r"\n");
            if engine == Engine::MySql {
                db.query("SET SESSION sql_mode=''", 0).await?;
                ensure!(db.sample_readonly(literal).await?[0].rows[0][0].display() == "\n");
                db.query("SET SESSION sql_mode='NO_BACKSLASH_ESCAPES,ANSI_QUOTES'", 0).await?;
                ensure!(db.sample_readonly(literal).await?[0].rows[0][0].display() == r"\n");
            }
            reusable(&db).await?;
            db.query(&format!("INSERT INTO {name}.counter VALUES(3)"), 0).await?;
            Ok(())
        }.await;
        // Avoid the deliberately configured MySQL completion mode in cleanup.
        if engine == Engine::MySql {
            db.query("SET SESSION completion_type=0", 0).await?;
        }
        db.query(
            &format!(
                "DROP SCHEMA {name}{}",
                if engine == Engine::Postgres {
                    " CASCADE"
                } else {
                    ""
                }
            ),
            0,
        )
        .await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "Rowboat PostgreSQL15435/MySQL13309"]
async fn lost_backend_retires_session_instead_of_publishing_a_sample() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let db = connect(engine, false).await?;
        let observer = connect(engine, false).await?;
        let id = scalar(
            &db,
            if engine == Engine::Postgres {
                "SELECT pg_backend_pid()"
            } else {
                "SELECT CONNECTION_ID()"
            },
        )
        .await?;
        let sql = if engine == Engine::Postgres {
            "SELECT pg_sleep(20)"
        } else {
            "SELECT SLEEP(20)"
        };
        let worker = db.clone();
        let task = tokio::spawn(async move { worker.sample_readonly(sql).await });
        wait_sampling(&observer, engine, &id).await?;
        observer
            .query(
                &if engine == Engine::Postgres {
                    format!("SELECT pg_terminate_backend({id})")
                } else {
                    format!("KILL CONNECTION {id}")
                },
                1,
            )
            .await?;
        let error = timeout(Duration::from_secs(10), task).await??.unwrap_err();
        ensure!(error.to_string().contains("session retired"), "{error:#}");
        ensure!(
            timeout(Duration::from_secs(2), db.query("SELECT 1", 1))
                .await?
                .is_err()
        );
        ensure!(db.sample_readonly("SELECT 1").await.is_err());
    }
    Ok(())
}

struct PreflightGate {
    armed: AtomicBool,
    entered: Notify,
    release: Notify,
    connections: AtomicUsize,
    sample_seen: AtomicBool,
    readonly_begin_seen: AtomicBool,
}
struct PreflightProxy {
    port: u16,
    gate: Arc<PreflightGate>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for PreflightProxy {
    fn drop(&mut self) {
        self.gate.release.notify_one();
        // The accept task owns a JoinSet: aborting it also drops/aborts every
        // forwarding task and closes its sockets, including assertion failures.
        self.task.abort();
    }
}
impl PreflightProxy {
    async fn new(engine: Engine) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let gate = Arc::new(PreflightGate {
            armed: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
            connections: AtomicUsize::new(0),
            sample_seen: AtomicBool::new(false),
            readonly_begin_seen: AtomicBool::new(false),
        });
        let shared = gate.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut client, _)) = accepted else { break };
                        let primary = shared.connections.fetch_add(1, Ordering::SeqCst) == 0;
                        let gate = shared.clone();
                        connections.spawn(async move {
                            let mut server = TcpStream::connect(("127.0.0.1", if engine == Engine::Postgres {15435} else {13309})).await?;
                            if !primary {
                                tokio::io::copy_bidirectional(&mut client, &mut server).await?;
                                return Ok::<(), anyhow::Error>(());
                            }
                            let (mut cr, mut cw) = client.into_split();
                            let (mut sr, mut sw) = server.into_split();
                            let outgoing = async {
                                let mut startup = engine == Engine::Postgres;
                                loop {
                                    // Frame the native client protocol so matching is
                                    // independent of TCP chunk/packet boundaries.
                                    let mut header = vec![0; if engine == Engine::Postgres && !startup {5} else {4}];
                                    if let Err(error) = cr.read_exact(&mut header).await {
                                        if error.kind() == std::io::ErrorKind::UnexpectedEof {return Ok::<(), anyhow::Error>(());}
                                        return Err(error.into());
                                    }
                                    let payload_len = if engine == Engine::Postgres {
                                        let offset = usize::from(!startup);
                                        let length = u32::from_be_bytes(header[offset..offset+4].try_into().unwrap()) as usize;
                                        length.checked_sub(4).ok_or_else(|| anyhow::anyhow!("Invalid PostgreSQL test frame"))?
                                    } else {
                                        usize::from(header[0]) | usize::from(header[1]) << 8 | usize::from(header[2]) << 16
                                    };
                                    ensure!(payload_len <= 1024 * 1024, "Unexpectedly large test protocol frame");
                                    let mut payload = vec![0; payload_len];
                                    cr.read_exact(&mut payload).await?;
                                    startup = false;
                                    let contains = |needle: &[u8]| payload.windows(needle.len()).any(|w| w == needle);
                                    if contains(b"314159") {gate.sample_seen.store(true, Ordering::SeqCst);}
                                    if contains(b"START TRANSACTION READ ONLY") {gate.readonly_begin_seen.store(true, Ordering::SeqCst);}
                                    let probe: &[u8] = if engine == Engine::Postgres {b"SAVEPOINT rowboat_probe_"} else {b"SELECT @@session.autocommit"};
                                    if contains(probe) && gate.armed.swap(false, Ordering::SeqCst) {
                                        gate.entered.notify_one();
                                        gate.release.notified().await;
                                    }
                                    sw.write_all(&header).await?;
                                    sw.write_all(&payload).await?;
                                }
                            };
                            tokio::select! {
                                result = outgoing => result?,
                                result = tokio::io::copy(&mut sr, &mut cw) => {result?;},
                            }
                            Ok(())
                        });
                    }
                    completed = connections.join_next(), if !connections.is_empty() => {
                        if let Some(Err(error)) = completed {eprintln!("preflight proxy task: {error}");}
                    }
                }
            }
        });
        Ok(Self { port, gate, task })
    }

    async fn database(&self, engine: Engine) -> Result<Database> {
        Database::connect(
            &ConnectionProfile {
                engine,
                host: "127.0.0.1".into(),
                port: self.port,
                username: if engine == Engine::Postgres {
                    "postgres"
                } else {
                    "root"
                }
                .into(),
                database: "rowboat".into(),
                ssl_mode: "disable".into(),
                ..Default::default()
            },
            "rowboat-test",
        )
        .await
    }
}

#[tokio::test]
#[ignore = "Rowboat PostgreSQL15435/MySQL13309; deterministic loopback preflight gate"]
async fn explicit_cancel_during_preflight_is_remembered_without_cancelling_caller_probe()
-> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        for state in ["idle", "active", "failed_or_autocommit"] {
            let proxy = PreflightProxy::new(engine).await?;
            let db = proxy.database(engine).await?;
            let table = unique();
            db.query(
                &format!(
                    "CREATE TEMPORARY TABLE {table}(n integer); INSERT INTO {table} VALUES(1)"
                ),
                0,
            )
            .await?;
            if state != "idle" {
                db.query(
                    &format!("START TRANSACTION; INSERT INTO {table} VALUES(2)"),
                    0,
                )
                .await?;
            }
            if state == "failed_or_autocommit" {
                if engine == Engine::Postgres {
                    ensure!(db.query("SELECT 1/0", 1).await.is_err());
                } else {
                    db.query("SET autocommit=0", 0).await?;
                }
            }
            let connections_before = proxy.gate.connections.load(Ordering::SeqCst);
            proxy.gate.armed.store(true, Ordering::SeqCst);
            let worker = db.clone();
            let sample = tokio::spawn(async move {
                worker
                    .sample_readonly("SELECT 314159 AS cancelled_preflight")
                    .await
            });
            timeout(Duration::from_secs(3), proxy.gate.entered.notified()).await?;
            ensure!(
                db.sample_readonly("SELECT 2")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("busy")
            );
            for _ in 0..2 {
                timeout(Duration::from_secs(1), db.cancel()).await??;
            }
            ensure!(
                !sample.is_finished(),
                "Cancellation interrupted the caller-state probe"
            );
            ensure!(
                proxy.gate.connections.load(Ordering::SeqCst) == connections_before,
                "{engine:?}/{state}: preflight cancel opened a backend cancellation connection"
            );
            proxy.gate.release.notify_one();
            let error = timeout(Duration::from_secs(8), sample).await??.unwrap_err();
            if state == "idle" {
                ensure!(error.to_string().contains("cancel"), "{error:#}");
            }
            ensure!(
                !proxy.gate.sample_seen.load(Ordering::SeqCst),
                "Cancelled sample SQL reached the server"
            );
            ensure!(
                !proxy.gate.readonly_begin_seen.load(Ordering::SeqCst),
                "Cancelled preflight began a transaction"
            );
            ensure!(
                proxy.gate.connections.load(Ordering::SeqCst) == connections_before,
                "{engine:?}/{state}: preflight cleanup sent a backend cancel"
            );
            ensure!(db.in_transaction().await? == (state != "idle"));
            if state == "failed_or_autocommit" && engine == Engine::Postgres {
                ensure!(
                    db.query("SELECT 1", 1).await.is_err(),
                    "Failed caller transaction was reset"
                );
            } else {
                ensure!(
                    scalar(&db, &format!("SELECT count(*) FROM {table}")).await?
                        == if state == "idle" { "1" } else { "2" },
                    "Caller work was rolled back"
                );
            }
            if state == "failed_or_autocommit" && engine == Engine::MySql {
                ensure!(scalar(&db, "SELECT @@session.autocommit").await? == "0");
            }
            db.query("ROLLBACK", 0).await?;
            if engine == Engine::MySql {
                db.query("SET autocommit=1", 0).await?;
            }
            ensure!(
                scalar(&db, &format!("SELECT count(*) FROM {table}")).await? == "1",
                "Caller work was committed"
            );
            ensure!(
                db.sample_readonly("SELECT 42").await?[0].rows[0][0].display() == "42",
                "Cancellation leaked to the next sample"
            );
            db.query(&format!("DROP TABLE {table}"), 0).await?;
        }
    }
    Ok(())
}
