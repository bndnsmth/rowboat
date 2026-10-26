use anyhow::Result;
use rowboat_core::*;

fn profile(engine: Engine) -> ConnectionProfile {
    ConnectionProfile {
        name: "Rowboat disposable integration database".into(),
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

fn text(value: impl ToString) -> Cell {
    Cell::Text(value.to_string())
}

async fn exercise(engine: Engine) -> Result<()> {
    let db = Database::connect(&profile(engine), "rowboat-test").await?;
    let table = TableRef {
        schema: if engine == Engine::Postgres {
            "public"
        } else {
            "rowboat"
        }
        .into(),
        name: "rowboat_integration_rows".into(),
    };
    let qualified = rowboat_core::sql::qualified_table(engine, &table);
    db.query(&format!("DROP TABLE IF EXISTS {qualified}"), 100)
        .await?;
    db.query(&format!("CREATE TABLE {qualified} (id INTEGER PRIMARY KEY, name VARCHAR(255) NOT NULL, note TEXT NULL, amount DECIMAL(20,4), active BOOLEAN NOT NULL DEFAULT TRUE)"), 100).await?;

    let catalog = db.catalog().await?;
    assert!(catalog.iter().any(|item| item.table == table));
    let structure = db.structure(&table).await?;
    assert_eq!(structure.columns.len(), 5);
    assert!(
        structure
            .columns
            .iter()
            .any(|column| column.name == "id" && column.primary_key)
    );

    let attack = "O'Reilly; DROP TABLE rowboat_integration_rows; --";
    assert_eq!(
        db.apply_changes(
            &table,
            &[
                RowChange::Insert {
                    values: vec![
                        ("id".into(), text(1)),
                        ("name".into(), text(attack)),
                        ("note".into(), Cell::Null),
                        ("amount".into(), text("1234567890123456.7890"))
                    ]
                },
                RowChange::Insert {
                    values: vec![
                        ("id".into(), text(2)),
                        ("name".into(), text("NULL")),
                        ("note".into(), text("line one\nline two,quoted \" text"))
                    ]
                },
            ]
        )
        .await?,
        2
    );

    let request = BrowseRequest {
        limit: 50,
        sort: Some(("id".into(), true)),
        ..Default::default()
    };
    let rows = db.browse(&table, &request).await?;
    assert_eq!(rows.rows.len(), 2);
    let id_idx = rows.columns.iter().position(|c| c.name == "id").unwrap();
    let name_idx = rows.columns.iter().position(|c| c.name == "name").unwrap();
    let note_idx = rows.columns.iter().position(|c| c.name == "note").unwrap();
    let amount_idx = rows
        .columns
        .iter()
        .position(|c| c.name == "amount")
        .unwrap();
    let row = rows.rows.iter().find(|r| r[id_idx] == text(1)).unwrap();
    assert_eq!(row[name_idx], text(attack));
    assert_eq!(row[note_idx], Cell::Null);
    assert_eq!(row[amount_idx], text("1234567890123456.7890"));

    let filtered = db
        .browse(
            &table,
            &BrowseRequest {
                filters: vec![Filter {
                    column: "name".into(),
                    op: FilterOp::Eq,
                    value: text(attack),
                }],
                limit: 10,
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(filtered.rows.len(), 1);
    let original: Vec<_> = rows
        .columns
        .iter()
        .zip(row)
        .map(|(c, v)| (c.name.clone(), v.clone()))
        .collect();
    assert_eq!(
        db.apply_changes(
            &table,
            &[RowChange::Update {
                original: original.clone(),
                values: vec![("name".into(), text("updated"))]
            }]
        )
        .await?,
        1
    );
    let conflict = db
        .apply_changes(
            &table,
            &[RowChange::Update {
                original,
                values: vec![("name".into(), text("stale overwrite"))],
            }],
        )
        .await;
    assert!(conflict.is_err(), "stale writes must be rejected");

    // A failing batch must not leave its first insert committed.
    let failed = db
        .apply_changes(
            &table,
            &[
                RowChange::Insert {
                    values: vec![
                        ("id".into(), text(3)),
                        ("name".into(), text("must roll back")),
                    ],
                },
                RowChange::Insert {
                    values: vec![("id".into(), text(2)), ("name".into(), text("duplicate"))],
                },
            ],
        )
        .await;
    assert!(failed.is_err());
    assert_eq!(db.browse(&table, &request).await?.rows.len(), 2);

    db.begin().await?;
    db.query(
        &format!("INSERT INTO {qualified} (id,name) VALUES (4,'transaction')"),
        100,
    )
    .await?;
    db.rollback().await?;
    assert_eq!(db.browse(&table, &request).await?.rows.len(), 2);
    db.begin().await?;
    db.query(
        &format!("INSERT INTO {qualified} (id,name) VALUES (5,'committed')"),
        100,
    )
    .await?;
    db.commit().await?;
    assert_eq!(db.browse(&table, &request).await?.rows.len(), 3);

    let multiple = db
        .query("SELECT 11 AS result_a; SELECT 22 AS result_b;", 100)
        .await?;
    assert!(
        multiple
            .iter()
            .any(|r| r.rows.iter().any(|row| row.contains(&text(11))))
    );
    assert!(
        multiple
            .iter()
            .any(|r| r.rows.iter().any(|row| row.contains(&text(22))))
    );
    let capped = db
        .query(&format!("SELECT * FROM {qualified} ORDER BY id"), 1)
        .await?;
    let capped = capped.iter().find(|r| !r.columns.is_empty()).unwrap();
    assert_eq!(capped.rows.len(), 1);
    assert!(capped.truncated);

    let mut readonly_profile = profile(engine);
    readonly_profile.read_only = true;
    let readonly = Database::connect(&readonly_profile, "rowboat-test").await?;
    assert!(
        readonly
            .query(&format!("DELETE FROM {qualified}"), 100)
            .await
            .is_err()
    );

    let latest = db.browse(&table, &request).await?;
    let first = &latest.rows[0];
    let original = latest
        .columns
        .iter()
        .zip(first)
        .map(|(c, v)| (c.name.clone(), v.clone()))
        .collect();
    assert_eq!(
        db.apply_changes(&table, &[RowChange::Delete { original }])
            .await?,
        1
    );
    assert_eq!(db.browse(&table, &request).await?.rows.len(), 2);

    db.query(&format!("DROP TABLE {qualified}"), 100).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires task-owned Podman databases: mise run db:up"]
async fn postgres_workflows() -> Result<()> {
    exercise(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "Requires task-owned Podman databases: mise run db:up"]
async fn mysql_workflows() -> Result<()> {
    exercise(Engine::MySql).await
}

async fn cancellation(engine: Engine) -> Result<()> {
    let db = Database::connect(&profile(engine), "rowboat-test").await?;
    let worker = db.clone();
    let query = tokio::spawn(async move {
        worker
            .query(
                if engine == Engine::Postgres {
                    "SELECT pg_sleep(30)"
                } else {
                    "SELECT SLEEP(30)"
                },
                100,
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    db.cancel().await?;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), query).await??;
    assert!(outcome.is_err(), "cancelled operation must return an error");
    assert!(
        !db.query("SELECT 1", 100).await?.is_empty(),
        "session remains usable after cancellation"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Requires task-owned Podman databases"]
async fn postgres_cancel() -> Result<()> {
    cancellation(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "Requires task-owned Podman databases"]
async fn mysql_cancel() -> Result<()> {
    cancellation(Engine::MySql).await
}

async fn type_roundtrip(engine: Engine) -> Result<()> {
    let db = Database::connect(&profile(engine), "rowboat-test").await?;
    let sql = match engine {
        Engine::Postgres => {
            "SELECT 9223372036854775807::bigint AS large_int, 1234567890123456.7890::numeric(20,4) AS decimal_value, NULL::text AS missing, 'NULL'::text AS text_null, decode('0001ff','hex') AS bytes, '{\"a\":1}'::jsonb AS document, DATE '2026-10-15' AS day, 'hello 🌊'::text AS unicode_value"
        }
        Engine::MySql => {
            "SELECT CAST(9223372036854775807 AS SIGNED) AS large_int, CAST(1234567890123456.7890 AS DECIMAL(20,4)) AS decimal_value, NULL AS missing, 'NULL' AS text_null, UNHEX('0001ff') AS bytes, CAST('{\"a\":1}' AS JSON) AS document, DATE '2026-10-15' AS day, 'hello 🌊' AS unicode_value"
        }
        Engine::Sqlite => {
            unreachable!("network type fixtures only; SQLite has storage-class tests")
        }
    };
    let results = db.query(sql, 10).await?;
    let result = results.iter().find(|r| r.columns.len() == 8).unwrap();
    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    assert_eq!(row[0], text("9223372036854775807"));
    assert_eq!(row[1], text("1234567890123456.7890"));
    assert_eq!(row[2], Cell::Null);
    assert_eq!(row[3], text("NULL"));
    assert_eq!(row[4], Cell::Binary(vec![0, 1, 255]));
    assert!(row[5].display().contains("\"a\""));
    assert_eq!(row[6], text("2026-10-15"));
    assert_eq!(row[7], text("hello 🌊"));

    // Lossless exports must distinguish text NULL, SQL NULL, and binary bytes.
    let csv = rowboat_services::export_csv(result)?;
    let imported = rowboat_services::import_csv(&csv)?;
    assert_eq!(
        imported[0]
            .iter()
            .map(|(_, cell)| cell.clone())
            .collect::<Vec<_>>(),
        *row
    );
    let json = rowboat_services::export_json(result)?;
    let imported_json = rowboat_services::import_json(&json)?;
    for (name, cell) in &imported_json[0] {
        let index = result.columns.iter().position(|c| &c.name == name).unwrap();
        assert_eq!(cell, &row[index]);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "Requires task-owned Podman databases"]
async fn postgres_types_and_exports() -> Result<()> {
    type_roundtrip(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "Requires task-owned Podman databases"]
async fn mysql_types_and_exports() -> Result<()> {
    type_roundtrip(Engine::MySql).await
}

async fn composite_keys(engine: Engine) -> Result<()> {
    let db = Database::connect(&profile(engine), "rowboat-test").await?;
    let table = TableRef {
        schema: if engine == Engine::Postgres {
            "public"
        } else {
            "rowboat"
        }
        .into(),
        name: "rowboat_composite_test".into(),
    };
    let name = rowboat_core::sql::qualified_table(engine, &table);
    db.query(&format!("DROP TABLE IF EXISTS {name}; CREATE TABLE {name} (tenant INTEGER NOT NULL, id INTEGER NOT NULL, value TEXT, PRIMARY KEY (tenant,id)); INSERT INTO {name} VALUES (1,1,'first'),(2,1,'second');"), 20).await?;
    assert!(
        db.apply_changes(
            &table,
            &[RowChange::Delete {
                original: vec![("id".into(), text(1))]
            }]
        )
        .await
        .is_err(),
        "partial primary key must not delete across tenants"
    );
    assert_eq!(
        db.apply_changes(
            &table,
            &[RowChange::Update {
                original: vec![
                    ("tenant".into(), text(1)),
                    ("id".into(), text(1)),
                    ("value".into(), text("first"))
                ],
                values: vec![("value".into(), text("one tenant only"))]
            }]
        )
        .await?,
        1
    );
    let rows = db
        .browse(
            &table,
            &BrowseRequest {
                limit: 20,
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(rows.rows.len(), 2);
    assert!(rows.rows.iter().any(|row| row.contains(&text("second"))));
    db.query(&format!("DROP TABLE {name}"), 20).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires task-owned Podman databases"]
async fn postgres_composite_keys() -> Result<()> {
    composite_keys(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "Requires task-owned Podman databases"]
async fn mysql_composite_keys() -> Result<()> {
    composite_keys(Engine::MySql).await
}

async fn backup_roundtrip(engine: Engine) -> Result<()> {
    let mut admin_profile = profile(engine);
    if engine == Engine::MySql {
        admin_profile.username = "root".into();
    }
    let admin = Database::connect(&admin_profile, "rowboat-test").await?;
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let source_name = format!("rowboat_backup_src_{suffix}");
    let target_name = format!("rowboat_backup_dst_{suffix}");
    for name in [&source_name, &target_name] {
        admin
            .query(
                &format!(
                    "CREATE DATABASE {}",
                    rowboat_core::sql::quote_ident(engine, name)
                ),
                10,
            )
            .await?;
    }
    let mut source_profile = admin_profile.clone();
    source_profile.database = source_name.clone();
    let mut target_profile = admin_profile;
    target_profile.database = target_name.clone();
    let source = Database::connect(&source_profile, "rowboat-test").await?;
    source.query("CREATE TABLE backup_marker (id INTEGER PRIMARY KEY, note TEXT); INSERT INTO backup_marker VALUES (1,'Unicode 🌊 and quote '' preserved'),(2,NULL);", 10).await?;
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/test-backups");
    std::fs::create_dir_all(&directory)?;
    let destination = directory.join(format!("{source_name}.sql"));
    let runtime = rowboat_services::ClientRuntime::Podman {
        image: match engine {
            Engine::Postgres => "docker.io/library/postgres:18",
            Engine::MySql => "docker.io/library/mysql:8.4",
            Engine::Sqlite => unreachable!("server backup fixtures only"),
        }
        .into(),
    };
    rowboat_services::backup(&source_profile, "rowboat-test", &destination, &runtime).await?;
    assert!(std::fs::metadata(&destination)?.len() > 100);
    // Refuse overwriting an existing backup rather than silently truncating it.
    assert!(
        rowboat_services::backup(&source_profile, "rowboat-test", &destination, &runtime)
            .await
            .is_err()
    );
    rowboat_services::restore(&target_profile, "rowboat-test", &destination, &runtime).await?;
    let target = Database::connect(&target_profile, "rowboat-test").await?;
    let results = target
        .query("SELECT id,note FROM backup_marker ORDER BY id", 10)
        .await?;
    let result = results.iter().find(|r| r.rows.len() == 2).unwrap();
    assert_eq!(result.rows[0][1], text("Unicode 🌊 and quote ' preserved"));
    assert_eq!(result.rows[1][1], Cell::Null);
    drop(source);
    drop(target);
    for name in [&source_name, &target_name] {
        admin
            .query(
                &format!(
                    "DROP DATABASE {}",
                    rowboat_core::sql::quote_ident(engine, name)
                ),
                10,
            )
            .await?;
    }
    std::fs::remove_file(destination)?;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires task-owned Podman databases and cached client images"]
async fn postgres_backup_restore() -> Result<()> {
    backup_roundtrip(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "Requires task-owned Podman databases and cached client images"]
async fn mysql_backup_restore() -> Result<()> {
    backup_roundtrip(Engine::MySql).await
}

async fn admin_workflows(engine: Engine) -> Result<()> {
    use rowboat_admin::{ddl::*, objects::ObjectKind};
    let db = Database::connect(&profile(engine), "rowboat-test").await?;
    let table = TableRef {
        schema: if engine == Engine::Postgres {
            "public"
        } else {
            "rowboat"
        }
        .into(),
        name: "rowboat_designed_table".into(),
    };
    let qualified = rowboat_core::sql::qualified_table(engine, &table);
    db.query(&format!("DROP TABLE IF EXISTS {qualified}"), 20)
        .await?;
    let design = TableDesign {
        table: table.clone(),
        columns: vec![
            ColumnDesign {
                name: "id".into(),
                data_type: DataType::Integer,
                nullable: false,
                default: None,
            },
            ColumnDesign {
                name: "name".into(),
                data_type: DataType::VarChar(120),
                nullable: false,
                default: None,
            },
        ],
        primary_key: Some(PrimaryKeyDesign {
            name: "rowboat_designed_pk".into(),
            columns: vec!["id".into()],
        }),
        indexes: vec![IndexDesign {
            name: "rowboat_designed_name".into(),
            columns: vec!["name".into()],
            unique: false,
        }],
        foreign_keys: vec![],
    };
    let plan = preview(engine, &[DdlOperation::CreateTable(design.clone())])?;
    db.query(&plan.sql(), 20).await?;
    let structure = db.structure(&table).await?;
    assert_eq!(structure.columns.len(), 2);
    assert!(
        structure
            .indexes
            .iter()
            .any(|index| index.name == "rowboat_designed_name")
    );
    let mut desired = design.clone();
    desired.columns.push(ColumnDesign {
        name: "note".into(),
        data_type: DataType::Text,
        nullable: true,
        default: None,
    });
    let changes = rowboat_admin::diff::compare(engine, &[design], &[desired])?;
    db.query(&changes.sql(), 20).await?;
    assert_eq!(db.structure(&table).await?.columns.len(), 3);
    assert!(
        rowboat_admin::query::completion_candidates(&db, "rowboat_designed")
            .await?
            .iter()
            .any(|candidate| candidate.label == "rowboat_designed_table")
    );
    assert!(
        !rowboat_admin::query::explain(&db, &format!("SELECT * FROM {qualified}"))
            .await?
            .is_empty()
    );
    assert!(
        !rowboat_admin::metrics::sample(&db)
            .await?
            .metrics
            .is_empty()
    );
    assert!(
        !rowboat_admin::objects::connection_info(&db)
            .await?
            .is_empty()
    );
    assert!(
        !rowboat_admin::objects::server_activity(&db)
            .await?
            .is_empty()
    );
    assert!(!rowboat_admin::objects::namespaces(&db).await?.is_empty());

    let view = TableRef {
        schema: table.schema.clone(),
        name: "rowboat_designed_view".into(),
    };
    let view_name = rowboat_core::sql::qualified_table(engine, &view);
    db.query(
        &format!("CREATE OR REPLACE VIEW {view_name} AS SELECT id, name FROM {qualified}"),
        20,
    )
    .await?;
    assert!(
        rowboat_admin::objects::browse_objects(&db, ObjectKind::View)
            .await?
            .iter()
            .any(|object| object.name == view.name)
    );
    db.query(
        &format!("DROP VIEW {view_name}; DROP TABLE {qualified}"),
        20,
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires task-owned Podman databases"]
async fn postgres_admin_workflows() -> Result<()> {
    admin_workflows(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "Requires task-owned Podman databases"]
async fn mysql_admin_workflows() -> Result<()> {
    admin_workflows(Engine::MySql).await
}
