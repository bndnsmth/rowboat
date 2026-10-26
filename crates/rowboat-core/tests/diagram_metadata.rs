//! Schema-only diagram reads against disposable, explicitly owned fixtures.
use anyhow::Result;
use rowboat_core::{database::DiagramCancellation, *};

fn table(schema: &str, name: &str) -> TableRef {
    TableRef {
        schema: schema.into(),
        name: name.into(),
    }
}

#[tokio::test]
async fn diagram_sqlite_composite_cycle_self_and_incoming_bound() -> Result<()> {
    let path = std::env::temp_dir().join(format!(
        "pass22_diagram_{}.sqlite",
        uuid::Uuid::new_v4().simple()
    ));
    let profile = ConnectionProfile {
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: path.clone(),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    let db = Database::connect(&profile, "").await?;
    db.query("CREATE TABLE pass22_a (tenant INTEGER, id INTEGER, parent INTEGER, PRIMARY KEY(tenant,id), FOREIGN KEY(tenant,parent) REFERENCES pass22_a(tenant,id), FOREIGN KEY(tenant,parent) REFERENCES pass22_b(tenant,id)); CREATE TABLE pass22_b (tenant INTEGER, id INTEGER, parent INTEGER, PRIMARY KEY(tenant,id), FOREIGN KEY(tenant,parent) REFERENCES pass22_a(tenant,id));",1).await?;
    let cancel = DiagramCancellation::default();
    let result = db
        .diagram_metadata(&table("main", "pass22_a"), &cancel)
        .await?;
    assert_eq!(result.columns.iter().filter(|c| c.primary_key).count(), 2);
    assert_eq!(result.relationships.len(), 3);
    assert!(result.relationships.iter().all(|r| r.columns.len() == 2));
    assert!(
        result
            .relationships
            .iter()
            .any(|r| r.source == r.referenced)
    );
    assert!(result.warnings.is_empty());
    for i in 0..35 {
        db.query(&format!("CREATE TABLE pass22_extra_{i:02} (tenant INTEGER, parent INTEGER, FOREIGN KEY(tenant,parent) REFERENCES pass22_a(tenant,id))"),1).await?;
    }
    let result = db
        .diagram_metadata(&table("main", "pass22_a"), &cancel)
        .await?;
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("Partial SQLite incoming"))
    );
    assert!(result.relationships.len() <= 34);
    // Outgoing keys of explicitly expanded tables are always available, even
    // when that table sorts beyond the incoming discovery budget.
    let result = db
        .diagram_metadata(&table("main", "pass22_extra_34"), &cancel)
        .await?;
    assert!(
        result
            .relationships
            .iter()
            .any(|r| r.source.name == "pass22_extra_34")
    );
    cancel.cancel();
    let failure = db
        .diagram_metadata(&table("main", "pass22_a"), &cancel)
        .await
        .unwrap_err();
    assert!(failure.cancelled);
    assert!(!failure.session_retired);
    assert!(
        db.diagram_metadata(
            &table("main", "pass22_missing"),
            &DiagramCancellation::default()
        )
        .await
        .is_err()
    );
    assert!(
        db.diagram_metadata(&table("", "pass22_a"), &DiagramCancellation::default())
            .await
            .is_err()
    );
    assert!(
        db.diagram_metadata(&table("main", "pass22_a"), &DiagramCancellation::default())
            .await
            .is_ok()
    );
    drop(db);
    std::fs::remove_file(path)?;
    Ok(())
}

async fn live(engine: Engine) -> Result<()> {
    let db = Database::connect(
        &ConnectionProfile {
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
        },
        "rowboat-test",
    )
    .await?;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let namespace = if engine == Engine::Postgres {
        format!("pass22_diagram_{}", &suffix[..8])
    } else {
        "rowboat".into()
    };
    if engine == Engine::Postgres {
        db.query(&format!("CREATE SCHEMA {namespace}"), 1).await?;
    }
    let a = table(&namespace, &format!("pass22_diagram_a_{}", &suffix[..8]));
    let b = table(
        if engine == Engine::Postgres {
            "public"
        } else {
            "rowboat"
        },
        &format!("pass22_diagram_b_{}", &suffix[..8]),
    );
    let qa = sql::qualified_table(engine, &a);
    let qb = sql::qualified_table(engine, &b);
    // Empty tables prove FK discovery is independent of selecting an actual row.
    db.query(&format!("CREATE TABLE {qa} (tenant INTEGER NOT NULL, id INTEGER NOT NULL, parent INTEGER, PRIMARY KEY(tenant,id), FOREIGN KEY(tenant,parent) REFERENCES {qa}(tenant,id)); CREATE TABLE {qb} (tenant INTEGER NOT NULL, id INTEGER NOT NULL, parent INTEGER, PRIMARY KEY(tenant,id), FOREIGN KEY(tenant,parent) REFERENCES {qa}(tenant,id)); ALTER TABLE {qa} ADD FOREIGN KEY(tenant,parent) REFERENCES {qb}(tenant,id);"),1).await?;
    let result = db
        .diagram_metadata(&a, &DiagramCancellation::default())
        .await?;
    assert_eq!(result.columns.iter().filter(|c| c.primary_key).count(), 2);
    assert_eq!(result.relationships.len(), 3, "{result:?}");
    assert!(result.relationships.iter().all(|r| r.columns.len() == 2
        && r.columns[0].source == "tenant"
        && r.columns[1].source == "parent"));
    assert!(
        result
            .relationships
            .iter()
            .any(|r| r.source == b && r.referenced == a)
    );
    assert!(
        result
            .relationships
            .iter()
            .any(|r| r.source == a && r.referenced == b)
    );
    assert!(
        result
            .relationships
            .iter()
            .any(|r| r.source == a && r.referenced == a)
    );
    if engine == Engine::Postgres {
        assert!(
            result
                .relationships
                .iter()
                .any(|r| r.source.schema != r.referenced.schema)
        );
    }
    let cancel = DiagramCancellation::default();
    cancel.cancel();
    assert!(
        db.diagram_metadata(&a, &cancel)
            .await
            .unwrap_err()
            .cancelled
    );
    assert!(
        db.diagram_metadata(&b, &DiagramCancellation::default())
            .await
            .is_ok()
    );
    // Parent owns server fixture cleanup, including these unique pass22 tables.
    Ok(())
}
#[tokio::test]
#[ignore = "requires parent-owned PostgreSQL fixture on 15435"]
async fn diagram_postgres_live_composite_cycle_cross_schema() -> Result<()> {
    live(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "requires parent-owned MySQL fixture on 13309"]
async fn diagram_mysql_live_composite_cycle() -> Result<()> {
    live(Engine::MySql).await
}
