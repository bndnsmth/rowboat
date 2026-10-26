//! Explicit local disposable fixtures only; test-owned pass12_core_* namespaces.
use anyhow::{Result, ensure};
use rowboat_core::{
    Cell, ConnectionProfile, Database, Engine, QueryEditPlan, QueryEditSource, QueryEvent,
    QueryResult, RowChange, sql::quote_ident,
};

async fn connect(engine: Engine) -> Result<Database> {
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
            ..Default::default()
        },
        "rowboat-test",
    )
    .await
}
fn name() -> String {
    format!(
        "pass12_core_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}
async fn setup(engine: Engine) -> Result<(Database, [String; 2])> {
    let db = connect(engine).await?;
    let p = name();
    let names = [format!("{p}_a"), format!("{p}_b")];
    for name in &names {
        let q = quote_ident(engine, name);
        db.query(
            &format!(
                "CREATE {} {q}",
                if engine == Engine::Postgres {
                    "SCHEMA"
                } else {
                    "DATABASE"
                }
            ),
            0,
        )
        .await?;
        db.query(&format!("CREATE TABLE {q}.records (tenant INT NOT NULL, id INT NOT NULL, payload VARCHAR(100), computed INT GENERATED ALWAYS AS (id+1) STORED, PRIMARY KEY(tenant,id))"), 0).await?;
        db.query(
            &format!(
                "INSERT INTO {q}.records (tenant,id,payload) VALUES (7,1,'first'),(7,2,'second')"
            ),
            0,
        )
        .await?;
        db.query(
            &format!("CREATE VIEW {q}.record_view AS SELECT * FROM {q}.records"),
            0,
        )
        .await?;
        db.query(
            &format!("CREATE TABLE {q}.no_key (id INT, payload VARCHAR(100))"),
            0,
        )
        .await?;
        db.query(&format!("INSERT INTO {q}.no_key VALUES (1,'not a key')"), 0)
            .await?;
    }
    select(&db, &names[0]).await?;
    Ok((db, names))
}
async fn select(db: &Database, name: &str) -> Result<()> {
    if db.engine() == Engine::MySql {
        db.select_database(name).await
    } else {
        db.query(
            &format!("SET search_path TO {}", quote_ident(db.engine(), name)),
            0,
        )
        .await?;
        Ok(())
    }
}
async fn cleanup(engine: Engine, names: &[String]) -> Result<()> {
    let db = connect(engine).await?;
    for name in names {
        db.query(
            &match engine {
                Engine::Sqlite => unreachable!("network fixture test"),
                Engine::Postgres => format!("DROP SCHEMA {} CASCADE", quote_ident(engine, name)),
                Engine::MySql => format!("DROP DATABASE {}", quote_ident(engine, name)),
            },
            0,
        )
        .await?;
    }
    Ok(())
}
async fn captured(db: &Database, sql: &str) -> Result<(Vec<QueryResult>, QueryEditSource)> {
    let (mut stream, capture) = db
        .query_stream_with_edit_source(sql, Default::default())
        .await?;
    let mut results: Vec<QueryResult> = Vec::new();
    let mut complete = false;
    while let Some(event) = stream.recv().await {
        match event {
            QueryEvent::Schema {
                result_index,
                columns,
                ..
            } => {
                ensure!(result_index == results.len());
                results.push(QueryResult {
                    columns,
                    ..Default::default()
                });
            }
            QueryEvent::Rows { result_index, rows } => results[result_index].rows.extend(rows),
            QueryEvent::ResultEnd {
                result_index,
                rows_affected,
                elapsed_ms,
                truncated,
                ..
            } => {
                results[result_index].rows_affected = rows_affected;
                results[result_index].elapsed_ms = elapsed_ms;
                results[result_index].truncated = truncated;
            }
            QueryEvent::Complete { .. } => complete = true,
            QueryEvent::Error { message, .. } => anyhow::bail!(message),
        }
    }
    ensure!(complete);
    Ok((results, capture.finish().await))
}
fn update(plan: &QueryEditPlan, row: usize, column: &str, value: &str) -> RowChange {
    RowChange::Update {
        original: plan
            .result()
            .columns
            .iter()
            .zip(&plan.result().rows[row])
            .map(|(c, v)| (c.name.clone(), v.clone()))
            .collect(),
        values: vec![(column.into(), Cell::Text(value.into()))],
    }
}
async fn scalar(db: &Database, sql: &str) -> Result<String> {
    Ok(db.query(sql, 1).await?[0].rows[0][0].display())
}

#[tokio::test]
#[ignore = "explicit PostgreSQL15435/MySQL13309 Podman fixtures"]
async fn projected_aliases_preview_and_context_switch_never_retarget_updates() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, names) = setup(engine).await?;
        let result: Result<()> = async {
            let label = quote_ident(engine,"Shown Value");
            let sql = format!("SELECT r.tenant AS account, r.id AS key_value, r.payload AS {label}, r.computed FROM records AS r WHERE r.tenant=7 AND r.id IN (1,2) ORDER BY r.id LIMIT 10");
            let (results,source) = captured(&db,&sql).await?;
            let plan = source.plan(&results[0])?;
            ensure!(plan.table().schema == names[0]);
            ensure!(plan.columns()[0].base_name == "tenant" && plan.columns()[0].primary_key);
            ensure!(plan.columns()[1].base_name == "id" && plan.columns()[1].primary_key);
            ensure!(plan.columns()[2].result_name == "Shown Value");
            ensure!(!plan.columns()[3].editable);
            ensure!(plan.result().columns[2].name == "payload");
            let changes = [update(&plan,0,"payload","reviewed ' text")];
            let preview = plan.preview_changes(&changes)?;
            ensure!(preview.starts_with("UPDATE ") && preview.contains(&quote_ident(engine,&names[0])) && preview.contains("WHERE"));
            ensure!(!preview.contains("Shown Value") && !preview.contains("key_value"));
            // Review is pure and cannot write.
            ensure!(scalar(&db,"SELECT payload FROM records WHERE tenant=7 AND id=1").await? == "first");
            select(&db,&names[1]).await?;
            ensure!(db.apply_query_changes(&plan,&changes).await? == 1);
            ensure!(scalar(&db,"SELECT payload FROM records WHERE tenant=7 AND id=1").await? == "first");
            ensure!(scalar(&db,&format!("SELECT payload FROM {}.records WHERE tenant=7 AND id=1",quote_ident(engine,&names[0]))).await? == "reviewed ' text");
            // Changed context does not change an already bound source. Independent
            // workspace handles cannot adopt its plan even on the same server.
            let other = connect(engine).await?;
            ensure!(other.apply_query_changes(&plan,&changes).await.is_err());
            ensure!(plan.preview_changes(&[update(&plan,0,"computed","99")]).is_err());
            ensure!(plan.preview_changes(&[update(&plan,0,"unprojected","x")]).is_err());
            let (all,source) = captured(&db,"SELECT records.* FROM records ORDER BY id").await?;
            ensure!(source.plan(&all[0])?.columns().len() == 4);
            Ok(())
        }.await;
        let cleaned = cleanup(engine, &names).await;
        result?;
        cleaned?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "explicit PostgreSQL15435/MySQL13309 Podman fixtures"]
async fn unsupported_queries_keys_nulls_duplicates_and_generated_columns_explain_readonly()
-> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, names) = setup(engine).await?;
        let result: Result<()> = async {
            for sql in [
                "SELECT id,payload FROM records",   // incomplete composite key
                "SELECT payload AS id FROM no_key", // label is not proof
                "SELECT a.* FROM records a JOIN records b ON a.id=b.id AND a.tenant=b.tenant",
                "SELECT tenant,id+1 AS id,payload FROM records",
                "SELECT count(*) AS id FROM records",
                "WITH x AS (SELECT * FROM records) SELECT * FROM x",
                "SELECT * FROM records UNION ALL SELECT * FROM records",
                "SELECT * FROM record_view",
                "SELECT tenant,id,id AS another_id FROM records",
                "SELECT tenant AS same,id AS same FROM records",
                "SELECT * FROM records; SELECT * FROM records",
                "SELECT * FROM records WHERE abs(id)=1",
            ] {
                let (results, source) = captured(&db, sql).await?;
                let error = source.plan(&results[0]).unwrap_err();
                ensure!(!error.to_string().is_empty(), "{sql}");
            }
            let (results, source) = captured(&db, "SELECT * FROM records ORDER BY id").await?;
            let mut duplicate = results[0].clone();
            duplicate.rows.push(duplicate.rows[0].clone());
            ensure!(
                source
                    .plan(&duplicate)
                    .unwrap_err()
                    .to_string()
                    .contains("exact captured execution")
            );
            let mut null = results[0].clone();
            null.rows[0][0] = Cell::Null;
            ensure!(
                source
                    .plan(&null)
                    .unwrap_err()
                    .to_string()
                    .contains("exact captured execution")
            );
            let plan = source.plan(&results[0])?;
            let mut forged = update(&plan, 0, "payload", "new");
            if let RowChange::Update { original, .. } = &mut forged {
                original[2].1 = Cell::Text("forged".into());
            }
            ensure!(plan.preview_changes(&[forged]).is_err());
            // A caller-owned transaction remains usable; metadata capture does
            // not execute potentially failing source lookups inside it.
            db.begin().await?;
            let (results, source) = captured(&db, "SELECT * FROM records").await?;
            ensure!(source.plan(&results[0]).is_err() && db.in_transaction().await?);
            db.rollback().await?;
            Ok(())
        }
        .await;
        let cleaned = cleanup(engine, &names).await;
        result?;
        cleaned?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "explicit PostgreSQL15435/MySQL13309 Podman fixtures"]
async fn conflict_is_atomic_and_dropped_recreated_sources_are_rejected() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, names) = setup(engine).await?;
        let result: Result<()> = async {
            let (results,source) = captured(&db,"SELECT tenant,id,payload FROM records ORDER BY id").await?;
            let plan = source.plan(&results[0])?;
            let other = connect(engine).await?;
            let table = format!("{}.records",quote_ident(engine,&names[0]));
            other.query(&format!("UPDATE {table} SET payload='concurrent' WHERE tenant=7 AND id=2"),0).await?;
            let changes = [update(&plan,0,"payload","would rollback"),update(&plan,1,"payload","stale")];
            ensure!(db.apply_query_changes(&plan,&changes).await.unwrap_err().to_string().contains("Conflict"));
            ensure!(scalar(&db,"SELECT payload FROM records WHERE tenant=7 AND id=1").await? == "first");
            ensure!(scalar(&db,"SELECT payload FROM records WHERE tenant=7 AND id=2").await? == "concurrent");
            ensure!(!db.in_transaction().await?);
            // Drop dependent view, then recreate exactly the same base-table DDL.
            other.query(&format!("DROP VIEW {}.record_view",quote_ident(engine,&names[0])),0).await?;
            other.query(&format!("DROP TABLE {table}"),0).await?;
            other.query(&format!("CREATE TABLE {table} (tenant INT NOT NULL, id INT NOT NULL, payload VARCHAR(100), computed INT GENERATED ALWAYS AS (id+1) STORED, PRIMARY KEY(tenant,id))"),0).await?;
            other.query(&format!("INSERT INTO {table} (tenant,id,payload) VALUES (7,1,'first'),(7,2,'second')"),0).await?;
            ensure!(db.apply_query_changes(&plan,&changes[..1]).await.unwrap_err().to_string().contains("identity or definition changed"));
            ensure!(scalar(&db,"SELECT payload FROM records WHERE tenant=7 AND id=1").await? == "first");
            Ok(())
        }.await;
        let cleaned = cleanup(engine, &names).await;
        result?;
        cleaned?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "explicit PostgreSQL15435/MySQL13309 Podman fixtures"]
async fn source_inference_never_executes_functions_or_replays_original_select() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, names) = setup(engine).await?;
        let result: Result<()> = async {
            let schema = quote_ident(engine,&names[0]);
            db.query("CREATE TABLE hits (n INT NOT NULL)",0).await?;
            db.query("INSERT INTO hits VALUES (0)",0).await?;
            let create = match engine {
                Engine::Sqlite => unreachable!("network fixture test"),
                Engine::Postgres => format!("CREATE FUNCTION {schema}.bump() RETURNS INTEGER LANGUAGE plpgsql AS $$ BEGIN UPDATE {schema}.hits SET n=n+1; RETURN 1; END $$"),
                Engine::MySql => format!("CREATE FUNCTION {schema}.bump() RETURNS INTEGER DETERMINISTIC MODIFIES SQL DATA BEGIN UPDATE {schema}.hits SET n=n+1; RETURN 1; END"),
            };
            db.query(&create,0).await?;
            let (results,source) = captured(&db,&format!("SELECT {schema}.bump() AS id FROM records WHERE tenant=7 AND id=1")).await?;
            ensure!(source.plan(&results[0]).is_err());
            ensure!(scalar(&db,"SELECT n FROM hits").await? == "1");
            let (results,source) = captured(&db,"SELECT tenant,id,payload FROM records WHERE tenant=7 AND id=1").await?;
            let plan = source.plan(&results[0])?;
            db.apply_query_changes(&plan,&[update(&plan,0,"payload","updated")]).await?;
            ensure!(scalar(&db,"SELECT n FROM hits").await? == "1");
            Ok(())
        }.await;
        let cleaned = cleanup(engine, &names).await;
        result?;
        cleaned?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "explicit PostgreSQL15435/MySQL13309 Podman fixtures"]
async fn execution_binding_rejects_unrelated_mutated_and_reordered_results() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let (db, names) = setup(engine).await?;
        let result: Result<()> = async {
            select(&db, &names[1]).await?;
            let (own, source) = captured(&db, "SELECT r.id AS tenant_alias, r.tenant AS id_alias, r.payload AS shown FROM records r WHERE r.tenant=7 AND r.id=1").await?;
            let unrelated = db.query(&format!("SELECT r.id AS tenant_alias, r.tenant AS id_alias, r.payload AS shown FROM {}.records r WHERE r.tenant=7 AND r.id=2", quote_ident(engine, &names[0])), 100).await?;
            ensure!(source.plan(&unrelated[0]).is_err(), "Accepted another query's real A/id=2 result as captured B/id=1");
            let plan = source.plan(&own[0])?;
            ensure!(plan.result().rows.len() == 1 && plan.result().rows[0][0].display() == "1");
            let mut changed = own[0].clone(); changed.rows[0][2] = Cell::Text("changed".into());
            ensure!(source.plan(&changed).is_err(), "Accepted mutated payload");
            changed = own[0].clone(); changed.rows[0][2] = Cell::Binary(b"first".to_vec());
            ensure!(source.plan(&changed).is_err(), "Accepted a changed Cell variant");
            changed = own[0].clone(); changed.rows_affected += 1;
            ensure!(source.plan(&changed).is_err(), "Accepted changed result status");
            changed = own[0].clone(); changed.truncated = !changed.truncated;
            ensure!(source.plan(&changed).is_err(), "Accepted changed truncation status");
            changed = own[0].clone(); changed.elapsed_ms += 1;
            ensure!(source.plan(&changed).is_err(), "Accepted changed completion metadata");
            changed = own[0].clone(); changed.columns[0].primary_key = !changed.columns[0].primary_key;
            ensure!(source.plan(&changed).is_err(), "Accepted changed column metadata");
            let (all, source) = captured(&db, "SELECT tenant,id,payload FROM records ORDER BY id").await?;
            ensure!(source.plan(&all[0]).is_ok());
            let mut reordered = all[0].clone(); reordered.rows.reverse();
            ensure!(source.plan(&reordered).is_err(), "Accepted reordered rows");
            let mut subset = all[0].clone(); subset.rows.pop();
            ensure!(source.plan(&subset).is_err(), "Accepted missing emitted rows");
            ensure!(scalar(&db, "SELECT payload FROM records WHERE tenant=7 AND id=2").await? == "second");
            Ok(())
        }.await;
        let cleaned = cleanup(engine, &names).await;
        result?;
        cleaned?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "explicit MySQL13309 Podman fixture"]
async fn mysql_owned_capture_and_apply_override_chain_and_release_without_changing_mode()
-> Result<()> {
    let engine = Engine::MySql;
    let (db, names) = setup(engine).await?;
    let result: Result<()> = async {
        for mode in [1, 2] {
            let expected_mode = if mode == 1 { "CHAIN" } else { "RELEASE" };
            db.query(&format!("SET SESSION completion_type={mode}"), 0).await?;
            let (rows, source) = captured(&db, "SELECT tenant,id,payload FROM records WHERE tenant=7 AND id=1").await?;
            let plan = source.plan(&rows[0])?;
            ensure!(!db.in_transaction().await?, "completion_type={mode} leaked a transaction after capture");
            ensure!(scalar(&db, "SELECT @@session.completion_type").await? == expected_mode);
            ensure!(db.apply_query_changes(&plan, &[update(&plan, 0, "payload", &format!("mode {mode}"))]).await? == 1);
            ensure!(!db.in_transaction().await?, "completion_type={mode} leaked a transaction after Apply");
            ensure!(scalar(&db, "SELECT @@session.completion_type").await? == expected_mode);

            db.query("SET SESSION completion_type=0", 0).await?;
            let (rows, source) = captured(&db, "SELECT tenant,id,payload FROM records WHERE tenant=7 AND id=1").await?;
            let plan = source.plan(&rows[0])?;
            db.query(&format!("SET SESSION completion_type={mode}"), 0).await?;
            let expected = format!("changed mode before apply {mode}");
            ensure!(db.apply_query_changes(&plan, &[update(&plan, 0, "payload", &expected)]).await? == 1);
            ensure!(!db.in_transaction().await?, "completion_type changed before Apply leaked a transaction");
            ensure!(scalar(&db, "SELECT @@session.completion_type").await? == expected_mode);
            let observer = connect(engine).await?;
            ensure!(scalar(&observer, &format!("SELECT payload FROM {}.records WHERE tenant=7 AND id=1", quote_ident(engine, &names[0]))).await? == expected);

            // Mode can change between capture and Apply; original-value conflict
            // must roll back without chaining or releasing the physical session.
            db.query("SET SESSION completion_type=0", 0).await?;
            let (rows, source) = captured(&db, "SELECT tenant,id,payload FROM records WHERE tenant=7 AND id=2").await?;
            let plan = source.plan(&rows[0])?;
            db.query(&format!("SET SESSION completion_type={mode}; UPDATE records SET payload='concurrent {mode}' WHERE tenant=7 AND id=2"), 0).await?;
            ensure!(db.apply_query_changes(&plan, &[update(&plan, 0, "payload", "stale")]).await.is_err());
            ensure!(!db.in_transaction().await?, "completion_type={mode} leaked a transaction after conflict rollback");
            ensure!(scalar(&db, "SELECT @@session.completion_type").await? == expected_mode);

            let (mut stream, capture) = db.query_stream_with_edit_source("SELECT * FROM records WHERE missing_column=1", Default::default()).await?;
            let mut failed = false;
            while let Some(event) = stream.recv().await { failed |= matches!(event, QueryEvent::Error { .. }); }
            ensure!(failed && capture.finish().await.read_only_reason().is_some());
            ensure!(!db.in_transaction().await?, "completion_type={mode} leaked a transaction after capture rollback");
            let (mut stream, capture) = db.query_stream_with_edit_source("SELECT * FROM records", rowboat_core::QueryStreamOptions { batch_rows: 1, channel_capacity: 1, ..Default::default() }).await?;
            ensure!(matches!(stream.recv().await, Some(QueryEvent::Schema { .. })));
            drop(stream);
            ensure!(tokio::time::timeout(std::time::Duration::from_secs(10), capture.finish()).await?.read_only_reason().is_some());
            ensure!(!db.in_transaction().await?, "completion_type={mode} leaked a transaction after dropped capture");
            ensure!(scalar(&db, "SELECT @@session.completion_type").await? == expected_mode);
        }
        Ok(())
    }.await;
    let cleaned = cleanup(engine, &names).await;
    result?;
    cleaned?;
    Ok(())
}
