use anyhow::{Context, Result, ensure};
use rowboat_admin::{
    ddl::*,
    live_diff::{NamespaceMapping, columns::*},
    object_editor::*,
    objects::ObjectKind,
};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine, TableRef, sql::quote_ident};
use std::time::{SystemTime, UNIX_EPOCH};
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
            "root"
        }
        .into(),
        ssl_mode: "disable".into(),
        read_only: false,
        ..Default::default()
    }
}
async fn exec(db: &Database, sql: &str) -> Result<()> {
    db.query(sql, 1000).await?;
    Ok(())
}
async fn scalar(db: &Database, sql: &str) -> Result<String> {
    let r = db.query(sql, 10).await?;
    match &r[0].rows[0][0] {
        Cell::Text(s) => Ok(s.clone()),
        Cell::Binary(b) => Ok(String::from_utf8(b.clone())?),
        _ => anyhow::bail!("Unexpected scalar NULL"),
    }
}
fn unique() -> Result<String> {
    Ok(format!(
        "ko5{:x}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ))
}
async fn inspect(
    db: &Database,
    schema: &str,
    kind: ObjectKind,
    name: &str,
) -> Result<ObjectSnapshot> {
    let id = resolve_object(db, schema, kind, name, None, None).await?;
    inspect_object(db, &id).await
}
async fn applied(db: &Database, edit: &ObjectEdit) -> Result<()> {
    let outcome = apply_object_edit(db, edit).await?;
    ensure!(
        outcome.status == ObjectApplyStatus::Applied,
        "Apply outcome: {outcome:?}"
    );
    Ok(())
}

async fn mysql_programs(schema: &str) -> Result<()> {
    let db = Database::connect(&profile(Engine::MySql), "rowboat-test").await?;
    let ns = quote_ident(Engine::MySql, schema);
    exec(&db,&format!("CREATE SCHEMA {ns}; CREATE TABLE {ns}.events(value INTEGER); CREATE TABLE {ns}.t(id INTEGER,value INTEGER); CREATE USER '{schema}'@'localhost' ACCOUNT LOCK")).await?;
    exec(&db,&format!("CREATE FUNCTION {ns}.f(x INTEGER) RETURNS INTEGER DETERMINISTIC SQL SECURITY INVOKER COMMENT 'keep me' RETURN x+1; CREATE PROCEDURE {ns}.p(IN x INTEGER) SQL SECURITY INVOKER COMMENT 'keep procedure' BEGIN INSERT INTO {ns}.events VALUES(x); END; GRANT EXECUTE ON PROCEDURE {ns}.p TO '{schema}'@'localhost' WITH GRANT OPTION")).await?;
    exec(&db,&format!("CREATE TRIGGER {ns}.first_trigger BEFORE INSERT ON {ns}.t FOR EACH ROW SET NEW.value=NEW.value+1; CREATE TRIGGER {ns}.middle_trigger BEFORE INSERT ON {ns}.t FOR EACH ROW SET NEW.value=NEW.value*2; CREATE TRIGGER {ns}.last_trigger BEFORE INSERT ON {ns}.t FOR EACH ROW SET NEW.value=NEW.value+100")).await?;
    let f = inspect(&db, schema, ObjectKind::Function, "f").await?;
    let edit = plan_edit(
        &f,
        ObjectPatch::MySqlBody {
            body: "BEGIN /* native body ; */ RETURN x+2; END".into(),
        },
    )?;
    ensure!(
        edit.non_atomic()
            && edit.recovery_plan().is_some()
            && edit.plan().statements[0].destructive,
        "Missing non-atomic replacement/recovery metadata"
    );
    applied(&db, &edit).await?;
    let after = inspect_object(&db, f.identity()).await?;
    ensure!(
        after.definition().contains("DETERMINISTIC")
            && after.definition().contains("SQL SECURITY INVOKER")
            && after.definition().contains("keep me"),
        "Function native attributes lost"
    );
    ensure!(
        scalar(&db, &format!("SELECT {ns}.f(4)")).await? == "6",
        "Function body unchanged"
    );
    ensure!(
        !object_is_current(&db, &edit).await?,
        "Original routine not stale"
    );
    let p = inspect(&db, schema, ObjectKind::Procedure, "p").await?;
    let edit = plan_edit(
        &p,
        ObjectPatch::MySqlBody {
            body: format!(
                "BEGIN INSERT INTO {ns}.events VALUES(x+1); INSERT INTO {ns}.events VALUES(x+2); END"
            ),
        },
    )?;
    applied(&db, &edit).await?;
    let after = inspect_object(&db, p.identity()).await?;
    ensure!(
        after.catalog()["replacement"]["grants"] == p.catalog()["replacement"]["grants"],
        "Routine grants/grant options lost"
    );
    ensure!(
        scalar(&db, &format!("SELECT COUNT(*) FROM {ns}.events")).await? == "0",
        "Planning/replacement executed a stored body"
    );
    exec(&db, &format!("CALL {ns}.p(10)")).await?;
    ensure!(
        scalar(&db, &format!("SELECT SUM(value) FROM {ns}.events")).await? == "23",
        "Procedure statement list did not execute"
    );
    let trigger = inspect(&db, schema, ObjectKind::Trigger, "middle_trigger").await?;
    applied(
        &db,
        &plan_edit(
            &trigger,
            ObjectPatch::MySqlBody {
                body: "SET NEW.value=NEW.value*3".into(),
            },
        )?,
    )
    .await?;
    let after = inspect_object(&db, trigger.identity()).await?;
    ensure!(
        after.catalog()["replacement"]["native"]["order"]
            == trigger.catalog()["replacement"]["native"]["order"],
        "Trigger effective order changed"
    );
    exec(&db, &format!("INSERT INTO {ns}.t VALUES(1,1)")).await?;
    ensure!(
        scalar(&db, &format!("SELECT value FROM {ns}.t")).await? == "106",
        "Trigger order/body behavior was not preserved"
    );
    applied(
        &db,
        &plan_edit(
            &after,
            ObjectPatch::MySqlBody {
                body: "SET NEW.value=NEW.value*4".into(),
            },
        )?,
    )
    .await?;
    exec(&db, &format!("INSERT INTO {ns}.t VALUES(2,1)")).await?;
    ensure!(
        scalar(&db, &format!("SELECT value FROM {ns}.t WHERE id=2")).await? == "108",
        "Repeated trigger replacement lost ordering"
    );
    // A native CREATE failure after DROP must report absence and retain a
    // reviewable recovery plan. It must never pretend an implicit commit rolled back.
    let original = inspect(&db, schema, ObjectKind::Function, "f").await?;
    let failing = plan_edit(
        &original,
        ObjectPatch::MySqlBody {
            body: "RETURN 1 +".into(),
        },
    )?;
    let outcome = apply_object_edit(&db, &failing).await?;
    ensure!(
        outcome.status == ObjectApplyStatus::Failed
            && outcome.completed_statements == 1
            && outcome.failed_statement == Some(1)
            && !outcome.rolled_back
            && outcome.presence == ObjectPresence::Absent,
        "Incorrect partial failure outcome: {outcome:?}"
    );
    for statement in &failing
        .recovery_plan()
        .context("No recovery plan")?
        .statements
    {
        exec(&db, &statement.sql).await?;
    }
    ensure!(
        scalar(&db, &format!("SELECT {ns}.f(4)")).await? == "6",
        "Manual reviewed recovery did not restore original"
    );
    let original = inspect(&db, schema, ObjectKind::Function, "f").await?;
    for body in [
        "BEGIN RETURN 1; END; DROP TABLE t",
        "RETURN 1; /*! DROP TABLE t */",
        "BEGIN IF 1 THEN RETURN 2; END IF; END",
        "RETURN 'bad\\quote'",
    ] {
        ensure!(
            plan_edit(&original, ObjectPatch::MySqlBody { body: body.into() }).is_err(),
            "Unsupported/escaping body accepted: {body}"
        );
    }
    let edit = plan_edit(
        &original,
        ObjectPatch::MySqlBody {
            body: "RETURN x+3".into(),
        },
    )?;
    let other_session = Database::connect(&profile(Engine::MySql), "rowboat-test").await?;
    ensure!(
        apply_object_edit(&other_session, &edit).await?.status == ObjectApplyStatus::Stale,
        "Reviewed edit crossed physical session identity"
    );
    exec(
        &db,
        &format!("ALTER FUNCTION {ns}.f COMMENT 'concurrent change'"),
    )
    .await?;
    ensure!(
        apply_object_edit(&db, &edit).await?.status == ObjectApplyStatus::Stale,
        "Stale replacement was applied"
    );
    let source_schema = format!("{schema}s");
    let source_ns = quote_ident(Engine::MySql, &source_schema);
    exec(&db,&format!("CREATE SCHEMA {source_ns}; CREATE FUNCTION {source_ns}.f(x INTEGER) RETURNS INTEGER DETERMINISTIC SQL SECURITY DEFINER COMMENT 'synchronized attributes' RETURN x+99")).await?;
    let source = inspect(&db, &source_schema, ObjectKind::Function, "f").await?;
    let destination = inspect(&db, schema, ObjectKind::Function, "f").await?;
    let mapping = NamespaceMapping {
        source: source_schema.clone(),
        destination: schema.into(),
    };
    let comparison = compare_object_attributes(&source, &destination, &mapping)?;
    ensure!(
        comparison.differences.len() == 2,
        "Missing routine attribute differences"
    );
    applied(&db, comparison.edit()).await?;
    let destination = inspect(&db, schema, ObjectKind::Function, "f").await?;
    ensure!(
        compare_object_attributes(&source, &destination, &mapping)?
            .edit()
            .plan()
            .is_empty(),
        "Routine attributes did not converge"
    );
    ensure!(
        scalar(&db, &format!("SELECT {ns}.f(4)")).await? == "6",
        "Attribute sync overwrote the destination routine body"
    );
    exec(&db, &format!("DROP SCHEMA {source_ns}")).await?;
    exec(
        &db,
        &format!("DROP SCHEMA {ns}; DROP USER '{schema}'@'localhost'"),
    )
    .await?;
    Ok(())
}
async fn postgres_materialized(schema: &str) -> Result<()> {
    let db = Database::connect(&profile(Engine::Postgres), "rowboat-test").await?;
    let ns = quote_ident(Engine::Postgres, schema);
    exec(&db,&format!("CREATE SCHEMA {ns}; CREATE TABLE {ns}.src(id INTEGER,value INTEGER); INSERT INTO {ns}.src VALUES(1,10); CREATE MATERIALIZED VIEW {ns}.mv AS SELECT id,value FROM {ns}.src WITH NO DATA; COMMENT ON MATERIALIZED VIEW {ns}.mv IS 'retain table comment'; COMMENT ON COLUMN {ns}.mv.value IS 'retain column comment'")).await?;
    let original = inspect(&db, schema, ObjectKind::MaterializedView, "mv").await?;
    ensure!(
        plan_edit(
            &original,
            ObjectPatch::MaterializedRefresh {
                concurrently: true,
                with_data: true
            }
        )
        .is_err(),
        "Unpopulated concurrent refresh accepted"
    );
    applied(
        &db,
        &plan_edit(
            &original,
            ObjectPatch::MaterializedRefresh {
                concurrently: false,
                with_data: true,
            },
        )?,
    )
    .await?;
    let original = inspect(&db, schema, ObjectKind::MaterializedView, "mv").await?;
    ensure!(
        plan_edit(
            &original,
            ObjectPatch::MaterializedRefresh {
                concurrently: true,
                with_data: true
            }
        )
        .is_err(),
        "Concurrent refresh without unique index accepted"
    );
    applied(
        &db,
        &plan_edit(
            &original,
            ObjectPatch::MaterializedIndex {
                index: IndexDesign {
                    name: "mv_id".into(),
                    columns: vec!["id".into()],
                    unique: true,
                },
            },
        )?,
    )
    .await?;
    exec(&db,&format!("COMMENT ON INDEX {ns}.mv_id IS 'retain index comment'; INSERT INTO {ns}.src VALUES(2,20)")).await?;
    let original = inspect(&db, schema, ObjectKind::MaterializedView, "mv").await?;
    applied(
        &db,
        &plan_edit(
            &original,
            ObjectPatch::MaterializedRefresh {
                concurrently: true,
                with_data: true,
            },
        )?,
    )
    .await?;
    ensure!(
        scalar(&db, &format!("SELECT SUM(value) FROM {ns}.mv")).await? == "30",
        "Concurrent refresh did not load rows"
    );
    exec(
        &db,
        &format!("CREATE VIEW {ns}.dependent AS SELECT id FROM {ns}.mv"),
    )
    .await?;
    let original = inspect(&db, schema, ObjectKind::MaterializedView, "mv").await?;
    ensure!(
        plan_edit(
            &original,
            ObjectPatch::MaterializedQuery {
                query: format!("SELECT id,value*2 FROM {ns}.src")
            }
        )
        .is_err(),
        "Dependent view was silently dropped"
    );
    exec(&db, &format!("DROP VIEW {ns}.dependent")).await?;
    let original = inspect(&db, schema, ObjectKind::MaterializedView, "mv").await?;
    let edit = plan_edit(
        &original,
        ObjectPatch::MaterializedQuery {
            query: format!("SELECT id,value*2 FROM {ns}.src"),
        },
    )?;
    ensure!(
        !edit.non_atomic() && !edit.plan().sql().contains("CASCADE"),
        "Unsafe materialized replacement"
    );
    applied(&db, &edit).await?;
    let after = inspect(&db, schema, ObjectKind::MaterializedView, "mv").await?;
    ensure!(
        scalar(&db, &format!("SELECT SUM(value) FROM {ns}.mv")).await? == "60",
        "Materialized query not replaced"
    );
    ensure!(
        after.catalog()["comment"] == original.catalog()["comment"]
            && after.catalog()["columns"][1]["comment"]
                == original.catalog()["columns"][1]["comment"]
            && after.catalog()["indexes"][0]["comment"]
                == original.catalog()["indexes"][0]["comment"],
        "Materialized comments/index metadata lost"
    );
    let edit = plan_edit(
        &after,
        ObjectPatch::MaterializedQuery {
            query: format!("SELECT absent,value FROM {ns}.src"),
        },
    )?;
    let outcome = apply_object_edit(&db, &edit).await?;
    ensure!(
        outcome.status == ObjectApplyStatus::Failed
            && outcome.rolled_back
            && outcome.presence == ObjectPresence::Present,
        "PG replacement did not roll back: {outcome:?}"
    );
    ensure!(
        scalar(&db, &format!("SELECT SUM(value) FROM {ns}.mv")).await? == "60",
        "Failed replacement lost old materialized data"
    );
    exec(
        &db,
        &format!("ALTER MATERIALIZED VIEW {ns}.mv ALTER COLUMN value SET STATISTICS 10"),
    )
    .await?;
    let custom = inspect(&db, schema, ObjectKind::MaterializedView, "mv").await?;
    ensure!(
        plan_edit(
            &custom,
            ObjectPatch::MaterializedQuery {
                query: format!("SELECT id,value FROM {ns}.src")
            }
        )
        .is_err(),
        "Custom column statistics were silently lost"
    );
    exec(
        &db,
        &format!("ALTER MATERIALIZED VIEW {ns}.mv ALTER COLUMN value SET STATISTICS -1"),
    )
    .await?;
    exec(
        &db,
        &format!("ALTER MATERIALIZED VIEW {ns}.mv DEPENDS ON EXTENSION plpgsql"),
    )
    .await?;
    let dependent = inspect(&db, schema, ObjectKind::MaterializedView, "mv").await?;
    ensure!(
        plan_edit(
            &dependent,
            ObjectPatch::MaterializedQuery {
                query: format!("SELECT id,value FROM {ns}.src")
            }
        )
        .is_err(),
        "Explicit extension dependency was silently lost"
    );
    exec(
        &db,
        &format!("ALTER MATERIALIZED VIEW {ns}.mv NO DEPENDS ON EXTENSION plpgsql"),
    )
    .await?;
    exec(&db, &format!("GRANT SELECT ON {ns}.mv TO PUBLIC")).await?;
    let original = inspect(&db, schema, ObjectKind::MaterializedView, "mv").await?;
    ensure!(
        plan_edit(
            &original,
            ObjectPatch::MaterializedQuery {
                query: format!("SELECT id,value FROM {ns}.src")
            }
        )
        .is_err(),
        "Explicit ACL silently lost"
    );
    exec(&db, &format!("DROP SCHEMA {ns} CASCADE")).await?;
    Ok(())
}
async fn mysql_columns(schema: &str) -> Result<()> {
    let db = Database::connect(&profile(Engine::MySql), "rowboat-test").await?;
    let mapping = NamespaceMapping {
        source: format!("{schema}s"),
        destination: format!("{schema}d"),
    };
    let s = quote_ident(Engine::MySql, &mapping.source);
    let d = quote_ident(Engine::MySql, &mapping.destination);
    exec(&db, &format!("CREATE SCHEMA {s}; CREATE SCHEMA {d}")).await?;
    for (ns, source) in [(&s, true), (&d, false)] {
        exec(&db,&format!("CREATE TABLE {ns}.t(id INTEGER PRIMARY KEY,qty INTEGER NOT NULL DEFAULT 2,category ENUM('Small','LARGE'{}) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL DEFAULT 'Small' COMMENT 'case sensitive',flags SET('X','Y') DEFAULT 'X',stamp DATETIME(3) DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3),small TINYINT(1) UNSIGNED DEFAULT 1,amount INTEGER GENERATED ALWAYS AS (qty*{}) STORED COMMENT 'generated retained'{})",if source{",'NEW'"}else{""},if source{3}else{2},if source{",new_amount INTEGER GENERATED ALWAYS AS (qty+1) VIRTUAL"}else{",destination_only INTEGER DEFAULT 7"})).await?;
    }
    exec(&db, &format!("INSERT INTO {d}.t(id,qty) VALUES(1,4)")).await?;
    let table = TableRef {
        schema: mapping.destination.clone(),
        name: "t".into(),
    };
    let original = inspect_mysql_column(&db, &table, "amount").await?;
    let mut desired = original.definition().clone();
    desired.generation.as_mut().unwrap().expression = "qty * 5".into();
    let edit = plan_mysql_column_edit(&original, desired)?;
    ensure!(
        mysql_column_is_current(&db, &edit).await?,
        "Fresh advanced column is stale"
    );
    for statement in &edit.plan().statements {
        exec(&db, &statement.sql).await?;
    }
    ensure!(
        scalar(&db, &format!("SELECT amount FROM {d}.t")).await? == "20",
        "Stored generated expression not edited"
    );
    ensure!(
        !mysql_column_is_current(&db, &edit).await?,
        "Changed generated expression not detected"
    );
    let comparison = compare_mysql_columns(&db, &db, &mapping, "t").await?;
    ensure!(
        !comparison.is_blocked(),
        "Advanced comparison blocked: {:?}",
        comparison.blockers
    );
    ensure!(
        columns_destination_is_current(&db, &comparison).await?,
        "Fresh column-set snapshot stale"
    );
    exec(
        &db,
        &format!("ALTER TABLE {d}.t ADD COLUMN freshness_probe INTEGER"),
    )
    .await?;
    ensure!(
        !columns_destination_is_current(&db, &comparison).await?,
        "New sibling column not detected"
    );
    exec(
        &db,
        &format!("ALTER TABLE {d}.t DROP COLUMN freshness_probe"),
    )
    .await?;
    let comparison = compare_mysql_columns(&db, &db, &mapping, "t").await?;
    for statement in &comparison.plan.statements {
        exec(&db, &statement.sql).await?;
    }
    let equal = compare_mysql_columns(&db, &db, &mapping, "t").await?;
    ensure!(
        !equal.is_blocked() && equal.plan.is_empty(),
        "Advanced columns did not converge: {} {:?}",
        equal.plan.sql(),
        equal.blockers
    );
    ensure!(
        scalar(
            &db,
            &format!("SELECT amount+new_amount+destination_only FROM {d}.t WHERE id=1")
        )
        .await?
            == "24",
        "Generated results/destination-only data not preserved"
    );
    exec(
        &db,
        &format!("UPDATE {d}.t SET category='NEW',flags='X,Y' WHERE id=1"),
    )
    .await?;
    ensure!(
        scalar(&db, &format!("SELECT category FROM {d}.t WHERE id=1")).await? == "NEW",
        "ENUM expansion not applied"
    );
    let generated = inspect_mysql_column(&db, &table, "amount").await?;
    let mut conversion = generated.definition().clone();
    conversion.generation.as_mut().unwrap().storage = GeneratedStorage::Virtual;
    ensure!(
        plan_mysql_column_edit(&generated, conversion).is_err(),
        "STORED/VIRTUAL conversion used lossy drop/add"
    );
    exec(&db, &format!("DROP SCHEMA {s}; DROP SCHEMA {d}")).await?;
    Ok(())
}
#[tokio::test]
#[ignore = "Requires task-owned MySQL 127.0.0.1:13309"]
async fn pass5_mysql_program_replacement_order_grants_and_partial_failure() -> Result<()> {
    let s = unique()?;
    mysql_programs(&s).await.with_context(|| {
        format!("Pass5 program fixture {s}; failure retains schema/user for diagnosis")
    })
}
#[tokio::test]
#[ignore = "Requires task-owned PostgreSQL 127.0.0.1:15435"]
async fn pass5_postgres_materialized_refresh_index_replace_and_rollback() -> Result<()> {
    let s = unique()?;
    postgres_materialized(&s)
        .await
        .with_context(|| format!("Pass5 materialized fixture {s}; failure retains schema"))
}
#[tokio::test]
#[ignore = "Requires task-owned MySQL 127.0.0.1:13309"]
async fn pass5_mysql_advanced_column_edit_and_comparison() -> Result<()> {
    let s = unique()?;
    mysql_columns(&s).await.with_context(|| {
        format!("Pass5 advanced-column fixture {s}s/{s}d; failure retains schemas")
    })
}

async fn postgres_attributes(schema: &str) -> Result<()> {
    let db = Database::connect(&profile(Engine::Postgres), "rowboat-test").await?;
    let mapping = NamespaceMapping {
        source: format!("{schema}s"),
        destination: format!("{schema}d"),
    };
    let s = quote_ident(Engine::Postgres, &mapping.source);
    let d = quote_ident(Engine::Postgres, &mapping.destination);
    for ns in [&s, &d] {
        exec(&db,&format!("CREATE SCHEMA {ns}; CREATE TABLE {ns}.t(id INTEGER); CREATE FUNCTION {ns}.tf() RETURNS trigger LANGUAGE plpgsql AS $b$BEGIN RETURN NEW; END$b$; CREATE TRIGGER tg BEFORE INSERT ON {ns}.t FOR EACH ROW EXECUTE FUNCTION {ns}.tf()")).await?;
    }
    exec(&db,&format!("ALTER TABLE {s}.t DISABLE TRIGGER tg; CREATE SEQUENCE {s}.seq START 7 INCREMENT 2 CACHE 3 MAXVALUE 1000 CYCLE; CREATE SEQUENCE {d}.seq; CREATE FUNCTION {s}.f() RETURNS INTEGER LANGUAGE sql SECURITY DEFINER AS $b$SELECT 1$b$; COMMENT ON FUNCTION {s}.f() IS 'source comment'; CREATE FUNCTION {d}.f() RETURNS INTEGER LANGUAGE sql SECURITY INVOKER AS $b$SELECT 2$b$")).await?;
    for (kind, name) in [
        (ObjectKind::Sequence, "seq"),
        (ObjectKind::Trigger, "tg"),
        (ObjectKind::Function, "f"),
    ] {
        let source = inspect(&db, &mapping.source, kind, name).await?;
        let destination = inspect(&db, &mapping.destination, kind, name).await?;
        let comparison = compare_object_attributes(&source, &destination, &mapping)?;
        ensure!(
            !comparison.differences.is_empty(),
            "Missing {kind:?} attribute differences"
        );
        applied(&db, comparison.edit()).await?;
        let destination = inspect(&db, &mapping.destination, kind, name).await?;
        ensure!(
            compare_object_attributes(&source, &destination, &mapping)?
                .edit()
                .plan()
                .is_empty(),
            "{kind:?} attributes did not converge"
        );
    }
    ensure!(
        scalar(&db, &format!("SELECT nextval('{d}.seq')")).await? == "1",
        "Sequence attribute sync copied/reset mutable counter"
    );
    ensure!(
        scalar(&db, &format!("SELECT {d}.f()")).await? == "2",
        "Routine attribute sync overwrote destination body"
    );
    exec(
        &db,
        &format!("DROP SCHEMA {s} CASCADE; DROP SCHEMA {d} CASCADE"),
    )
    .await?;
    Ok(())
}
#[tokio::test]
#[ignore = "Requires task-owned PostgreSQL 127.0.0.1:15435"]
async fn pass5_postgres_existing_object_attribute_comparison() -> Result<()> {
    let s = unique()?;
    postgres_attributes(&s)
        .await
        .with_context(|| format!("Pass5 attribute fixtures {s}s/{s}d; failure retains schemas"))
}
