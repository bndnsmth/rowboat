use rowboat_admin::{access::*, ddl::*};
use rowboat_core::{Cell, Engine, TableRef};

fn table() -> TableRef {
    TableRef {
        schema: "db`\"name".into(),
        name: "tbl`\"name".into(),
    }
}
fn full() -> MySqlColumnDefinition {
    MySqlColumnDefinition {
        column: ColumnDesign {
            name: "id`name".into(),
            data_type: DataType::BigInt,
            nullable: false,
            default: None,
        },
        unsigned: true,
        auto_increment: true,
        character_set: None,
        collation: None,
        comment: "owner's key".into(),
        on_update_current_timestamp_precision: None,
        invisible: false,
    }
}

#[test]
fn constraints_are_typed_quoted_and_validate_expressions() {
    let ops = [
        DdlOperation::AddUnique {
            table: table(),
            unique: UniqueDesign {
                name: "u`\"q".into(),
                columns: vec!["col`\"name".into()],
            },
        },
        DdlOperation::AddCheck {
            table: table(),
            check: CheckDesign {
                name: "c`\"q".into(),
                expression: "length('a;b') > 0".into(),
            },
        },
    ];
    let pg = preview(Engine::Postgres, &ops).unwrap().sql();
    let my = preview(Engine::MySql, &ops).unwrap().sql();
    assert!(pg.contains("CONSTRAINT \"u`\"\"q\" UNIQUE (\"col`\"\"name\")"));
    assert!(my.contains("CONSTRAINT `u``\"q` UNIQUE (`col``\"name`)"));
    assert!(my.contains("CHECK (length('a;b') > 0)"));
    for expression in [
        "1=1); DROP TABLE x; (1",
        "1=1 -- ignore",
        "1=1 /* ignore */",
    ] {
        assert!(
            preview(
                Engine::MySql,
                &[DdlOperation::AddCheck {
                    table: table(),
                    check: CheckDesign {
                        name: "c".into(),
                        expression: expression.into()
                    }
                }]
            )
            .is_err()
        );
    }
}
#[test]
fn mysql_rename_stays_in_original_database() {
    assert_eq!(
        preview(
            Engine::MySql,
            &[DdlOperation::RenameTable {
                table: table(),
                new_name: "new`name".into()
            }]
        )
        .unwrap()
        .sql(),
        "ALTER TABLE `db``\"name`.`tbl``\"name` RENAME TO `db``\"name`.`new``name`;"
    );
}
#[test]
fn targeted_edits_do_not_restate_other_column_attributes() {
    let op = DdlOperation::SetColumnDefault {
        table: table(),
        name: "c".into(),
        default: Some(DefaultValue::Literal(Cell::Text("value".into()))),
    };
    let my = preview(Engine::MySql, &[op]).unwrap().sql();
    assert!(my.contains("ALTER COLUMN `c` SET DEFAULT"));
    assert!(!my.contains("MODIFY"));
    let nullable = DdlOperation::SetColumnNullable {
        table: table(),
        name: "c".into(),
        nullable: true,
    };
    assert!(preview(Engine::MySql, std::slice::from_ref(&nullable)).is_err());
    let pg = preview(Engine::Postgres, &[nullable]).unwrap().sql();
    assert!(pg.contains("DROP NOT NULL"));
    assert!(!pg.contains("DEFAULT"));
    assert!(!pg.contains(" TYPE "));
    let legacy = preview(
        Engine::MySql,
        &[DdlOperation::AlterColumn {
            table: table(),
            column: full().column,
        }],
    )
    .unwrap();
    assert!(
        legacy
            .warnings
            .iter()
            .any(|s| s.contains("LOSSY") && s.contains("AUTO_INCREMENT"))
    );
}
#[test]
fn mysql_complete_definition_rejects_attribute_loss_and_invalid_combinations() {
    let definition = full();
    let sql = definition.sql().unwrap();
    assert!(sql.contains("BIGINT UNSIGNED NOT NULL AUTO_INCREMENT COMMENT 'owner''s key' VISIBLE"));
    let mut invalid = definition.clone();
    invalid.column.default = Some(DefaultValue::Literal(Cell::Text("1".into())));
    assert!(invalid.sql().is_err());
    invalid = definition.clone();
    invalid.collation = Some("utf8mb4_bin".into());
    assert!(invalid.sql().is_err());
    invalid = definition;
    invalid.comment = "back\\slash".into();
    assert!(invalid.sql().is_err());
}
#[test]
fn account_patches_only_change_selected_attributes() {
    let role = pg_role_sql(&PgRoleOperation::Patch {
        name: "r\"ole".into(),
        options: PgRolePatch {
            connection_limit: Some(9),
            valid_until: Some("infinity".into()),
            ..Default::default()
        },
    })
    .unwrap()
    .sql();
    assert_eq!(
        role,
        "ALTER ROLE \"r\"\"ole\" WITH CONNECTION LIMIT 9 VALID UNTIL E'infinity';"
    );
    assert!(!role.contains("NOLOGIN"));
    assert!(
        pg_role_sql(&PgRoleOperation::Patch {
            name: "r".into(),
            options: PgRolePatch::default()
        })
        .is_err()
    );
    let account = MySqlAccount {
        user: "u`ser".into(),
        host: "%".into(),
    };
    let plan = mysql_user_sql(
        &MySqlUserOperation::AlterPolicy {
            account: account.clone(),
            policy: MySqlAccountPolicy {
                resources: MySqlResourceLimits {
                    max_user_connections: Some(0),
                    ..Default::default()
                },
                password_expiration: Some(MySqlPasswordExpiration::Never),
                locked: Some(false),
                ..Default::default()
            },
        },
        MySqlStringMode::BackslashEscapes,
    )
    .unwrap();
    assert_eq!(
        plan.sql(),
        "ALTER USER `u``ser`@`%` WITH MAX_USER_CONNECTIONS 0 PASSWORD EXPIRE NEVER ACCOUNT UNLOCK;"
    );
    assert!(
        mysql_user_sql(
            &MySqlUserOperation::AlterPolicy {
                account,
                policy: MySqlAccountPolicy {
                    failed_login_attempts: Some(32768),
                    ..Default::default()
                }
            },
            MySqlStringMode::BackslashEscapes
        )
        .is_err()
    );
}
#[test]
fn database_grants_escape_wildcards_in_addition_to_identifier_quotes() {
    let op = GrantOperation {
        target: GrantTarget::Database("db_%`name".into()),
        grantee: Grantee::MySqlAccount(MySqlAccount {
            user: "reader".into(),
            host: "%".into(),
        }),
        privileges: vec![Privilege::Select],
        revoke: false,
        grant_option: false,
    };
    let wildcard = grant_sql(Engine::MySql, &op).unwrap();
    assert_eq!(
        wildcard.sql(),
        "GRANT SELECT ON `db\\_\\%``name`.* TO `reader`@`%`;"
    );
    assert!(
        wildcard
            .warnings
            .iter()
            .any(|s| s.contains("partial_revokes=OFF"))
    );
    assert_eq!(
        grant_sql_with_mysql_mode(Engine::MySql, &op, MySqlDatabaseGrantMode::LiteralNames)
            .unwrap()
            .sql(),
        "GRANT SELECT ON `db_%``name`.* TO `reader`@`%`;"
    );
}

#[test]
fn review_metadata_exposes_transaction_and_destructive_limits() {
    let op = DdlOperation::DropColumn {
        table: table(),
        name: "old".into(),
    };
    let plan = preview(Engine::MySql, &[op]).unwrap();
    assert_eq!(
        plan.review(Engine::MySql),
        PlanReview {
            implicit_commit: true,
            requires_outside_transaction: true,
            destructive_statement_count: 1
        }
    );
    let plan = rowboat_admin::objects::namespace_sql(
        Engine::Postgres,
        &rowboat_admin::objects::NamespaceOperation::CreateDatabase {
            name: "new_db".into(),
        },
    )
    .unwrap();
    assert!(plan.review(Engine::Postgres).requires_outside_transaction);
    assert!(!plan.review(Engine::Postgres).implicit_commit);
}
