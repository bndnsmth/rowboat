use anyhow::Result;
use rowboat_admin::{ddl::*, sqlite_schema as schema};
use rowboat_core::*;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.local")
            .join(format!(
                "pass16_schema_{}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
    fn profile(&self, read_only: bool) -> ConnectionProfile {
        ConnectionProfile {
            engine: Engine::Sqlite,
            read_only,
            file: Some(SqliteFileOptions {
                path: self.0.join("main.sqlite"),
                mode: if read_only {
                    SqliteOpenMode::OpenExisting
                } else {
                    SqliteOpenMode::CreateIfMissing
                },
            }),
            ..Default::default()
        }
    }
    async fn db(&self) -> Result<Database> {
        Database::connect(&self.profile(false), "").await
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn table(name: &str) -> TableRef {
    TableRef {
        schema: "main".into(),
        name: name.into(),
    }
}
fn column(name: &str) -> ColumnDesign {
    ColumnDesign {
        name: name.into(),
        data_type: DataType::Text,
        nullable: true,
        default: None,
    }
}
fn create(name: &str) -> DdlOperation {
    DdlOperation::CreateTable(TableDesign {
        table: table(name),
        columns: vec![
            ColumnDesign {
                name: "id".into(),
                data_type: DataType::Integer,
                nullable: false,
                default: None,
            },
            column("value"),
        ],
        primary_key: Some(PrimaryKeyDesign {
            name: "pk".into(),
            columns: vec!["id".into()],
        }),
        indexes: vec![],
        foreign_keys: vec![],
    })
}
async fn scalar(db: &Database, sql: &str) -> Result<String> {
    Ok(db.query(sql, 1).await?[0].rows[0][0].display())
}
async fn apply(db: &Database, operations: &[DdlOperation]) -> Result<()> {
    let review = schema::prepare(db, operations).await?;
    let outcome = schema::apply(db, &review).await?;
    assert_eq!(
        outcome.status,
        AdministrativeStatus::Committed,
        "{outcome:?}"
    );
    assert!(!db.in_transaction().await?);
    Ok(())
}

#[tokio::test]
async fn complete_native_workflow_and_inline_constraints() -> Result<()> {
    let f = Fixture::new()?;
    let db = f.db().await?;
    let operations = vec![
        create("t"),
        DdlOperation::AddCheck {
            table: table("t"),
            check: CheckDesign {
                name: "positive".into(),
                expression: "id > 0".into(),
            },
        },
        DdlOperation::AddUnique {
            table: table("t"),
            unique: UniqueDesign {
                name: "one_value".into(),
                columns: vec!["value".into()],
            },
        },
    ];
    let review = schema::prepare(&db, &operations).await?;
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM sqlite_schema WHERE name='t'").await?,
        "0"
    );
    assert_eq!(review.plan().statements.len(), 1);
    assert_eq!(
        schema::apply(&db, &review).await?.status,
        AdministrativeStatus::Committed
    );
    db.query("INSERT INTO t VALUES(1,'before')", 0).await?;
    assert!(db.query("INSERT INTO t VALUES(-1,'bad')", 0).await.is_err());
    apply(
        &db,
        &[DdlOperation::AddColumn {
            table: table("t"),
            column: ColumnDesign {
                default: Some(DefaultValue::Literal(Cell::Text("default".into()))),
                ..column("extra")
            },
        }],
    )
    .await?;
    apply(
        &db,
        &[DdlOperation::RenameColumn {
            table: table("t"),
            name: "extra".into(),
            new_name: "renamed".into(),
        }],
    )
    .await?;
    apply(
        &db,
        &[DdlOperation::CreateIndex {
            table: table("t"),
            index: IndexDesign {
                name: "idx".into(),
                columns: vec!["renamed".into()],
                unique: true,
            },
        }],
    )
    .await?;
    assert_eq!(scalar(&db, "SELECT renamed FROM t").await?, "default");
    apply(
        &db,
        &[
            DdlOperation::DropIndex {
                table: table("t"),
                name: "idx".into(),
            },
            DdlOperation::DropColumn {
                table: table("t"),
                name: "renamed".into(),
            },
        ],
    )
    .await?;
    apply(
        &db,
        &[DdlOperation::RenameTable {
            table: table("t"),
            new_name: "final".into(),
        }],
    )
    .await?;
    assert_eq!(scalar(&db, "SELECT value FROM final").await?, "before");
    apply(
        &db,
        &[DdlOperation::DropTable {
            table: table("final"),
            cascade: false,
        }],
    )
    .await?;
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM sqlite_schema WHERE name='final'").await?,
        "0"
    );
    Ok(())
}

#[tokio::test]
async fn quoting_attached_index_grammar_and_foreign_key_create() -> Result<()> {
    let f = Fixture::new()?;
    let db = f.db().await?;
    db.query("ATTACH ':memory:' AS \"aux';--\"", 0).await?;
    let name = "t\"; DROP TABLE sentinel; --";
    db.query("CREATE TABLE sentinel(id)", 0).await?;
    let mut design = match create(name) {
        DdlOperation::CreateTable(t) => t,
        _ => unreachable!(),
    };
    design.table.schema = "aux';--".into();
    design.indexes.push(IndexDesign {
        name: "i\";--".into(),
        columns: vec!["value".into()],
        unique: false,
    });
    apply(&db, &[DdlOperation::CreateTable(design.clone())]).await?;
    let inspected = schema::inspect(&db, &design.table).await?;
    assert!(
        inspected[2]
            .rows
            .iter()
            .any(|row| row[1].display() == "i\";--")
    );
    apply(
        &db,
        &[DdlOperation::DropIndex {
            table: design.table,
            name: "i\";--".into(),
        }],
    )
    .await?;
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*) FROM sqlite_schema WHERE name='sentinel'"
        )
        .await?,
        "1"
    );
    apply(&db, &[create("parent")]).await?;
    let mut child = match create("child") {
        DdlOperation::CreateTable(t) => t,
        _ => unreachable!(),
    };
    child.foreign_keys.push(ForeignKeyDesign {
        name: "parent_fk".into(),
        columns: vec!["id".into()],
        referenced_table: table("parent"),
        referenced_columns: vec!["id".into()],
        on_update: ReferentialAction::Cascade,
        on_delete: ReferentialAction::Cascade,
    });
    apply(&db, &[DdlOperation::CreateTable(child)]).await?;
    assert_eq!(
        schema::inspect(&db, &table("child")).await?[4].rows.len(),
        1
    );
    assert!(
        db.query("INSERT INTO child VALUES(1,'orphan')", 0)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn native_changes_preserve_strict_without_rowid_generated_indexes_triggers_and_rows()
-> Result<()> {
    let f = Fixture::new()?;
    let db = f.db().await?;
    db.query("CREATE TABLE t(id INTEGER PRIMARY KEY,value TEXT,derived TEXT GENERATED ALWAYS AS(value || '!') STORED) STRICT, WITHOUT ROWID; CREATE INDEX preserved ON t(value); CREATE TABLE audit(value TEXT); CREATE TRIGGER t_audit AFTER UPDATE OF value ON t BEGIN INSERT INTO audit VALUES(new.value); END; INSERT INTO t(id,value) VALUES(1,'one')",0).await?;
    let before=db.query("SELECT type,name,sql FROM sqlite_schema WHERE type IN ('index','trigger') ORDER BY name",100).await?[0].rows.clone();
    apply(
        &db,
        &[DdlOperation::AddColumn {
            table: table("t"),
            column: column("extra"),
        }],
    )
    .await?;
    apply(
        &db,
        &[DdlOperation::DropColumn {
            table: table("t"),
            name: "extra".into(),
        }],
    )
    .await?;
    assert_eq!(db.query("SELECT type,name,sql FROM sqlite_schema WHERE type IN ('index','trigger') ORDER BY name",100).await?[0].rows,before);
    assert_eq!(scalar(&db, "SELECT derived FROM t").await?, "one!");
    assert_eq!(
        scalar(
            &db,
            "SELECT wr || strict FROM pragma_table_list WHERE name='t'"
        )
        .await?,
        "11"
    );
    db.query("UPDATE t SET value='two'", 0).await?;
    assert_eq!(scalar(&db, "SELECT value FROM audit").await?, "two");
    let inspected = schema::inspect(&db, &table("t")).await?;
    assert_eq!(inspected[1].rows[2][6].display(), "3");
    assert!(!inspected[3].rows.is_empty());
    apply(
        &db,
        &[DdlOperation::RenameColumn {
            table: table("t"),
            name: "value".into(),
            new_name: "body".into(),
        }],
    )
    .await?;
    db.query("UPDATE t SET body='three'", 0).await?;
    assert_eq!(scalar(&db, "SELECT derived FROM t").await?, "three!");
    assert_eq!(
        scalar(&db, "SELECT value FROM audit ORDER BY rowid DESC LIMIT 1").await?,
        "three"
    );
    let review = schema::prepare(
        &db,
        &[
            DdlOperation::AddColumn {
                table: table("t"),
                column: column("rollback_me"),
            },
            DdlOperation::DropColumn {
                table: table("t"),
                name: "body".into(),
            },
        ],
    )
    .await?;
    assert_eq!(
        schema::apply(&db, &review).await?.status,
        AdministrativeStatus::RolledBack
    );
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM pragma_table_xinfo('t')").await?,
        "3"
    );
    assert_eq!(scalar(&db, "SELECT derived FROM t").await?, "three!");
    Ok(())
}

#[tokio::test]
async fn failed_native_ddl_rolls_back_all_prior_steps() -> Result<()> {
    let f = Fixture::new()?;
    let db = f.db().await?;
    apply(&db, &[create("t")]).await?;
    db.query("INSERT INTO t VALUES(1,'same'),(2,'same')", 0)
        .await?;
    let review = schema::prepare(
        &db,
        &[
            DdlOperation::AddColumn {
                table: table("t"),
                column: column("extra"),
            },
            DdlOperation::CreateIndex {
                table: table("t"),
                index: IndexDesign {
                    name: "bad_unique".into(),
                    columns: vec!["value".into()],
                    unique: true,
                },
            },
        ],
    )
    .await?;
    let outcome = schema::apply(&db, &review).await?;
    assert_eq!(
        outcome.status,
        AdministrativeStatus::RolledBack,
        "{outcome:?}"
    );
    assert!(outcome.rolled_back && outcome.reusable);
    assert_eq!(outcome.completed_steps, 1);
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM pragma_table_xinfo('t')").await?,
        "2"
    );
    assert_eq!(scalar(&db, "SELECT count(*) FROM t").await?, "2");
    assert!(!db.in_transaction().await?);
    Ok(())
}

#[tokio::test]
async fn stale_external_ddl_and_other_sessions_are_rejected() -> Result<()> {
    let f = Fixture::new()?;
    let db = f.db().await?;
    let other = f.db().await?;
    apply(&db, &[create("t")]).await?;
    let review = schema::prepare(
        &db,
        &[DdlOperation::AddColumn {
            table: table("t"),
            column: column("extra"),
        }],
    )
    .await?;
    let different = schema::apply(&other, &review).await?;
    assert_eq!(different.completed_steps, 0);
    assert_eq!(different.status, AdministrativeStatus::RolledBack);
    other.query("CREATE INDEX external ON t(value)", 0).await?;
    let stale = schema::apply(&db, &review).await?;
    assert_eq!(stale.completed_steps, 0);
    assert!(stale.error.unwrap().contains("stale"));
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM pragma_table_xinfo('t')").await?,
        "2"
    );
    Ok(())
}

#[tokio::test]
async fn attachment_incarnation_and_replaced_file_are_rejected() -> Result<()> {
    let f = Fixture::new()?;
    let db = f.db().await?;
    db.query("ATTACH ':memory:' AS aux; CREATE TABLE aux.t(id)", 0)
        .await?;
    let review = schema::prepare(
        &db,
        &[DdlOperation::AddColumn {
            table: TableRef {
                schema: "aux".into(),
                name: "t".into(),
            },
            column: column("extra"),
        }],
    )
    .await?;
    db.query(
        "DETACH aux; ATTACH ':memory:' AS aux; CREATE TABLE aux.t(id)",
        0,
    )
    .await?;
    assert_eq!(schema::apply(&db, &review).await?.completed_steps, 0);
    apply(&db, &[create("t")]).await?;
    let review = schema::prepare(
        &db,
        &[DdlOperation::AddColumn {
            table: table("t"),
            column: column("extra"),
        }],
    )
    .await?;
    std::fs::rename(f.0.join("main.sqlite"), f.0.join("old.sqlite"))?;
    let replacement = f.db().await?;
    replacement
        .query("CREATE TABLE t(id INTEGER PRIMARY KEY,value TEXT)", 0)
        .await?;
    // BEGIN itself may detect SQLITE_READONLY_DBMOVED before the snapshot check.
    match schema::apply(&db, &review).await {
        Ok(outcome) => assert_eq!(outcome.completed_steps, 0),
        Err(error) => assert!(!error.to_string().is_empty()),
    }
    assert_eq!(
        scalar(&replacement, "SELECT count(*) FROM pragma_table_xinfo('t')").await?,
        "2"
    );
    Ok(())
}

#[tokio::test]
async fn read_only_caller_transaction_flags_and_unsafe_dependencies_reject() -> Result<()> {
    let f = Fixture::new()?;
    let db = f.db().await?;
    apply(&db, &[create("t")]).await?;
    let operations = [DdlOperation::AddColumn {
        table: table("t"),
        column: column("extra"),
    }];
    let review = schema::prepare(&db, &operations).await?;
    db.query("BEGIN; INSERT INTO t VALUES(1,'caller')", 0)
        .await?;
    assert!(schema::prepare(&db, &operations).await.is_err());
    assert!(schema::apply(&db, &review).await.is_err());
    assert!(db.in_transaction().await?);
    assert_eq!(scalar(&db, "SELECT value FROM t").await?, "caller");
    db.query("ROLLBACK", 0).await?;
    let ro = Database::connect(&f.profile(true), "").await?;
    assert!(schema::apply(&ro, &review).await.is_err());
    db.query("PRAGMA legacy_alter_table=ON", 0).await?;
    assert!(schema::prepare(&db, &operations).await.is_err());
    assert_eq!(scalar(&db, "PRAGMA legacy_alter_table").await?, "1");
    db.query("PRAGMA legacy_alter_table=OFF; INSERT INTO t VALUES(1,'parent'); CREATE TABLE child(id REFERENCES t(id) ON DELETE CASCADE); INSERT INTO child VALUES(1)",0).await?;
    assert!(
        schema::prepare(
            &db,
            &[DdlOperation::DropTable {
                table: table("t"),
                cascade: false
            }]
        )
        .await
        .is_err()
    );
    assert_eq!(scalar(&db, "SELECT count(*) FROM child").await?, "1");
    db.query("CREATE VIEW view_t AS SELECT * FROM t", 0).await?;
    assert!(
        schema::prepare(
            &db,
            &[DdlOperation::AddColumn {
                table: table("view_t"),
                column: column("extra")
            }]
        )
        .await
        .is_err()
    );
    db.query("CREATE VIRTUAL TABLE docs USING fts5(content)", 0)
        .await?;
    for name in ["docs", "docs_data", "sqlite_schema"] {
        assert!(
            schema::prepare(
                &db,
                &[DdlOperation::DropTable {
                    table: table(name),
                    cascade: false
                }]
            )
            .await
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn unsupported_rebuilds_emit_no_sql() {
    for operation in [
        DdlOperation::SetColumnType {
            table: table("t"),
            name: "value".into(),
            data_type: DataType::Integer,
            using: None,
        },
        DdlOperation::SetColumnDefault {
            table: table("t"),
            name: "value".into(),
            default: None,
        },
        DdlOperation::SetColumnNullable {
            table: table("t"),
            name: "value".into(),
            nullable: false,
        },
        DdlOperation::AddCheck {
            table: table("t"),
            check: CheckDesign {
                name: "check".into(),
                expression: "1".into(),
            },
        },
        DdlOperation::RenameIndex {
            table: table("t"),
            name: "i".into(),
            new_name: "j".into(),
        },
    ] {
        assert!(schema::supported_operation(&operation).is_err());
        assert!(preview(Engine::Sqlite, &[operation]).is_err());
    }
    assert!(!schema::supported_action(schema::Action::AlterColumn));
    assert!(schema::supported_action(schema::Action::CreateIndex));
    assert!(DataType::parse(Engine::Sqlite, "blob").is_ok());
    assert!(DataType::parse(Engine::Sqlite, "integer); DROP TABLE t; --").is_err());
}

#[tokio::test]
async fn named_index_columns_never_become_sqlite_double_quoted_string_expressions() -> Result<()> {
    let f = Fixture::new()?;
    let db = f.db().await?;
    apply(&db, &[create("t")]).await?;
    let operation = DdlOperation::CreateIndex {
        table: table("t"),
        index: IndexDesign {
            name: "typo_index".into(),
            columns: vec!["not_a_column".into()],
            unique: false,
        },
    };
    if let Ok(review) = schema::prepare(&db, &[operation]).await {
        let outcome = schema::apply(&db, &review).await?;
        assert_eq!(
            outcome.status,
            AdministrativeStatus::RolledBack,
            "{outcome:?}"
        );
    }
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*) FROM sqlite_schema WHERE name='typo_index'"
        )
        .await?,
        "0"
    );
    Ok(())
}

#[tokio::test]
async fn cancelled_busy_apply_releases_lease_before_queued_peer() -> Result<()> {
    let f = Fixture::new()?;
    let db = f.db().await?;
    let blocker = f.db().await?;
    apply(&db, &[create("t")]).await?;
    let review = schema::prepare(
        &db,
        &[DdlOperation::AddColumn {
            table: table("t"),
            column: column("extra"),
        }],
    )
    .await?;
    blocker.query("BEGIN IMMEDIATE", 0).await?;
    let task_db = db.clone();
    let applying = tokio::spawn(async move { schema::apply(&task_db, &review).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let peer_db = db.clone();
    let peer = tokio::spawn(async move {
        scalar(&peer_db, "SELECT count(*) FROM pragma_table_xinfo('t')").await
    });
    applying.abort();
    blocker.query("ROLLBACK", 0).await?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), peer).await???,
        "2"
    );
    assert!(!db.in_transaction().await?);
    Ok(())
}
