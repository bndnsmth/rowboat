use anyhow::{Context, Result, ensure};
use rowboat_admin::{explain, metrics, object_editor::*, objects::ObjectKind};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine, sql::quote_ident};
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
    let results = db.query(sql, 10).await?;
    match &results[0].rows[0][0] {
        Cell::Text(s) => Ok(s.clone()),
        Cell::Binary(b) => Ok(String::from_utf8(b.clone())?),
        Cell::Null => anyhow::bail!("Unexpected NULL"),
    }
}
async fn apply(db: &Database, edit: &ObjectEdit) -> Result<()> {
    ensure!(object_is_current(db, edit).await?, "Fresh object was stale");
    for s in &edit.plan().statements {
        exec(db, &s.sql)
            .await
            .with_context(|| format!("Applying explicit test review: {}", s.reason))?;
    }
    Ok(())
}
async fn inspect(
    db: &Database,
    schema: &str,
    kind: ObjectKind,
    name: &str,
    signature: Option<&str>,
    parent: Option<&str>,
) -> Result<ObjectSnapshot> {
    let id = resolve_object(db, schema, kind, name, signature, parent).await?;
    inspect_object(db, &id).await
}
async fn exercise(engine: Engine, schema: &str) -> Result<()> {
    let db = Database::connect(&profile(engine), "rowboat-test").await?;
    let ns = quote_ident(engine, schema);
    exec(&db,&format!("CREATE SCHEMA {ns}; CREATE TABLE {ns}.a(id INTEGER PRIMARY KEY); CREATE TABLE {ns}.b(id INTEGER PRIMARY KEY); CREATE TABLE {ns}.marker(id INTEGER); INSERT INTO {ns}.a VALUES(1),(2); INSERT INTO {ns}.b VALUES(1)")).await?;
    if engine == Engine::Postgres {
        exec(&db,&format!("CREATE FUNCTION {ns}.f(x integer) RETURNS integer LANGUAGE plpgsql IMMUTABLE STRICT SECURITY DEFINER PARALLEL SAFE COST 17 SET search_path=pg_catalog AS $body$BEGIN RETURN x+1; END$body$; COMMENT ON FUNCTION {ns}.f(integer) IS 'original comment'; REVOKE ALL ON FUNCTION {ns}.f(integer) FROM PUBLIC; CREATE FUNCTION {ns}.f(x text) RETURNS text LANGUAGE sql AS $body$SELECT x$body$; CREATE PROCEDURE {ns}.p(x integer) LANGUAGE plpgsql AS $body$BEGIN INSERT INTO {ns}.marker VALUES(x); END$body$")).await?;
        exec(&db,&format!("CREATE FUNCTION {ns}.trigger_fn() RETURNS trigger LANGUAGE plpgsql AS $body$BEGIN INSERT INTO {ns}.marker VALUES(99); RETURN NEW; END$body$; CREATE TRIGGER same_trigger BEFORE INSERT ON {ns}.a FOR EACH ROW EXECUTE FUNCTION {ns}.trigger_fn(); CREATE TRIGGER same_trigger BEFORE INSERT ON {ns}.b FOR EACH ROW EXECUTE FUNCTION {ns}.trigger_fn(); CREATE VIEW {ns}.v WITH (security_barrier=true,security_invoker=true,check_option=local) AS SELECT id FROM {ns}.a WHERE id>0; CREATE MATERIALIZED VIEW {ns}.mv AS SELECT id FROM {ns}.a WITH NO DATA; CREATE SEQUENCE {ns}.seq AS bigint START 7 INCREMENT 2 CACHE 3 MAXVALUE 1000 CYCLE OWNED BY {ns}.a.id")).await?;
        ensure!(
            resolve_object(&db, schema, ObjectKind::Function, "f", None, None)
                .await
                .is_err(),
            "Overload ambiguity was guessed"
        );
        ensure!(
            resolve_object(&db, schema, ObjectKind::Trigger, "same_trigger", None, None)
                .await
                .is_err(),
            "Trigger parent ambiguity was guessed"
        );
        let f = inspect(
            &db,
            schema,
            ObjectKind::Function,
            "f",
            Some("x integer"),
            None,
        )
        .await?;
        let edit=plan_edit(&f,ObjectPatch::RoutineBody{body:"BEGIN /* semicolons ; are body syntax */ PERFORM '$rowboat_body$'; RETURN x+2; END;".into()})?;
        let original = serde_json::to_value(edit.original())?;
        apply(&db, &edit).await?;
        ensure!(
            !object_is_current(&db, &edit).await?,
            "Edited routine still reports original current"
        );
        ensure!(
            serde_json::to_value(edit.original())? == original,
            "Original snapshot mutated"
        );
        let after = inspect_object(&db, f.identity()).await?;
        for field in [
            "oid",
            "proacl",
            "prosecdef",
            "proconfig",
            "provolatile",
            "proparallel",
            "procost",
            "proisstrict",
        ] {
            ensure!(
                after.catalog()["record"][field] == f.catalog()["record"][field],
                "Routine attribute {field} was lost"
            );
        }
        ensure!(
            after.catalog()["comment"] == f.catalog()["comment"],
            "Routine comment was lost"
        );
        ensure!(
            scalar(&db, &format!("SELECT {ns}.f(3)")).await? == "5",
            "Patched body not active"
        );
        apply(
            &db,
            &plan_edit(
                &after,
                ObjectPatch::RoutineComment {
                    comment: "kept identity, changed comment".into(),
                },
            )?,
        )
        .await?;
        let after = inspect_object(&db, f.identity()).await?;
        apply(
            &db,
            &plan_edit(&after, ObjectPatch::RoutineSecurity { definer: false })?,
        )
        .await?;
        ensure!(
            inspect_object(&db, f.identity()).await?.catalog()["record"]["proacl"]
                == f.catalog()["record"]["proacl"],
            "Targeted routine ALTER lost ACL"
        );
        let p = inspect(
            &db,
            schema,
            ObjectKind::Procedure,
            "p",
            Some("IN x integer"),
            None,
        )
        .await?;
        ensure!(
            p.definition().contains("INSERT INTO"),
            "Procedure native body incomplete"
        );
        let pe = plan_edit(
            &p,
            ObjectPatch::RoutineBody {
                body: format!("BEGIN INSERT INTO {ns}.marker VALUES(x+1); END;"),
            },
        )?;
        apply(&db, &pe).await?;
        let t = inspect(
            &db,
            schema,
            ObjectKind::Trigger,
            "same_trigger",
            None,
            Some("a"),
        )
        .await?;
        let te = plan_edit(
            &t,
            ObjectPatch::TriggerEnabled {
                state: TriggerState::Replica,
            },
        )?;
        apply(&db, &te).await?;
        let t_after = inspect_object(&db, t.identity()).await?;
        ensure!(
            t_after.catalog()["record"]["tgenabled"] == "R",
            "Wrong trigger state"
        );
        ensure!(
            inspect(
                &db,
                schema,
                ObjectKind::Trigger,
                "same_trigger",
                None,
                Some("b")
            )
            .await?
            .catalog()["record"]["tgenabled"]
                == "O",
            "Changed the other same-named trigger"
        );
        let mv = inspect(&db, schema, ObjectKind::MaterializedView, "mv", None, None).await?;
        ensure!(
            mv.definition().contains("WITH NO DATA"),
            "Missing materialized view state"
        );
        ensure!(
            plan_edit(
                &mv,
                ObjectPatch::ViewQuery {
                    query: "SELECT 2".into()
                }
            )
            .is_err(),
            "Materialized view destructive replacement accepted"
        );
        let seq = inspect(&db, schema, ObjectKind::Sequence, "seq", None, None).await?;
        ensure!(
            seq.definition().contains("OWNED BY") && seq.definition().contains("CACHE 3"),
            "Sequence definition incomplete"
        );
        let se = plan_edit(
            &seq,
            ObjectPatch::Sequence {
                patch: SequencePatch {
                    increment: Some(4),
                    cache: Some(5),
                    ..Default::default()
                },
            },
        )?;
        apply(&db, &se).await?;
        let new_seq = inspect_object(&db, seq.identity()).await?;
        ensure!(
            new_seq.catalog()["ownership"] == seq.catalog()["ownership"],
            "Sequence ownership lost"
        );
        ensure!(
            new_seq.catalog()["sequence"]["seqincrement"] == 4,
            "Sequence increment unchanged"
        );
    } else {
        exec(&db,&format!("CREATE FUNCTION {ns}.f(x INTEGER) RETURNS INTEGER DETERMINISTIC SQL SECURITY INVOKER COMMENT 'original comment' RETURN x+1; CREATE PROCEDURE {ns}.p(IN x INTEGER) SQL SECURITY INVOKER COMMENT 'procedure comment' BEGIN INSERT INTO {ns}.marker VALUES(x); INSERT INTO {ns}.marker VALUES(x+1); END; CREATE TRIGGER {ns}.same_trigger BEFORE INSERT ON {ns}.a FOR EACH ROW BEGIN INSERT INTO {ns}.marker VALUES(99); END; CREATE ALGORITHM=MERGE SQL SECURITY INVOKER VIEW {ns}.v AS SELECT id FROM {ns}.a WHERE id>0 WITH CASCADED CHECK OPTION")).await?;
        for (kind, name) in [
            (ObjectKind::Function, "f"),
            (ObjectKind::Procedure, "p"),
            (ObjectKind::Trigger, "same_trigger"),
        ] {
            let object = inspect(&db, schema, kind, name, None, None).await?;
            ensure!(
                object.definition().contains("CREATE DEFINER=")
                    && object.definition().contains(name),
                "Missing native MySQL CREATE/DEFINER"
            );
            ensure!(
                plan_edit(
                    &object,
                    ObjectPatch::RoutineBody {
                        body: "RETURN 7".into()
                    }
                )
                .is_err(),
                "Unsupported MySQL body replacement was allowed"
            );
        }
        let f = inspect(&db, schema, ObjectKind::Function, "f", None, None).await?;
        let edit = plan_edit(
            &f,
            ObjectPatch::RoutineComment {
                comment: "changed ' comment".into(),
            },
        )?;
        apply(&db, &edit).await?;
        ensure!(
            !object_is_current(&db, &edit).await?,
            "Changed routine comment wasn't stale"
        );
        let after = inspect_object(&db, f.identity()).await?;
        ensure!(
            after.definition().contains("SQL SECURITY INVOKER")
                && after.definition().contains("DETERMINISTIC")
                && after.definition().contains("RETURN x+1"),
            "MySQL routine attributes/body were lost"
        );
        let edit = plan_edit(&after, ObjectPatch::RoutineSecurity { definer: true })?;
        apply(&db, &edit).await?;
        ensure!(
            scalar(&db, &format!("SELECT {ns}.f(3)")).await? == "4",
            "Routine body changed unexpectedly"
        );
        let mode = scalar(&db, "SELECT @@session.sql_mode").await?;
        exec(&db,&format!("SET SESSION sql_mode='ANSI_QUOTES'; CREATE FUNCTION {ns}.mode_guard() RETURNS INTEGER DETERMINISTIC RETURN 1")).await?;
        exec(
            &db,
            &format!(
                "SET SESSION sql_mode={}",
                rowboat_core::sql::literal(engine, &Cell::Text(mode))
            ),
        )
        .await?;
        let mode_guard =
            inspect(&db, schema, ObjectKind::Function, "mode_guard", None, None).await?;
        ensure!(
            plan_edit(
                &mode_guard,
                ObjectPatch::RoutineComment {
                    comment: "would reset stored mode".into()
                }
            )
            .is_err(),
            "Routine ALTER silently replaced its stored SQL mode"
        );
    }
    // Inspection and review must not execute trigger/routine bodies.
    ensure!(
        scalar(&db, &format!("SELECT COUNT(*) FROM {ns}.marker")).await? == "0",
        "Object inspection/review executed a body"
    );
    let view = inspect(&db, schema, ObjectKind::View, "v", None, None).await?;
    for sql in [
        format!("SELECT id FROM {ns}.a; INSERT INTO {ns}.marker VALUES(42)"),
        format!("SELECT id FROM {ns}.a); DROP TABLE {ns}.b; --"),
    ] {
        ensure!(
            plan_edit(&view, ObjectPatch::ViewQuery { query: sql }).is_err(),
            "Hidden extra command accepted"
        );
    }
    let edit = plan_edit(
        &view,
        ObjectPatch::ViewQuery {
            query: format!("SELECT id FROM {ns}.a WHERE id > 1 -- ending comment"),
        },
    )?;
    apply(&db, &edit).await?;
    ensure!(
        scalar(&db, &format!("SELECT COUNT(*) FROM {ns}.v")).await? == "1",
        "View query patch wasn't active"
    );
    let after = inspect_object(&db, view.identity()).await?;
    if engine == Engine::Postgres {
        for field in ["oid", "relowner", "relacl", "reloptions"] {
            ensure!(
                after.catalog()["record"][field] == view.catalog()["record"][field],
                "View {field} changed"
            );
        }
    } else {
        ensure!(
            after.catalog()["view"] == view.catalog()["view"],
            "View definer/security/check/charset changed"
        );
        ensure!(
            after.definition().contains("ALGORITHM=MERGE"),
            "View algorithm lost"
        );
        ensure!(
            after.definition().contains("(`id`)"),
            "First reviewed ALTER did not produce the native explicit column-list regression shape"
        );
        let repeated = plan_edit(
            &after,
            ObjectPatch::ViewQuery {
                query: format!("SELECT id AS attempted_rename FROM {ns}.a WHERE id > 0"),
            },
        )
        .context("Planning repeated MySQL view edit with native explicit column list")?;
        apply(&db, &repeated).await?;
        ensure!(
            !object_is_current(&db, &repeated).await?,
            "Repeated edit did not invalidate its original"
        );
        let twice = inspect_object(&db, view.identity()).await?;
        ensure!(
            twice.catalog()["columns"] == view.catalog()["columns"],
            "Repeated view edit renamed inspected columns"
        );
        ensure!(
            twice.catalog()["view"] == view.catalog()["view"]
                && twice.definition().contains("ALGORITHM=MERGE"),
            "Repeated view edit lost native attributes"
        );
        ensure!(
            scalar(&db, &format!("SELECT SUM(id) FROM {ns}.v")).await? == "3",
            "Repeated view query failed or original column name was not preserved"
        );
        ensure!(
            plan_edit(
                &twice,
                ObjectPatch::ViewQuery {
                    query: format!("SELECT id FROM {ns}.a; INSERT INTO {ns}.marker VALUES(42)")
                }
            )
            .is_err(),
            "Repeated edit loosened the query boundary guard"
        );
    }
    // Full native inspection, estimates-only JSON, and real dashboard samples.
    let query = format!("SELECT a.id FROM {ns}.a a JOIN {ns}.b b ON a.id=b.id WHERE a.id > 0");
    let report = explain::execute(&db, &query).await?;
    ensure!(
        !report.roots.is_empty(),
        "No decoded plan tree: {:?}",
        report.warnings
    );
    fn check(nodes: &[explain::PlanNode]) -> bool {
        nodes
            .iter()
            .any(|n| n.relation.is_some() || check(&n.children))
    }
    fn estimated(nodes: &[explain::PlanNode]) -> bool {
        nodes
            .iter()
            .all(|n| n.actual.is_empty() && estimated(&n.children))
    }
    ensure!(
        check(&report.roots) && estimated(&report.roots),
        "Missing relation or invented actual data"
    );
    ensure!(
        explain::execute(&db, &format!("SELECT 1; INSERT INTO {ns}.marker VALUES(3)"))
            .await
            .is_err(),
        "EXPLAIN bypassed boundary guard"
    );
    let a = metrics::sample_dashboard(&db).await?;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let b = metrics::sample_dashboard(&db).await?;
    let delta = metrics::dashboard_delta(&a, &b);
    ensure!(
        delta.elapsed_ms.is_some() && !delta.metrics.is_empty(),
        "Dashboard samples missing"
    );
    ensure!(
        delta
            .metrics
            .iter()
            .all(|m| m.state != metrics::DeltaState::Incompatible),
        "Same-connection sample identity changed"
    );
    ensure!(
        scalar(&db, &format!("SELECT COUNT(*) FROM {ns}.marker")).await? == "0",
        "Read-only inspection/explain executed hidden work"
    );
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
    ensure!(
        !object_is_current(&db, &edit).await?,
        "Dropped object reported current"
    );
    Ok(())
}
async fn run(engine: Engine) -> Result<()> {
    let name = format!(
        "pass12_admin_{:x}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    exercise(engine, &name).await.with_context(|| {
        format!(
            "Disposable pass4 schema {name}; failure retains only this run's objects for diagnosis"
        )
    })
}
#[tokio::test]
#[ignore = "Requires parent-owned PostgreSQL 127.0.0.1:15435"]
async fn postgres_object_edits_plans_and_dashboard() -> Result<()> {
    run(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Requires parent-owned MySQL 127.0.0.1:13309"]
async fn mysql_object_edits_plans_and_dashboard() -> Result<()> {
    run(Engine::MySql).await
}

async fn review_regression(engine: Engine, schema: &str) -> Result<()> {
    let db = Database::connect(&profile(engine), "rowboat-test").await?;
    let ns = quote_ident(engine, schema);
    exec(&db, &format!("CREATE SCHEMA {ns}")).await?;
    if engine == Engine::Postgres {
        exec(&db,&format!("CREATE TABLE {ns}.t(id INTEGER); CREATE VIEW {ns}.v AS SELECT id FROM {ns}.t; ALTER VIEW {ns}.v ALTER COLUMN id SET DEFAULT 1")).await?;
        let before = inspect(&db, schema, ObjectKind::View, "v", None, None).await?;
        let original = serde_json::to_value(&before)?;
        let patch = ObjectPatch::ViewQuery {
            query: format!("SELECT id FROM {ns}.t WHERE id > 0"),
        };
        let edit = plan_edit(&before, patch.clone())?;
        ensure!(
            object_is_current(&db, &edit).await?,
            "Fresh view with a default was stale"
        );
        exec(
            &db,
            &format!("ALTER VIEW {ns}.v ALTER COLUMN id SET DEFAULT 2"),
        )
        .await?;
        let after = inspect_object(&db, before.identity()).await?;
        ensure!(
            before.definition() == after.definition(),
            "Regression must leave the view query unchanged"
        );
        ensure!(
            before.catalog()["columns"][0]["record"]["atthasdef"] == true
                && after.catalog()["columns"][0]["record"]["atthasdef"] == true,
            "Regression must change an existing default, not its presence"
        );
        ensure!(
            before.catalog()["columns"][0]["default"] == "1"
                && after.catalog()["columns"][0]["default"] == "2",
            "Native default expression missing from view snapshot"
        );
        ensure!(
            before.catalog()["columns"][0]["default_record"]
                != after.catalog()["columns"][0]["default_record"],
            "Native pg_attrdef evidence did not change"
        );
        ensure!(
            !object_is_current(&db, &edit).await?,
            "Concurrent view default change accepted as current"
        );
        ensure!(
            serde_json::to_value(edit.original())? == original,
            "Original view snapshot was mutated"
        );
        apply(&db, &plan_edit(&after, patch)?).await?;
        exec(&db, &format!("INSERT INTO {ns}.v DEFAULT VALUES")).await?;
        ensure!(
            scalar(&db, &format!("SELECT id FROM {ns}.t")).await? == "2",
            "Fresh view replacement lost the new default"
        );
    } else {
        exec(&db,&format!("CREATE TABLE {ns}.src(id INTEGER); CREATE TABLE {ns}.dst(id INTEGER); INSERT INTO {ns}.src VALUES(1),(2)")).await?;
        let report = explain::execute(
            &db,
            &format!("INSERT INTO {ns}.dst SELECT id FROM {ns}.src"),
        )
        .await?;
        let raw: serde_json::Value = serde_json::from_str(&report.raw_json)?;
        ensure!(
            raw["query_block"]["insert_from"].is_object(),
            "Fixture did not produce the native insert_from wrapper"
        );
        fn find<'a>(
            nodes: &'a [explain::PlanNode],
            operation: Option<&str>,
            relation: Option<&str>,
        ) -> Option<&'a explain::PlanNode> {
            nodes.iter().find_map(|n| {
                if operation.is_none_or(|o| n.operation == o)
                    && relation.is_none_or(|r| n.relation.as_deref() == Some(r))
                {
                    Some(n)
                } else {
                    find(&n.children, operation, relation)
                }
            })
        }
        let wrapper = find(&report.roots, Some("insert_from"), None)
            .context("Normalized insert_from wrapper missing")?;
        let source = find(&wrapper.children, None, Some("src"))
            .context("Normalized INSERT source subtree missing")?;
        ensure!(
            source.access.is_some() && source.actual.is_empty(),
            "Source access/estimate classification missing"
        );
        ensure!(
            source
                .estimates
                .get("rows_examined_per_scan")
                .map(|n| n.0.as_str())
                == Some(
                    raw["query_block"]["insert_from"]["table"]["rows_examined_per_scan"]
                        .to_string()
                        .as_str()
                ),
            "Source row estimate not preserved exactly"
        );
        ensure!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("Unnormalized MySQL field insert_from")),
            "Standard insert_from still classified as unknown"
        );
        ensure!(
            scalar(&db, &format!("SELECT COUNT(*) FROM {ns}.dst")).await? == "0",
            "Plain EXPLAIN executed INSERT"
        );
        ensure!(
            scalar(&db, &format!("SELECT COUNT(*) FROM {ns}.src")).await? == "2",
            "Plain EXPLAIN changed source rows"
        );
    }
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
    Ok(())
}
async fn run_review_regression(engine: Engine) -> Result<()> {
    let schema = format!(
        "pass12_admin_{:x}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    review_regression(engine, &schema).await.with_context(|| {
        format!("Review-regression schema {schema}; failed fixtures retained for diagnosis")
    })
}
#[tokio::test]
#[ignore = "Requires parent-owned PostgreSQL 127.0.0.1:15435"]
async fn postgres_view_default_change_invalidates_object_edit() -> Result<()> {
    run_review_regression(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Requires parent-owned MySQL 127.0.0.1:13309"]
async fn mysql_insert_explain_retains_source_without_inserting() -> Result<()> {
    run_review_regression(Engine::MySql).await
}
