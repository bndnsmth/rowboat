//! Only explicit, parent-owned disposable containers. Every run has a unique
//! namespace/account prefix. Success cleans up; failures retain evidence.
use anyhow::{Context, Result, ensure};
use rowboat_admin::{
    access::*,
    ddl::*,
    objects::{NamespaceOperation, namespace_sql},
};
use rowboat_core::{
    Cell, ConnectionProfile, Database, Engine, TableRef,
    sql::{qualified_table, quote_ident},
};
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
async fn apply(db: &Database, ops: Vec<DdlOperation>) -> Result<()> {
    let plan = preview(db.engine(), &ops)?;
    for (i, statement) in plan.statements.iter().enumerate() {
        db.query(&statement.sql, 100)
            .await
            .with_context(|| format!("DDL statement {i}: {}", statement.reason))?;
    }
    Ok(())
}
async fn apply_plan(db: &Database, plan: DdlPlan) -> Result<()> {
    for statement in plan.statements {
        db.query(&statement.sql, 100).await?;
    }
    Ok(())
}
async fn credential_plan(db: &Database, plan: DdlPlan) -> Result<()> {
    for statement in plan.statements {
        // Syntax errors may echo the failing SQL fragment: never print it for
        // statements containing credentials, including test fixture credentials.
        db.query(&statement.sql, 100).await.map_err(|_| {
            anyhow::anyhow!("Credential operation failed (server details redacted)")
        })?;
    }
    Ok(())
}
fn col(name: &str, ty: DataType, nullable: bool) -> ColumnDesign {
    ColumnDesign {
        name: name.into(),
        data_type: ty,
        nullable,
        default: None,
    }
}
fn design(table: TableRef) -> TableDesign {
    TableDesign {
        table,
        columns: vec![
            col("id", DataType::Integer, false),
            col("label", DataType::VarChar(100), true),
        ],
        primary_key: None,
        indexes: vec![],
        foreign_keys: vec![],
    }
}
fn text_exists(results: &[rowboat_core::QueryResult], needle: &str) -> bool {
    results
        .iter()
        .flat_map(|r| &r.rows)
        .flatten()
        .any(|c| matches!(c,Cell::Text(s) if s==needle))
}

async fn exercise(engine: Engine, tag: &str) -> Result<()> {
    let db = Database::connect(&profile(engine), "rowboat-test").await?;
    let schema = format!("{tag}_scope");
    apply_plan(
        &db,
        namespace_sql(
            engine,
            &NamespaceOperation::CreateSchema {
                name: schema.clone(),
            },
        )?,
    )
    .await?;
    let table = TableRef {
        schema: schema.clone(),
        name: "table\"`quoted".into(),
    };
    ensure!(
        text_exists(&rowboat_admin::objects::schemas(&db).await?, &schema),
        "Namespace metadata was not returned as text"
    );
    let parent = TableRef {
        schema: schema.clone(),
        name: "parent".into(),
    };
    let table_sql = qualified_table(engine, &table);
    apply(
        &db,
        vec![
            DdlOperation::CreateTable(design(table.clone())),
            DdlOperation::CreateTable(design(parent.clone())),
        ],
    )
    .await?;
    apply(
        &db,
        vec![
            DdlOperation::AddPrimaryKey {
                table: table.clone(),
                primary_key: PrimaryKeyDesign {
                    name: "pk_main".into(),
                    columns: vec!["id".into()],
                },
            },
            DdlOperation::AddPrimaryKey {
                table: parent.clone(),
                primary_key: PrimaryKeyDesign {
                    name: "pk_parent".into(),
                    columns: vec!["id".into()],
                },
            },
            DdlOperation::AddColumn {
                table: table.clone(),
                column: col("parent_id", DataType::Integer, true),
            },
            DdlOperation::AddForeignKey {
                table: table.clone(),
                foreign_key: ForeignKeyDesign {
                    name: "fk_parent".into(),
                    columns: vec!["parent_id".into()],
                    referenced_table: parent.clone(),
                    referenced_columns: vec!["id".into()],
                    on_update: ReferentialAction::Cascade,
                    on_delete: ReferentialAction::SetNull,
                },
            },
            DdlOperation::AddUnique {
                table: table.clone(),
                unique: UniqueDesign {
                    name: "unique\"`label".into(),
                    columns: vec!["label".into()],
                },
            },
            DdlOperation::AddCheck {
                table: table.clone(),
                check: CheckDesign {
                    name: "check\"`id".into(),
                    expression: format!("{} > 0", quote_ident(engine, "id")),
                },
            },
            DdlOperation::CreateIndex {
                table: table.clone(),
                index: IndexDesign {
                    name: "index\"`label".into(),
                    columns: vec!["label".into()],
                    unique: false,
                },
            },
            DdlOperation::RenameIndex {
                table: table.clone(),
                name: "index\"`label".into(),
                new_name: "index_renamed".into(),
            },
            DdlOperation::SetColumnDefault {
                table: table.clone(),
                name: "label".into(),
                default: Some(DefaultValue::Literal(Cell::Text("default's value".into()))),
            },
        ],
    )
    .await?;
    let catalog = constraints(&db, &table).await?;
    for row in catalog.iter().flat_map(|r| &r.rows) {
        ensure!(
            row.iter().all(|c| matches!(c, Cell::Text(_) | Cell::Null)),
            "Constraint text was returned as binary"
        );
    }
    ensure!(
        text_exists(&catalog, "FOREIGN KEY") && text_exists(&catalog, "PRIMARY KEY"),
        "Constraint kinds are not readable text"
    );
    let foreign = catalog
        .iter()
        .flat_map(|r| &r.rows)
        .find(|r| r[0] == Cell::Text("fk_parent".into()))
        .ok_or_else(|| anyhow::anyhow!("Missing FK metadata"))?;
    let Cell::Text(definition) = &foreign[2] else {
        anyhow::bail!("Missing FK definition")
    };
    ensure!(
        definition.contains("REFERENCES")
            && definition.contains("parent")
            && definition.contains("ON UPDATE CASCADE")
            && definition.contains("ON DELETE SET NULL"),
        "FK definition omitted target/actions"
    );
    for name in ["unique\"`label", "check\"`id", "fk_parent"] {
        ensure!(text_exists(&catalog, name), "Constraint not found: {name}");
    }
    db.query(&format!("INSERT INTO {table_sql} (id) VALUES (1)"), 10)
        .await?;
    ensure!(
        db.query(
            &format!("INSERT INTO {table_sql} (id,label) VALUES (-1,'bad')"),
            10
        )
        .await
        .is_err(),
        "CHECK was not enforced"
    );
    ensure!(
        db.query(&format!("INSERT INTO {table_sql} (id) VALUES (2)"), 10)
            .await
            .is_err(),
        "UNIQUE was not enforced"
    );
    let rows = db
        .query(&format!("SELECT label FROM {table_sql} WHERE id=1"), 10)
        .await?;
    ensure!(
        text_exists(&rows, "default's value"),
        "Literal default did not roundtrip"
    );
    apply(
        &db,
        vec![DdlOperation::SetColumnDefault {
            table: table.clone(),
            name: "label".into(),
            default: None,
        }],
    )
    .await?;
    if engine == Engine::Postgres {
        apply(
            &db,
            vec![
                DdlOperation::SetColumnType {
                    table: table.clone(),
                    name: "label".into(),
                    data_type: DataType::Text,
                    using: Some("label::text".into()),
                },
                DdlOperation::SetColumnNullable {
                    table: table.clone(),
                    name: "label".into(),
                    nullable: false,
                },
            ],
        )
        .await?;
    } else {
        let mut full = mysql_column_definition(&db, &table, "label").await?;
        full.column.data_type = DataType::VarChar(140);
        full.column.nullable = false;
        apply(
            &db,
            vec![DdlOperation::ModifyMySqlColumn {
                table: table.clone(),
                definition: full,
            }],
        )
        .await?;
        preservation(&db, &schema).await?;
    }
    apply(
        &db,
        vec![
            DdlOperation::DropForeignKey {
                table: table.clone(),
                name: "fk_parent".into(),
            },
            DdlOperation::DropCheck {
                table: table.clone(),
                name: "check\"`id".into(),
            },
            DdlOperation::DropUnique {
                table: table.clone(),
                name: "unique\"`label".into(),
            },
            DdlOperation::DropIndex {
                table: table.clone(),
                name: "index_renamed".into(),
            },
            DdlOperation::RenameColumn {
                table: table.clone(),
                name: "parent_id".into(),
                new_name: "old_parent".into(),
            },
            DdlOperation::DropColumn {
                table: table.clone(),
                name: "old_parent".into(),
            },
            DdlOperation::DropPrimaryKey {
                table: table.clone(),
                name: "pk_main".into(),
            },
            DdlOperation::AlterColumn {
                table: table.clone(),
                column: col("id", DataType::BigInt, false),
            },
        ],
    )
    .await?;
    accounts(&db, tag, &schema, &table).await?;
    apply(
        &db,
        vec![
            DdlOperation::RenameTable {
                table: table.clone(),
                new_name: "renamed".into(),
            },
            DdlOperation::DropTable {
                table: TableRef {
                    schema: schema.clone(),
                    name: "renamed".into(),
                },
                cascade: false,
            },
            DdlOperation::DropTable {
                table: parent,
                cascade: false,
            },
        ],
    )
    .await?;
    if engine == Engine::Postgres {
        let renamed = format!("{schema}_renamed");
        apply_plan(
            &db,
            namespace_sql(
                engine,
                &NamespaceOperation::RenameSchema {
                    name: schema.clone(),
                    new_name: renamed.clone(),
                },
            )?,
        )
        .await?;
        apply_plan(
            &db,
            namespace_sql(
                engine,
                &NamespaceOperation::DropSchema {
                    name: renamed,
                    cascade: false,
                },
            )?,
        )
        .await?;
        let database = format!("{tag}_db");
        let renamed = format!("{tag}_db2");
        apply_plan(
            &db,
            namespace_sql(
                engine,
                &NamespaceOperation::CreateDatabase {
                    name: database.clone(),
                },
            )?,
        )
        .await?;
        apply_plan(
            &db,
            namespace_sql(
                engine,
                &NamespaceOperation::RenameDatabase {
                    name: database,
                    new_name: renamed.clone(),
                },
            )?,
        )
        .await?;
        apply_plan(
            &db,
            namespace_sql(engine, &NamespaceOperation::DropDatabase { name: renamed })?,
        )
        .await?;
    } else {
        apply_plan(
            &db,
            namespace_sql(
                engine,
                &NamespaceOperation::DropSchema {
                    name: schema,
                    cascade: false,
                },
            )?,
        )
        .await?;
        let database = format!("{tag}_db");
        apply_plan(
            &db,
            namespace_sql(
                engine,
                &NamespaceOperation::CreateDatabase {
                    name: database.clone(),
                },
            )?,
        )
        .await?;
        apply_plan(
            &db,
            namespace_sql(engine, &NamespaceOperation::DropDatabase { name: database })?,
        )
        .await?;
    }
    Ok(())
}

async fn preservation(db: &Database, schema: &str) -> Result<()> {
    let table = TableRef {
        schema: schema.into(),
        name: "preserve".into(),
    };
    let name = qualified_table(Engine::MySql, &table);
    db.query(&format!("CREATE TABLE {name} (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY COMMENT 'identifier', label VARCHAR(80) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL DEFAULT 'original' COMMENT 'owner''s label', modified TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3), hidden INTEGER INVISIBLE DEFAULT 7, computed INTEGER GENERATED ALWAYS AS (hidden + 1) STORED)"),10).await?;
    let id = mysql_column_definition(db, &table, "id").await?;
    let label = mysql_column_definition(db, &table, "label").await?;
    let timestamp = mysql_column_definition(db, &table, "modified").await?;
    let hidden = mysql_column_definition(db, &table, "hidden").await?;
    ensure!(
        mysql_column_definition(db, &table, "computed")
            .await
            .is_err(),
        "Generated definition must not be edited lossily"
    );
    for mut full in [id.clone(), label.clone(), timestamp.clone(), hidden.clone()] {
        if full.column.name == "label" {
            full.column.data_type = DataType::VarChar(120);
        }
        apply(
            db,
            vec![DdlOperation::ModifyMySqlColumn {
                table: table.clone(),
                definition: full,
            }],
        )
        .await?;
    }
    let after_id = mysql_column_definition(db, &table, "id").await?;
    ensure!(after_id == id, "AUTO_INCREMENT/unsigned/comment changed");
    let after_label = mysql_column_definition(db, &table, "label").await?;
    ensure!(
        after_label.collation == label.collation
            && after_label.character_set == label.character_set
            && after_label.comment == label.comment,
        "Text attributes changed"
    );
    ensure!(
        mysql_column_definition(db, &table, "modified").await? == timestamp,
        "Timestamp DEFAULT/ON UPDATE changed"
    );
    ensure!(
        mysql_column_definition(db, &table, "hidden")
            .await?
            .invisible
            == hidden.invisible,
        "Visibility changed"
    );
    db.query(&format!("INSERT INTO {name} () VALUES ()"), 10)
        .await?;
    let result = db
        .query(
            &format!("SELECT id,label='original' AS label_preserved,hidden FROM {name}"),
            10,
        )
        .await?;
    ensure!(
        result[0].rows[0]
            == vec![
                Cell::Text("1".into()),
                Cell::Text("1".into()),
                Cell::Text("7".into())
            ],
        "Preserved defaults/auto increment do not execute correctly"
    );
    apply(
        db,
        vec![DdlOperation::SetColumnDefault {
            table: table.clone(),
            name: "label".into(),
            default: Some(DefaultValue::Literal(Cell::Text("targeted".into()))),
        }],
    )
    .await?;
    let after = mysql_column_definition(db, &table, "label").await?;
    ensure!(
        after.character_set == label.character_set
            && after.collation == label.collation
            && after.comment == label.comment,
        "Targeted default edit changed unrelated attributes"
    );
    apply(
        db,
        vec![DdlOperation::DropTable {
            table,
            cascade: false,
        }],
    )
    .await
}

async fn accounts(db: &Database, tag: &str, schema: &str, table: &TableRef) -> Result<()> {
    let engine = db.engine();
    let password = "Rowboat'\"\\; %_\n密碼42!".to_owned();
    let changed = "Next'\"\\\n密碼99!".to_owned();
    let name = format!("{tag}'\"`");
    let renamed = format!("{tag}renamed");
    let mut login = profile(engine);
    login.username = name.clone();
    if engine == Engine::Postgres {
        credential_plan(
            db,
            pg_role_sql(&PgRoleOperation::Create {
                name: name.clone(),
                options: PgRoleOptions {
                    login: true,
                    ..Default::default()
                },
                password: Some(password.clone()),
            })?,
        )
        .await?;
        let connection = Database::connect(&login, &password).await.map_err(|_| {
            anyhow::anyhow!("Special-character PostgreSQL password authentication failed")
        })?;
        drop(connection);
        apply_plan(
            db,
            pg_role_sql(&PgRoleOperation::Patch {
                name: name.clone(),
                options: PgRolePatch {
                    connection_limit: Some(3),
                    valid_until: Some("infinity".into()),
                    ..Default::default()
                },
            })?,
        )
        .await?;
        ensure!(
            text_exists(&pg_role_info(db, &name).await?, "3"),
            "Role connection limit not applied"
        );
        let group = format!("{tag}group");
        credential_plan(
            db,
            pg_role_sql(&PgRoleOperation::Create {
                name: group.clone(),
                options: PgRoleOptions::default(),
                password: None,
            })?,
        )
        .await?;
        apply_plan(
            db,
            pg_role_sql(&PgRoleOperation::GrantMembership {
                role: group.clone(),
                member: name.clone(),
                admin_option: true,
            })?,
        )
        .await?;
        ensure!(
            text_exists(&pg_role_memberships(db, &name).await?, &group),
            "Membership missing"
        );
        apply_plan(
            db,
            pg_role_sql(&PgRoleOperation::RevokeMembership {
                role: group.clone(),
                member: name.clone(),
            })?,
        )
        .await?;
        apply_plan(db, pg_role_sql(&PgRoleOperation::Drop { name: group })?).await?;
        for target in [
            GrantTarget::Schema(schema.into()),
            GrantTarget::Table(table.clone()),
        ] {
            let privilege = if matches!(target, GrantTarget::Schema(_)) {
                Privilege::Usage
            } else {
                Privilege::Select
            };
            let mut op = GrantOperation {
                target,
                grantee: Grantee::PgRole(name.clone()),
                privileges: vec![privilege],
                revoke: false,
                grant_option: true,
            };
            apply_plan(db, grant_sql(engine, &op)?).await?;
            ensure!(
                text_exists(&pg_grants(db, &name).await?, &name),
                "Direct grants missing"
            );
            op.revoke = true;
            op.grant_option = false;
            apply_plan(db, grant_sql(engine, &op)?).await?;
        }
        credential_plan(
            db,
            pg_role_sql(&PgRoleOperation::SetPassword {
                name: name.clone(),
                password: changed.clone(),
            })?,
        )
        .await?;
        let connection = Database::connect(&login, &changed)
            .await
            .map_err(|_| anyhow::anyhow!("Changed PostgreSQL password authentication failed"))?;
        drop(connection);
        credential_plan(
            db,
            pg_role_sql(&PgRoleOperation::Alter {
                name: name.clone(),
                options: PgRoleOptions {
                    login: true,
                    connection_limit: Some(5),
                    ..Default::default()
                },
                password: None,
            })?,
        )
        .await?;
        apply_plan(
            db,
            pg_role_sql(&PgRoleOperation::ClearPassword { name: name.clone() })?,
        )
        .await?;
        apply_plan(
            db,
            pg_role_sql(&PgRoleOperation::Rename {
                name: name.clone(),
                new_name: renamed.clone(),
            })?,
        )
        .await?;
        apply_plan(db, pg_role_sql(&PgRoleOperation::Drop { name: renamed })?).await?;
    } else {
        let account = MySqlAccount {
            user: name.clone(),
            host: "%".into(),
        };
        credential_plan(
            db,
            mysql_user_sql(
                &MySqlUserOperation::Create {
                    account: account.clone(),
                    password: password.clone(),
                    locked: false,
                },
                MySqlStringMode::BackslashEscapes,
            )?,
        )
        .await?;
        // Database-level grant must treat the underscore in the task database as
        // a literal, and must grant access to exactly that existing database.
        let mut grant = GrantOperation {
            target: GrantTarget::Database(schema.into()),
            grantee: Grantee::MySqlAccount(account.clone()),
            privileges: vec![Privilege::Select],
            revoke: false,
            grant_option: false,
        };
        apply_plan(db, grant_sql(engine, &grant)?).await?;
        login.database = schema.into();
        let connection = Database::connect(&login, &password).await.map_err(|_| {
            anyhow::anyhow!("Special-character MySQL password/scope authentication failed")
        })?;
        connection
            .query(
                &format!("SELECT * FROM {}", qualified_table(engine, table)),
                10,
            )
            .await?;
        drop(connection);
        ensure!(
            !mysql_grants(db, &account).await?[0].rows.is_empty(),
            "SHOW GRANTS returned nothing"
        );
        let sibling = schema.replacen('_', "X", 1);
        apply_plan(
            db,
            namespace_sql(
                engine,
                &NamespaceOperation::CreateDatabase {
                    name: sibling.clone(),
                },
            )?,
        )
        .await?;
        let mut forbidden = login.clone();
        forbidden.database = sibling.clone();
        ensure!(
            Database::connect(&forbidden, &password).await.is_err(),
            "Database grant wildcard leaked access to a sibling database"
        );
        apply_plan(
            db,
            namespace_sql(engine, &NamespaceOperation::DropDatabase { name: sibling })?,
        )
        .await?;
        grant.revoke = true;
        apply_plan(db, grant_sql(engine, &grant)?).await?;
        grant.target = GrantTarget::Table(table.clone());
        grant.revoke = false;
        grant.grant_option = true;
        apply_plan(db, grant_sql(engine, &grant)?).await?;
        grant.revoke = true;
        grant.grant_option = false;
        apply_plan(db, grant_sql(engine, &grant)?).await?;
        let policy = MySqlAccountPolicy {
            resources: MySqlResourceLimits {
                max_queries_per_hour: Some(500),
                max_updates_per_hour: Some(100),
                max_connections_per_hour: Some(100),
                max_user_connections: Some(3),
            },
            password_expiration: Some(MySqlPasswordExpiration::IntervalDays(30)),
            failed_login_attempts: Some(3),
            password_lock_time: Some(MySqlPasswordLockTime::Days(1)),
            locked: Some(true),
        };
        apply_plan(
            db,
            mysql_user_sql(
                &MySqlUserOperation::AlterPolicy {
                    account: account.clone(),
                    policy,
                },
                MySqlStringMode::BackslashEscapes,
            )?,
        )
        .await?;
        let info = mysql_account_info(db, &account).await?;
        ensure!(
            text_exists(&info, "500") && text_exists(&info, "30") && text_exists(&info, "Y"),
            "Account policy not applied"
        );
        let attempt_index = info[0]
            .columns
            .iter()
            .position(|c| c.name == "failed_login_attempts")
            .ok_or_else(|| anyhow::anyhow!("Missing policy metadata"))?;
        ensure!(
            info[0].rows[0][attempt_index] == Cell::Text("3".into()),
            "Failed-login policy metadata not applied"
        );
        let mut locked_login = login.clone();
        locked_login.database = String::new();
        ensure!(
            Database::connect(&locked_login, &password).await.is_err(),
            "Locked account still authenticates"
        );
        apply_plan(
            db,
            mysql_user_sql(
                &MySqlUserOperation::SetLocked {
                    account: account.clone(),
                    locked: false,
                },
                MySqlStringMode::BackslashEscapes,
            )?,
        )
        .await?;
        for expiry in [
            MySqlPasswordExpiration::ExpireNow,
            MySqlPasswordExpiration::Default,
            MySqlPasswordExpiration::Never,
        ] {
            apply_plan(
                db,
                mysql_user_sql(
                    &MySqlUserOperation::AlterPolicy {
                        account: account.clone(),
                        policy: MySqlAccountPolicy {
                            password_expiration: Some(expiry),
                            password_lock_time: Some(MySqlPasswordLockTime::Unbounded),
                            ..Default::default()
                        },
                    },
                    MySqlStringMode::BackslashEscapes,
                )?,
            )
            .await?;
        }
        db.query("SET SESSION sql_mode='NO_BACKSLASH_ESCAPES'", 10)
            .await?;
        credential_plan(
            db,
            mysql_user_sql(
                &MySqlUserOperation::SetPassword {
                    account: account.clone(),
                    password: changed.clone(),
                },
                MySqlStringMode::NoBackslashEscapes,
            )?,
        )
        .await?;
        // Connect without selecting a database after the privileges were revoked.
        login.database = String::new();
        let connection = Database::connect(&login, &changed)
            .await
            .map_err(|_| anyhow::anyhow!("Changed MySQL password authentication failed"))?;
        drop(connection);
        db.query("SET SESSION sql_mode=DEFAULT", 10).await?;
        let new_account = MySqlAccount {
            user: renamed,
            host: "%".into(),
        };
        apply_plan(
            db,
            mysql_user_sql(
                &MySqlUserOperation::Rename {
                    account,
                    new_account: new_account.clone(),
                },
                MySqlStringMode::BackslashEscapes,
            )?,
        )
        .await?;
        apply_plan(
            db,
            mysql_user_sql(
                &MySqlUserOperation::Drop {
                    account: new_account,
                },
                MySqlStringMode::BackslashEscapes,
            )?,
        )
        .await?;
    }
    Ok(())
}

async fn run(engine: Engine) -> Result<()> {
    let tag = format!(
        "ka{:x}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    exercise(engine,&tag).await.with_context(||format!("Disposable {engine:?} parity run prefix {tag}; on failure preserve evidence and clean up this prefix explicitly"))
}
#[tokio::test]
#[ignore = "Requires parent-owned PostgreSQL fixture on 127.0.0.1:15435"]
async fn postgres_ddl_and_access_parity() -> Result<()> {
    run(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Requires parent-owned MySQL root fixture on 127.0.0.1:13309"]
async fn mysql_ddl_and_access_parity() -> Result<()> {
    run(Engine::MySql).await
}
