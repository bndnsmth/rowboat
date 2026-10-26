//! Opt-in checks against the parent task's disposable Podman fixtures only.
use anyhow::Result;
use rowboat_core::*;

async fn exercise(engine: Engine) -> Result<()> {
    let profile = ConnectionProfile {
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
    };
    let db = Database::connect(&profile, "rowboat-test").await?;
    let schema = if engine == Engine::Postgres {
        "public"
    } else {
        "rowboat"
    };
    let table = TableRef {
        schema: schema.into(),
        name: "rowboat_core_metadata_rows".into(),
    };
    let view = TableRef {
        schema: schema.into(),
        name: "rowboat_core_metadata_view".into(),
    };
    let name = sql::qualified_table(engine, &table);
    let view_name = sql::qualified_table(engine, &view);
    db.query(&format!("DROP VIEW IF EXISTS {view_name}; DROP TABLE IF EXISTS {name}; CREATE TABLE {name} (id INTEGER PRIMARY KEY, parent INTEGER NULL, name VARCHAR(30), CONSTRAINT rowboat_core_self_fk FOREIGN KEY(parent) REFERENCES {name}(id)); CREATE INDEX rowboat_core_name_idx ON {name}(name); INSERT INTO {name}(id,name) VALUES (2,'two'),(10,'ten'); CREATE VIEW {view_name} AS SELECT * FROM {name};"),10).await?;
    let catalog = db.catalog().await?;
    assert!(
        catalog.iter().any(|i| i.table == table),
        "table missing: {catalog:?}"
    );
    assert!(
        catalog
            .iter()
            .any(|i| i.table == view && i.kind.contains("VIEW")),
        "view missing: {catalog:?}"
    );
    let structure = db.structure(&table).await?;
    assert!(
        structure
            .foreign_keys
            .iter()
            .any(|fk| fk.name == "rowboat_core_self_fk"),
        "{structure:?}"
    );
    assert!(
        structure
            .indexes
            .iter()
            .any(|i| i.name == "rowboat_core_name_idx"),
        "{structure:?}"
    );
    assert!(
        db.structure(&view)
            .await?
            .definition
            .to_ascii_uppercase()
            .contains("VIEW")
    );
    let rows = db
        .browse(
            &table,
            &BrowseRequest {
                sort: Some(("id".into(), true)),
                limit: 10,
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(
        rows.rows[0][0],
        Cell::Text("2".into()),
        "numeric sort must not use display text"
    );
    db.begin().await?;
    db.apply_changes(
        &table,
        &[RowChange::Insert {
            values: vec![("id".into(), Cell::Text("3".into()))],
        }],
    )
    .await?;
    let failure = db
        .apply_changes(
            &table,
            &[
                RowChange::Insert {
                    values: vec![("id".into(), Cell::Text("4".into()))],
                },
                RowChange::Insert {
                    values: vec![("id".into(), Cell::Text("2".into()))],
                },
            ],
        )
        .await;
    assert!(failure.is_err());
    assert_eq!(
        db.browse(
            &table,
            &BrowseRequest {
                limit: 10,
                ..Default::default()
            }
        )
        .await?
        .rows
        .len(),
        3
    );
    db.rollback().await?;
    assert_eq!(
        db.browse(
            &table,
            &BrowseRequest {
                limit: 10,
                ..Default::default()
            }
        )
        .await?
        .rows
        .len(),
        2
    );
    let first = sql::quote_ident(engine, "first_value");
    // SQL-level transaction spellings must not let editing commit its caller.
    db.query(
        "START TRANSACTION; SAVEPOINT caller_point; ROLLBACK WORK TO SAVEPOINT caller_point",
        10,
    )
    .await?;
    assert!(
        db.begin().await.is_err(),
        "begin must not commit an existing MySQL transaction"
    );
    db.apply_changes(
        &table,
        &[RowChange::Insert {
            values: vec![("id".into(), Cell::Text("99".into()))],
        }],
    )
    .await?;
    db.rollback().await?;
    assert_eq!(
        db.browse(
            &table,
            &BrowseRequest {
                limit: 10,
                ..Default::default()
            }
        )
        .await?
        .rows
        .len(),
        2
    );
    if engine == Engine::MySql {
        db.query("SET autocommit = 0", 10).await?;
        db.apply_changes(
            &table,
            &[RowChange::Insert {
                values: vec![("id".into(), Cell::Text("98".into()))],
            }],
        )
        .await?;
        db.rollback().await?;
        assert_eq!(
            db.browse(
                &table,
                &BrowseRequest {
                    limit: 10,
                    ..Default::default()
                }
            )
            .await?
            .rows
            .len(),
            2
        );
        db.query("SET autocommit = 1", 10).await?;
    }
    let second = sql::quote_ident(engine, "second_value");
    let multiple = db
        .query(&format!("SELECT 11 AS {first}; SELECT 22 AS {second}"), 10)
        .await?;
    assert_eq!(multiple.len(), 2);
    assert_eq!(multiple[0].rows[0][0], Cell::Text("11".into()));
    assert_eq!(multiple[1].rows[0][0], Cell::Text("22".into()));
    let capped = db.query(&format!("SELECT * FROM {name}"), 1).await?;
    assert_eq!(capped[0].rows.len(), 1);
    assert!(capped[0].truncated);
    let readonly = Database::connect(
        &ConnectionProfile {
            read_only: true,
            ..profile
        },
        "rowboat-test",
    )
    .await?;
    assert!(
        readonly
            .query(&format!("DELETE FROM {name}"), 10)
            .await
            .is_err()
    );
    let original = rows
        .columns
        .iter()
        .zip(&rows.rows[0])
        .map(|(c, v)| (c.name.clone(), v.clone()))
        .collect();
    assert_eq!(
        db.apply_changes(&table, &[RowChange::Delete { original }])
            .await?,
        1
    );
    db.query(&format!("DROP VIEW {view_name}; DROP TABLE {name}"), 10)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires explicit task-owned Podman fixtures on ports 15435/13309"]
async fn mysql_metadata_sort_and_savepoints() -> Result<()> {
    exercise(Engine::MySql).await
}

#[tokio::test]
#[ignore = "Requires explicit task-owned Podman fixtures on ports 15435/13309"]
async fn postgres_metadata_sort_and_savepoints() -> Result<()> {
    exercise(Engine::Postgres).await
}
