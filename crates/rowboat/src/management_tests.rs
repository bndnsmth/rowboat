use super::{Draft, Field, Flag, Operation, Review, Target};
use rowboat_admin::{
    access::MySqlStringMode,
    ddl::{ColumnDesign, DataType, DefaultValue, MySqlColumnDefinition},
};
use rowboat_core::{Cell, ConnectionProfile, Engine, TableRef};
use std::collections::{BTreeMap, BTreeSet};

fn target(engine: Engine) -> Target {
    Target {
        session: 7,
        connection: "fixture".into(),
        database: "rowboat".into(),
        engine,
        table: Some(TableRef {
            schema: "public".into(),
            name: "people".into(),
        }),
    }
}
fn draft(operation: Operation, values: &[(Field, &str)]) -> Draft {
    Draft {
        operation,
        values: values.iter().map(|(k, v)| (*k, (*v).into())).collect(),
        flags: BTreeSet::new(),
    }
}
fn mode() -> MySqlStringMode {
    MySqlStringMode::BackslashEscapes
}

#[test]
fn reviewed_change_cannot_cross_connection_session_table_or_operation() {
    let target = target(Engine::Postgres);
    let review = Review::build(
        target.clone(),
        draft(Operation::DropColumn, &[(Field::Name, "old")]),
        mode(),
    )
    .unwrap();
    assert!(
        review
            .validate(
                &target,
                Operation::DropColumn,
                false,
                false,
                false,
                Some(false)
            )
            .is_ok()
    );
    let mut changed = target.clone();
    changed.session += 1;
    assert!(
        review
            .validate(
                &changed,
                Operation::DropColumn,
                false,
                false,
                false,
                Some(false)
            )
            .is_err()
    );
    changed = target.clone();
    changed.connection = "other".into();
    assert!(
        review
            .validate(
                &changed,
                Operation::DropColumn,
                false,
                false,
                false,
                Some(false)
            )
            .is_err()
    );
    changed = target.clone();
    changed.table.as_mut().unwrap().name = "other".into();
    assert!(
        review
            .validate(
                &changed,
                Operation::DropColumn,
                false,
                false,
                false,
                Some(false)
            )
            .is_err()
    );
    assert!(
        review
            .validate(
                &target,
                Operation::DropTable,
                false,
                false,
                false,
                Some(false)
            )
            .is_err()
    );
    for (readonly, busy, pending, transaction) in [
        (true, false, false, Some(false)),
        (false, true, false, Some(false)),
        (false, false, true, Some(false)),
        (false, false, false, Some(true)),
        (false, false, false, None),
    ] {
        assert!(
            review
                .validate(
                    &target,
                    Operation::DropColumn,
                    readonly,
                    busy,
                    pending,
                    transaction
                )
                .is_err()
        );
    }
    let profile = ConnectionProfile {
        id: target.connection.clone(),
        database: target.database.clone(),
        engine: target.engine,
        ..Default::default()
    };
    assert!(target.matches(7, &profile, target.table.as_ref()));
    assert!(!target.matches(8, &profile, target.table.as_ref()));
    assert!(!target.matches(7, &profile, None));
}

#[test]
fn password_preview_is_built_without_the_real_secret_in_every_string_mode() {
    let password = "secret'\\雪;\nline";
    for engine in [Engine::Postgres, Engine::MySql] {
        for mode in [
            MySqlStringMode::BackslashEscapes,
            MySqlStringMode::NoBackslashEscapes,
        ] {
            let review = Review::build(
                target(engine),
                draft(
                    Operation::SetPassword,
                    &[
                        (Field::Name, "alice"),
                        (Field::Host, "localhost"),
                        (Field::Password, password),
                    ],
                ),
                mode,
            )
            .unwrap();
            let shown = serde_json::to_string(&review.display).unwrap();
            for fragment in ["secret", "雪", "line"] {
                assert!(!shown.contains(fragment), "{shown}");
            }
            assert!(shown.contains("REDACTED"));
            assert!(review.plan.sql().contains("secret"));
            assert!(review.plan.sql().contains("雪"));
            assert!(!review.plan.sql().contains("REDACTED"));
            assert!(review.operation.secret());
        }
    }
}

#[test]
fn role_patch_does_not_reset_unselected_flags_or_password() {
    let mut values = BTreeMap::from([
        (Field::Name, "editor".into()),
        (Field::PgLogin, "Enabled".into()),
    ]);
    for field in [
        Field::PgSuperuser,
        Field::PgCreateDb,
        Field::PgCreateRole,
        Field::PgInherit,
        Field::PgReplication,
        Field::PgBypassRls,
    ] {
        values.insert(field, "Unchanged".into());
    }
    let review = Review::build(
        target(Engine::Postgres),
        Draft {
            operation: Operation::AlterRole,
            values,
            flags: BTreeSet::new(),
        },
        mode(),
    )
    .unwrap();
    let sql = review.plan.sql();
    assert!(sql.contains("LOGIN"));
    for flag in [
        "SUPERUSER",
        "CREATEDB",
        "CREATEROLE",
        "INHERIT",
        "REPLICATION",
        "BYPASSRLS",
        "CONNECTION LIMIT",
        "PASSWORD",
    ] {
        assert!(!sql.contains(flag), "unexpected {flag}: {sql}");
    }
}

#[test]
fn mysql_column_change_preserves_unedited_attributes_and_original_snapshot() {
    let original = MySqlColumnDefinition {
        column: ColumnDesign {
            name: "id".into(),
            data_type: DataType::Integer,
            nullable: false,
            default: None,
        },
        unsigned: true,
        auto_increment: true,
        character_set: None,
        collation: None,
        comment: "external id".into(),
        on_update_current_timestamp_precision: None,
        invisible: true,
    };
    let review = Review::build_with_column(
        target(Engine::MySql),
        draft(
            Operation::AlterColumn,
            &[
                (Field::Name, "id"),
                (Field::Type, "bigint"),
                (Field::DefaultKind, "Keep"),
            ],
        ),
        mode(),
        Some(&original),
    )
    .unwrap();
    let sql = review.plan.sql();
    for expected in [
        "BIGINT",
        "UNSIGNED",
        "AUTO_INCREMENT",
        "external id",
        "INVISIBLE",
        "NOT NULL",
    ] {
        assert!(sql.contains(expected), "{sql}");
    }
    assert_eq!(review.mysql_original, Some(original));
    assert!(
        Review::build(
            target(Engine::MySql),
            draft(
                Operation::AlterColumn,
                &[
                    (Field::Name, "id"),
                    (Field::Type, "bigint"),
                    (Field::DefaultKind, "None")
                ]
            ),
            mode()
        )
        .is_err()
    );
}

#[test]
fn mysql_literal_default_is_not_reinterpreted_as_an_expression() {
    let original = MySqlColumnDefinition {
        column: ColumnDesign {
            name: "tag".into(),
            data_type: DataType::VarChar(20),
            nullable: true,
            default: Some(DefaultValue::Literal(Cell::Text("CURRENT_USER()".into()))),
        },
        unsigned: false,
        auto_increment: false,
        character_set: Some("utf8mb4".into()),
        collation: Some("utf8mb4_bin".into()),
        comment: "".into(),
        on_update_current_timestamp_precision: None,
        invisible: false,
    };
    let mut values = draft(
        Operation::AlterColumn,
        &[
            (Field::Name, "tag"),
            (Field::Type, "varchar(40)"),
            (Field::DefaultKind, "Keep"),
        ],
    );
    values.flags.insert(Flag::Nullable);
    let review =
        Review::build_with_column(target(Engine::MySql), values, mode(), Some(&original)).unwrap();
    let sql = review.plan.sql();
    assert!(sql.contains("utf8mb4_bin"));
    assert!(sql.contains("DEFAULT (CONVERT(X'"));
    assert!(!sql.contains("DEFAULT (CURRENT_USER()"));
}

#[test]
fn quoted_column_lists_keep_comma_names_and_reject_raw_injection() {
    let review = Review::build(
        target(Engine::Postgres),
        draft(
            Operation::AddUnique,
            &[
                (Field::Name, "unique key"),
                (Field::Columns, r#"["last, first","tenant\"id"]"#),
            ],
        ),
        mode(),
    )
    .unwrap();
    let sql = review.plan.sql();
    assert!(sql.contains("\"last, first\""));
    assert!(sql.contains("\"tenant\"\"id\""));
    assert!(
        Review::build(
            target(Engine::Postgres),
            draft(
                Operation::AddCheck,
                &[
                    (Field::Name, "check"),
                    (Field::Expression, "id > 0); DROP TABLE people; --")
                ]
            ),
            mode()
        )
        .is_err()
    );
}

#[test]
fn database_drop_requires_another_maintenance_database_and_atomicity_is_explicit() {
    for engine in [Engine::Postgres, Engine::MySql] {
        assert!(
            Review::build(
                target(engine),
                draft(Operation::DropDatabase, &[(Field::Name, "rowboat")]),
                mode()
            )
            .is_err()
        );
        let review = Review::build(
            target(engine),
            draft(Operation::CreateDatabase, &[(Field::Name, "new_db")]),
            mode(),
        )
        .unwrap();
        assert!(!review.operation.atomic(engine));
    }
    assert!(Operation::AddForeignKey.atomic(Engine::Postgres));
    assert!(!Operation::AddForeignKey.atomic(Engine::MySql));
}

#[test]
fn rename_refreshes_new_table_without_retargeting_review_and_drop_clears_table() {
    let target = target(Engine::Postgres);
    let review = Review::build(
        target.clone(),
        draft(Operation::RenameTable, &[(Field::NewName, "renamed")]),
        mode(),
    )
    .unwrap();
    assert_eq!(review.target, target);
    assert_eq!(review.refresh_table.unwrap().name, "renamed");
    let review = Review::build(target, draft(Operation::DropTable, &[]), mode()).unwrap();
    assert!(review.refresh_table.is_none());
}
