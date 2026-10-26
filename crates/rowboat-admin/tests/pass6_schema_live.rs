use anyhow::{Context, Result, ensure};
use rowboat_admin::{
    live_diff::{NamespaceMapping, definitions::*},
    object_editor::{ObjectApplyStatus, ObjectIdentity, ObjectPresence, resolve_object},
    objects::ObjectKind,
};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine};
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
        "rb6s{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
async fn exec(db: &Database, sql: &str) -> Result<()> {
    db.query(sql, 100).await?;
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
async fn id(db: &Database, schema: &str, kind: ObjectKind, name: &str) -> Result<ObjectIdentity> {
    resolve_object(db, schema, kind, name, None, None).await
}
async fn compare(
    a: &Database,
    b: &Database,
    m: &NamespaceMapping,
    kind: ObjectKind,
    name: &str,
) -> Result<DefinitionComparison> {
    compare_definitions(
        a,
        &id(a, &m.source, kind, name).await?,
        b,
        &id(b, &m.destination, kind, name).await?,
        m,
        if kind == ObjectKind::View {
            Scope::ViewQuery
        } else {
            Scope::RoutineBody
        },
    )
    .await
}
async fn cleanup(db: &Database, m: &NamespaceMapping) -> Result<()> {
    let cascade = if db.engine() == Engine::Postgres {
        " CASCADE"
    } else {
        ""
    };
    exec(
        db,
        &format!(
            "DROP SCHEMA {}{cascade}; DROP SCHEMA {}{cascade}",
            m.source, m.destination
        ),
    )
    .await
}
async fn views(engine: Engine) -> Result<()> {
    let a = connect(engine).await?;
    let b = connect(engine).await?;
    let prefix = unique();
    let m = NamespaceMapping {
        source: format!("{prefix}a"),
        destination: format!("{prefix}b"),
    };
    for (schema, factor) in [(&m.source, 2), (&m.destination, 1)] {
        exec(&a,&format!("CREATE SCHEMA {schema}; CREATE TABLE {schema}.t(id INTEGER,value INTEGER,name VARCHAR(40)); INSERT INTO {schema}.t VALUES(1,10,'keep'),(2,-1,'filtered'); CREATE VIEW {schema}.v AS SELECT id,value*{factor} AS total,name AS label FROM {schema}.t WHERE value>0")).await?;
    }
    if engine == Engine::Postgres {
        exec(&b,&format!("COMMENT ON VIEW {}.v IS 'destination'; COMMENT ON COLUMN {}.v.label IS 'column'; GRANT SELECT ON {}.v TO PUBLIC",m.destination,m.destination,m.destination)).await?;
    } else {
        exec(&b,&format!("CREATE USER '{prefix}'@'localhost' ACCOUNT LOCK; GRANT SELECT(label) ON {}.v TO '{prefix}'@'localhost'",m.destination)).await?;
    }
    let c = compare(&a, &b, &m, ObjectKind::View, "v").await?;
    ensure!(
        !c.is_blocked() && !c.plan().is_empty(),
        "View preview blocked: {:?}",
        c.blockers()
    );
    ensure!(
        scalar(&b, &format!("SELECT total FROM {}.v", m.destination)).await? == "10",
        "Preview mutated destination"
    );
    let f = definitions_are_current(&a, &b, &c).await?;
    ensure!(f.source && f.destination);
    let result = apply_definition_comparison(&a, &b, &c).await?;
    ensure!(
        result.status == DefinitionApplyStatus::Applied,
        "{result:?}"
    );
    ensure!(scalar(&b, &format!("SELECT total FROM {}.v", m.destination)).await? == "20");
    if engine == Engine::Postgres {
        ensure!(
            scalar(
                &b,
                &format!("SELECT obj_description('{}.v'::regclass)", m.destination)
            )
            .await?
                == "destination"
        );
        ensure!(scalar(&b,&format!("SELECT count(*) FROM information_schema.table_privileges WHERE table_schema='{}' AND table_name='v' AND grantee='PUBLIC' AND privilege_type='SELECT'",m.destination)).await?=="1");
    } else {
        ensure!(scalar(&b,&format!("SELECT count(*) FROM mysql.columns_priv WHERE Db='{}' AND Table_name='v' AND Column_name='label' AND User='{prefix}' AND FIND_IN_SET('Select',Column_priv)>0",m.destination)).await?=="1","MySQL view column grant lost");
    }
    let converged = compare(&a, &b, &m, ObjectKind::View, "v").await?;
    ensure!(
        !converged.is_blocked() && converged.plan().is_empty(),
        "Definitions did not converge: {:?}",
        converged.blockers()
    );
    ensure!(
        apply_definition_comparison(&a, &b, &converged)
            .await?
            .status
            == DefinitionApplyStatus::Unchanged
    );
    // Both endpoints and referenced base-table metadata are immutable review
    // evidence, not just the text of the destination CREATE statement.
    exec(&a,&format!("CREATE OR REPLACE VIEW {}.v AS SELECT id,value*3 AS total,name AS label FROM {}.t WHERE value>0",m.source,m.source)).await?;
    let c = compare(&a, &b, &m, ObjectKind::View, "v").await?;
    exec(
        &a,
        &format!("ALTER TABLE {}.t ADD COLUMN changed INTEGER", m.source),
    )
    .await?;
    ensure!(
        apply_definition_comparison(&a, &b, &c).await?.status == DefinitionApplyStatus::StaleSource
    );
    exec(
        &a,
        &format!("ALTER TABLE {}.t DROP COLUMN changed", m.source),
    )
    .await?;
    let c = compare(&a, &b, &m, ObjectKind::View, "v").await?;
    exec(
        &b,
        &format!("ALTER TABLE {}.t ADD COLUMN changed INTEGER", m.destination),
    )
    .await?;
    ensure!(
        apply_definition_comparison(&a, &b, &c).await?.status
            == DefinitionApplyStatus::StaleDestination
    );
    exec(
        &b,
        &format!("ALTER TABLE {}.t DROP COLUMN changed", m.destination),
    )
    .await?;
    let c = compare(&a, &b, &m, ObjectKind::View, "v").await?;
    let reconnected = connect(engine).await?;
    ensure!(
        apply_definition_comparison(&reconnected, &b, &c)
            .await?
            .status
            == DefinitionApplyStatus::StaleSource
    );
    ensure!(
        timeout(
            Duration::from_secs(2),
            apply_definition_comparison(&a, &a, &c)
        )
        .await??
        .status
            == DefinitionApplyStatus::StaleDestination,
        "Aliased endpoint handles deadlocked"
    );
    exec(&a,&format!("CREATE OR REPLACE VIEW {}.v AS SELECT id,value*4 AS total,name AS label FROM {}.t WHERE value>0",m.source,m.source)).await?;
    ensure!(
        apply_definition_comparison(&a, &b, &c).await?.status == DefinitionApplyStatus::StaleSource
    );
    // Same physical session supports mapping without attempting a second lease.
    let same = compare(&b, &b, &m, ObjectKind::View, "v").await?;
    ensure!(
        timeout(
            Duration::from_secs(5),
            apply_definition_comparison(&b, &b, &same)
        )
        .await??
        .status
            == DefinitionApplyStatus::Applied
    );
    let dep_review = compare(&a, &b, &m, ObjectKind::View, "v").await?;
    exec(
        &b,
        &format!(
            "CREATE VIEW {}.dependent AS SELECT label FROM {}.v",
            m.destination, m.destination
        ),
    )
    .await?;
    ensure!(
        apply_definition_comparison(&a, &b, &dep_review)
            .await?
            .status
            == DefinitionApplyStatus::StaleDestination,
        "Added dependent view did not invalidate review"
    );
    let dep = compare(&a, &b, &m, ObjectKind::View, "v").await?;
    if engine == Engine::MySql {
        ensure!(
            dep.is_blocked() && dep.plan().is_empty(),
            "MySQL dependent view must block"
        );
    }
    exec(&b, &format!("DROP VIEW {}.dependent", m.destination)).await?;
    if engine == Engine::MySql {
        let c = compare(&a, &b, &m, ObjectKind::View, "v").await?;
        exec(
            &b,
            &format!(
                "CREATE TEMPORARY TABLE {}.t(id INTEGER,value INTEGER,name VARCHAR(40))",
                m.destination
            ),
        )
        .await?;
        ensure!(
            apply_definition_comparison(&a, &b, &c).await?.status
                == DefinitionApplyStatus::StaleDestination,
            "Temporary dependency shadow accepted"
        );
        let blocked = compare(&a, &b, &m, ObjectKind::View, "v").await?;
        ensure!(blocked.is_blocked());
        exec(&b, &format!("DROP TEMPORARY TABLE {}.t", m.destination)).await?;
    }
    exec(&a,&format!("CREATE OR REPLACE VIEW {}.v AS SELECT a.id,a.value*5 AS total,a.name AS label FROM {}.t a JOIN {}.t z ON z.id=a.id",m.source,m.source,m.source)).await?;
    let blocked = compare(&a, &b, &m, ObjectKind::View, "v").await?;
    ensure!(blocked.is_blocked() && blocked.plan().is_empty());
    ensure!(
        apply_definition_comparison(&a, &b, &blocked).await?.status
            == DefinitionApplyStatus::Blocked
    );
    ensure!(scalar(&b, &format!("SELECT total FROM {}.v", m.destination)).await? == "40");
    if engine == Engine::MySql {
        exec(&b, &format!("DROP USER '{prefix}'@'localhost'")).await?;
    }
    cleanup(&a, &m).await
}

async fn routines(engine: Engine) -> Result<()> {
    let db = connect(engine).await?;
    let prefix = unique();
    let m = NamespaceMapping {
        source: format!("{prefix}a"),
        destination: format!("{prefix}b"),
    };
    for (schema, n) in [(&m.source, 2), (&m.destination, 1)] {
        exec(&db, &format!("CREATE SCHEMA {schema}")).await?;
        let sql = if engine == Engine::Postgres {
            format!(
                "CREATE FUNCTION {schema}.f(x INTEGER) RETURNS INTEGER LANGUAGE sql AS 'SELECT x+{n}'"
            )
        } else {
            format!(
                "CREATE FUNCTION {schema}.f(x INTEGER) RETURNS INTEGER DETERMINISTIC SQL SECURITY INVOKER COMMENT 'keep' RETURN x+{n}"
            )
        };
        exec(&db, &sql).await?;
    }
    let grantee = prefix.clone();
    if engine == Engine::Postgres {
        exec(&db,&format!("COMMENT ON FUNCTION {}.f(INTEGER) IS 'keep'; GRANT EXECUTE ON FUNCTION {}.f(INTEGER) TO PUBLIC",m.destination,m.destination)).await?;
    } else {
        exec(&db,&format!("CREATE USER '{grantee}'@'localhost' ACCOUNT LOCK; GRANT EXECUTE ON FUNCTION {}.f TO '{grantee}'@'localhost' WITH GRANT OPTION",m.destination)).await?;
    }
    let c = compare(&db, &db, &m, ObjectKind::Function, "f").await?;
    ensure!(
        !c.is_blocked() && !c.plan().is_empty(),
        "Routine blocked: {:?}",
        c.blockers()
    );
    let result = apply_definition_comparison(&db, &db, &c).await?;
    ensure!(
        result.status == DefinitionApplyStatus::Applied,
        "{result:?}"
    );
    if engine == Engine::MySql {
        let native = result
            .object_outcome
            .as_ref()
            .context("Missing non-atomic result")?;
        ensure!(
            c.recovery_plan().is_some()
                && c.plan().statements.len() >= 2
                && native.status == ObjectApplyStatus::Applied
                && native.completed_statements == c.plan().statements.len()
                && !native.rolled_back
                && native.presence == ObjectPresence::Present,
            "Non-atomic evidence missing: {result:?}"
        );
    }
    ensure!(scalar(&db, &format!("SELECT {}.f(3)", m.destination)).await? == "5");
    let done = compare(&db, &db, &m, ObjectKind::Function, "f").await?;
    ensure!(
        !done.is_blocked() && done.plan().is_empty(),
        "Routine not converged: {:?}",
        done.blockers()
    );
    if engine == Engine::MySql {
        ensure!(scalar(&db,&format!("SELECT count(*) FROM mysql.procs_priv WHERE Db='{}' AND Routine_name='f' AND User='{grantee}' AND FIND_IN_SET('Execute',Proc_priv)>0 AND FIND_IN_SET('Grant',Proc_priv)>0",m.destination)).await?=="1");
    }
    let stale = compare(&db, &db, &m, ObjectKind::Function, "f").await?;
    if engine == Engine::Postgres {
        exec(
            &db,
            &format!(
                "REVOKE EXECUTE ON FUNCTION {}.f(INTEGER) FROM PUBLIC",
                m.source
            ),
        )
        .await?;
    } else {
        exec(
            &db,
            &format!(
                "GRANT EXECUTE ON FUNCTION {}.f TO '{grantee}'@'localhost'",
                m.source
            ),
        )
        .await?;
    }
    ensure!(
        apply_definition_comparison(&db, &db, &stale).await?.status
            == DefinitionApplyStatus::StaleSource,
        "Source grant change was not stale"
    );
    let stale = compare(&db, &db, &m, ObjectKind::Function, "f").await?;
    if engine == Engine::Postgres {
        exec(
            &db,
            &format!(
                "REVOKE EXECUTE ON FUNCTION {}.f(INTEGER) FROM PUBLIC",
                m.destination
            ),
        )
        .await?;
    } else {
        exec(
            &db,
            &format!(
                "REVOKE GRANT OPTION ON FUNCTION {}.f FROM '{grantee}'@'localhost'",
                m.destination
            ),
        )
        .await?;
    }
    ensure!(
        apply_definition_comparison(&db, &db, &stale).await?.status
            == DefinitionApplyStatus::StaleDestination,
        "Destination grant change was not stale"
    );
    if engine == Engine::Postgres {
        for (schema, n) in [(&m.source, 3), (&m.destination, 1)] {
            exec(&db,&format!("CREATE FUNCTION {schema}.pl(x INTEGER) RETURNS INTEGER LANGUAGE plpgsql AS 'BEGIN RETURN x*{n}; END'")).await?;
        }
        let c = compare(&db, &db, &m, ObjectKind::Function, "pl").await?;
        ensure!(!c.is_blocked(), "{:?}", c.blockers());
        ensure!(
            apply_definition_comparison(&db, &db, &c).await?.status
                == DefinitionApplyStatus::Applied
        );
        ensure!(scalar(&db, &format!("SELECT {}.pl(4)", m.destination)).await? == "12");
        exec(&db,&format!("CREATE OR REPLACE FUNCTION {}.f(x INTEGER) RETURNS INTEGER LANGUAGE sql AS 'SELECT abs(x)'",m.source)).await?;
    } else {
        for (schema, n) in [(&m.source, 3), (&m.destination, 1)] {
            exec(&db,&format!("CREATE PROCEDURE {schema}.p(IN x INTEGER,OUT y INTEGER) SQL SECURITY INVOKER BEGIN SET y=x*{n}; END")).await?;
        }
        let c = compare(&db, &db, &m, ObjectKind::Procedure, "p").await?;
        ensure!(!c.is_blocked(), "{:?}", c.blockers());
        ensure!(
            apply_definition_comparison(&db, &db, &c).await?.status
                == DefinitionApplyStatus::Applied
        );
        exec(&db, &format!("CALL {}.p(4,@result)", m.destination)).await?;
        ensure!(scalar(&db, "SELECT @result").await? == "12");
        exec(&db,&format!("DROP FUNCTION {}.f; CREATE FUNCTION {}.f(x INTEGER) RETURNS INTEGER DETERMINISTIC RETURN abs(x)",m.source,m.source)).await?;
    }
    let blocked = compare(&db, &db, &m, ObjectKind::Function, "f").await?;
    ensure!(blocked.is_blocked() && blocked.plan().is_empty());
    ensure!(
        apply_definition_comparison(&db, &db, &blocked)
            .await?
            .status
            == DefinitionApplyStatus::Blocked
    );
    ensure!(scalar(&db, &format!("SELECT {}.f(3)", m.destination)).await? == "5");
    if engine == Engine::MySql {
        exec(&db, &format!("DROP USER '{grantee}'@'localhost'")).await?;
    }
    cleanup(&db, &m).await
}
#[tokio::test]
#[ignore = "Parent-owned Rowboat PG15435"]
async fn pass6_postgres_views() -> Result<()> {
    views(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Parent-owned Rowboat MySQL13309"]
async fn pass6_mysql_views() -> Result<()> {
    views(Engine::MySql).await
}
#[tokio::test]
#[ignore = "Parent-owned Rowboat PG15435"]
async fn pass6_postgres_routines() -> Result<()> {
    routines(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Parent-owned Rowboat MySQL13309"]
async fn pass6_mysql_routines() -> Result<()> {
    routines(Engine::MySql).await
}

#[tokio::test]
#[ignore = "Parent-owned Rowboat PG15435"]
async fn pass6_postgres_cancel_comparison_apply_releases_both_sessions() -> Result<()> {
    let a = connect(Engine::Postgres).await?;
    let b = connect(Engine::Postgres).await?;
    let observer = connect(Engine::Postgres).await?;
    let prefix = unique();
    let m = NamespaceMapping {
        source: format!("{prefix}a"),
        destination: format!("{prefix}b"),
    };
    for (schema, n) in [(&m.source, 2), (&m.destination, 1)] {
        exec(
            &a,
            &format!("CREATE SCHEMA {schema}; CREATE VIEW {schema}.v AS SELECT {n} AS value"),
        )
        .await?;
    }
    let c = compare(&a, &b, &m, ObjectKind::View, "v").await?;
    ensure!(!c.is_blocked(), "{:?}", c.blockers());
    let pid = scalar(&b, "SELECT pg_backend_pid()").await?;
    observer.begin().await?;
    exec(
        &observer,
        &format!("LOCK TABLE {}.v IN ACCESS EXCLUSIVE MODE", m.destination),
    )
    .await?;
    let source = a.clone();
    let destination = b.clone();
    let applying =
        tokio::spawn(async move { apply_definition_comparison(&source, &destination, &c).await });
    timeout(Duration::from_secs(5),async {loop {exec(&observer,"SELECT pg_stat_clear_snapshot()").await?;if scalar(&observer,&format!("SELECT count(*) FROM pg_stat_activity WHERE pid={pid} AND wait_event_type='Lock'")).await?=="1"{return Ok::<_,anyhow::Error>(());}sleep(Duration::from_millis(10)).await;}}).await.context("Apply never reached lock barrier")??;
    let destination = b.clone();
    let commit = tokio::spawn(async move { destination.commit().await });
    let source = a.clone();
    let source_query = tokio::spawn(async move { source.query("SELECT 1", 1).await });
    // Longer than the administrative worker's producer-idle timeout: neither
    // session may expire/release ownership while the other is waiting on DDL.
    sleep(Duration::from_secs(31)).await;
    ensure!(
        !source_query.is_finished(),
        "Source lease expired during destination wait"
    );
    ensure!(!commit.is_finished(), "External commit interleaved");
    applying.abort();
    ensure!(applying.await.unwrap_err().is_cancelled());
    observer.rollback().await?;
    timeout(Duration::from_secs(10), commit).await???;
    timeout(Duration::from_secs(10), source_query).await???;
    ensure!(!a.in_transaction().await? && !b.in_transaction().await?);
    ensure!(scalar(&b, &format!("SELECT value FROM {}.v", m.destination)).await? == "1");
    cleanup(&a, &m).await
}

#[tokio::test]
#[ignore = "Parent-owned Rowboat PG15435/MySQL13309"]
async fn pass6_opposing_reviews_have_stable_lease_order() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let a = connect(engine).await?;
        let b = connect(engine).await?;
        let prefix = unique();
        let m = NamespaceMapping {
            source: format!("{prefix}a"),
            destination: format!("{prefix}b"),
        };
        for (schema, n) in [(&m.source, 2), (&m.destination, 1)] {
            exec(
                &a,
                &format!("CREATE SCHEMA {schema}; CREATE VIEW {schema}.v AS SELECT {n} AS value"),
            )
            .await?;
        }
        let forward = compare(&a, &b, &m, ObjectKind::View, "v").await?;
        let reverse = compare(
            &b,
            &a,
            &NamespaceMapping {
                source: m.destination.clone(),
                destination: m.source.clone(),
            },
            ObjectKind::View,
            "v",
        )
        .await?;
        ensure!(!forward.is_blocked() && !reverse.is_blocked());
        let (first, second) = timeout(Duration::from_secs(10), async {
            tokio::join!(
                apply_definition_comparison(&a, &b, &forward),
                apply_definition_comparison(&b, &a, &reverse)
            )
        })
        .await?;
        let statuses = [first?.status, second?.status];
        ensure!(
            statuses
                .iter()
                .filter(|s| **s == DefinitionApplyStatus::Applied)
                .count()
                == 1
                && statuses.iter().any(|s| matches!(
                    s,
                    DefinitionApplyStatus::StaleSource | DefinitionApplyStatus::StaleDestination
                )),
            "Unexpected opposing outcomes: {statuses:?}"
        );
        ensure!(!a.in_transaction().await? && !b.in_transaction().await?);
        cleanup(&a, &m).await?;
    }
    Ok(())
}
