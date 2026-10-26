use rowboat_admin::{access::*, ddl::*, diff, metrics::*, objects::*, query::*};
use rowboat_core::{Cell, Column, Engine, QueryResult, TableRef};
use std::collections::BTreeMap;

fn table(name: &str) -> TableDesign {
    TableDesign {
        table: TableRef {
            schema: "public".into(),
            name: name.into(),
        },
        columns: vec![ColumnDesign {
            name: "id".into(),
            data_type: DataType::Integer,
            nullable: false,
            default: None,
        }],
        primary_key: Some(PrimaryKeyDesign {
            name: format!("{name}_pk"),
            columns: vec!["id".into()],
        }),
        indexes: vec![],
        foreign_keys: vec![],
    }
}
#[test]
fn ddl_quotes_every_identifier_and_preserves_literal_values() {
    let mut t = table("orders\"; DROP TABLE x;--");
    t.table.schema = "odd\"schema".into();
    t.columns[0].name = "id\"x".into();
    t.primary_key.as_mut().unwrap().columns[0] = "id\"x".into();
    t.columns[0].default = Some(DefaultValue::Literal(Cell::Text(
        "'; DELETE FROM users; --".into(),
    )));
    let sql = preview(Engine::Postgres, &[DdlOperation::CreateTable(t)])
        .unwrap()
        .sql();
    assert!(sql.starts_with("CREATE TABLE \"odd\"\"schema\".\"orders\"\"; DROP TABLE x;--\""));
    assert!(sql.contains("\"id\"\"x\" INTEGER NOT NULL DEFAULT E'''; DELETE FROM users; --'"));
    assert_eq!(
        rowboat_core::sql::split_statements(Engine::Postgres, &sql)
            .unwrap()
            .len(),
        1
    );
    let op = DdlOperation::RenameColumn {
        table: TableRef {
            schema: "db`x".into(),
            name: "t`x".into(),
        },
        name: "a`b".into(),
        new_name: "c`d".into(),
    };
    assert_eq!(
        preview(Engine::MySql, &[op]).unwrap().sql(),
        "ALTER TABLE `db``x`.`t``x` RENAME COLUMN `a``b` TO `c``d`;"
    );
}
#[test]
fn types_are_parsed_not_interpolated() {
    for input in [
        "integer); DROP TABLE users; --",
        "varchar(3) COLLATE evil",
        "decimal(10,2);",
        "varchar(0)",
        "timestamp(7)",
        "decimal(3,4)",
        "text /* x */",
        "varchar(1)(2)",
    ] {
        assert!(DataType::parse(Engine::Postgres, input).is_err(), "{input}");
    }
    assert_eq!(
        DataType::parse(Engine::Postgres, "timestamp(3) with time zone")
            .unwrap()
            .sql(Engine::Postgres)
            .unwrap(),
        "TIMESTAMP(3) WITH TIME ZONE"
    );
    assert_eq!(
        DataType::parse(Engine::Postgres, "numeric(12, 2)[]")
            .unwrap()
            .sql(Engine::Postgres)
            .unwrap(),
        "DECIMAL(12, 2)[]"
    );
    assert!(DataType::parse(Engine::MySql, "jsonb").is_err());
    assert!(DataType::parse(Engine::MySql, "decimal(66,2)").is_err());
    assert!(
        DataType::Array(Box::new(DataType::Integer))
            .sql(Engine::MySql)
            .is_err()
    );
}
#[test]
fn explicit_raw_defaults_are_lexically_checked() {
    for s in [
        "now(); DROP TABLE t",
        "now() --comment",
        "(now()",
        "now())",
        "",
        "1 /* x */",
    ] {
        assert!(
            DefaultValue::RawExpression(s.into())
                .sql(Engine::Postgres)
                .is_err(),
            "{s}"
        );
    }
    assert_eq!(
        DefaultValue::RawExpression("coalesce(NULL, ';')".into())
            .sql(Engine::Postgres)
            .unwrap(),
        "(coalesce(NULL, ';'))"
    );
    assert!(
        DefaultValue::RawExpression("now()".into())
            .sql(Engine::Postgres)
            .is_ok()
    );
    assert_eq!(
        DefaultValue::Literal(Cell::Text("hello".into()))
            .sql(Engine::MySql)
            .unwrap(),
        "(CONVERT(X'68656c6c6f' USING utf8mb4))"
    );
}
#[test]
fn invalid_relationships_and_columns_are_rejected() {
    let mut t = table("t");
    t.columns.push(t.columns[0].clone());
    assert!(preview(Engine::Postgres, &[DdlOperation::CreateTable(t)]).is_err());
    let mut t = table("t");
    t.indexes.push(IndexDesign {
        name: "idx".into(),
        columns: vec!["missing".into()],
        unique: false,
    });
    assert!(preview(Engine::Postgres, &[DdlOperation::CreateTable(t)]).is_err());
    let fk = ForeignKeyDesign {
        name: "fk".into(),
        columns: vec!["id".into()],
        referenced_table: table("parent").table,
        referenced_columns: vec![],
        on_update: ReferentialAction::NoAction,
        on_delete: ReferentialAction::Cascade,
    };
    assert!(
        preview(
            Engine::Postgres,
            &[DdlOperation::AddForeignKey {
                table: table("t").table,
                foreign_key: fk
            }]
        )
        .is_err()
    );
}
#[test]
fn alter_and_drop_use_dialect_specific_syntax() {
    let t = table("t");
    let ops = [
        DdlOperation::AlterColumn {
            table: t.table.clone(),
            column: t.columns[0].clone(),
        },
        DdlOperation::DropIndex {
            table: t.table.clone(),
            name: "idx".into(),
        },
        DdlOperation::DropForeignKey {
            table: t.table.clone(),
            name: "fk".into(),
        },
    ];
    let pg = preview(Engine::Postgres, &ops).unwrap();
    assert_eq!(pg.statements.len(), 5);
    assert!(pg.sql().contains("ALTER COLUMN \"id\" DROP DEFAULT;"));
    assert!(pg.sql().contains("DROP INDEX \"public\".\"idx\";"));
    assert!(pg.sql().contains("DROP CONSTRAINT \"fk\";"));
    let my = preview(Engine::MySql, &ops).unwrap();
    assert_eq!(my.statements.len(), 3);
    assert!(my.sql().contains("MODIFY COLUMN `id` INTEGER NOT NULL;"));
    assert!(my.sql().contains("DROP INDEX `idx` ON `public`.`t`;"));
    assert!(my.sql().contains("DROP FOREIGN KEY `fk`;"));
}
#[test]
fn postgres_parameters_ignore_all_quoted_and_commented_text() {
    let values = BTreeMap::from([("v".into(), Cell::Text("O'Reilly\\x".into()))]);
    let sql = "SELECT :v::text, ':missing', E'\\\':missing', \"a:missing\", $$:missing$$, $tag$:missing$tag$ /* outer :missing /* :missing */ */ -- :missing\n, :v";
    let expanded = expand_named(Engine::Postgres, sql, &values).unwrap();
    assert_eq!(expanded.matches("E'O''Reilly\\\\x'").count(), 2);
    assert!(expanded.contains("::text"));
    assert!(expanded.contains("$tag$:missing$tag$"));
    assert_eq!(
        rowboat_core::sql::split_statements(Engine::Postgres, &expanded)
            .unwrap()
            .len(),
        1
    );
}
#[test]
fn mysql_parameters_are_safe_literals_and_handle_mysql_comments() {
    let values = BTreeMap::from([("v".into(), Cell::Text("'; DROP TABLE t; --\\".into()))]);
    let sql =
        "SELECT :v, ':ignored', `:ignored`, 'it\\\'s :ignored' # :ignored\n -- :ignored\n, :v";
    let result = expand_named(Engine::MySql, sql, &values).unwrap();
    assert_eq!(result.matches("CONVERT(X'").count(), 2);
    assert_eq!(
        rowboat_core::sql::split_statements(Engine::MySql, &result)
            .unwrap()
            .len(),
        1
    );
    assert!(expand_named(Engine::MySql, "SELECT /*! :v */", &values).is_err());
}
#[test]
fn parameter_errors_and_unicode_are_deterministic() {
    let values = BTreeMap::from([("x".into(), Cell::Null)]);
    assert_eq!(
        expand_named(
            Engine::Postgres,
            "SELECT $日本語$:missing$日本語$, :x",
            &values
        )
        .unwrap(),
        "SELECT $日本語$:missing$日本語$, NULL"
    );
    assert_eq!(
        expand_named(Engine::Postgres, "SELECT '日本語', :x", &values).unwrap(),
        "SELECT '日本語', NULL"
    );
    for sql in [
        "SELECT :missing",
        "SELECT 'unclosed",
        "SELECT $body$unclosed",
        "SELECT /* unclosed",
    ] {
        assert!(
            expand_named(Engine::Postgres, sql, &values).is_err(),
            "{sql}"
        );
    }
}

#[test]
fn mysql_parameter_lexer_can_match_nondefault_sql_modes() {
    let values = BTreeMap::from([("x".into(), Cell::Null)]);
    let sql = r"SELECT 'ends\', :x";
    assert!(expand_named(Engine::MySql, sql, &values).is_err());
    assert_eq!(
        expand_named_with_options(
            Engine::MySql,
            sql,
            &values,
            SqlLexOptions {
                mysql_no_backslash_escapes: true,
                ..Default::default()
            }
        )
        .unwrap(),
        r"SELECT 'ends\', NULL"
    );
    assert_eq!(
        expand_named_with_options(
            Engine::MySql,
            "SELECT \"identifier\\\", :x",
            &values,
            SqlLexOptions {
                mysql_ansi_quotes: true,
                ..Default::default()
            }
        )
        .unwrap(),
        "SELECT \"identifier\\\", NULL"
    );
    assert_eq!(
        expand_named(Engine::MySql, "SELECT `identifier\\`, :x", &values).unwrap(),
        "SELECT `identifier\\`, NULL"
    );
}

#[test]
fn index_rename_uses_schema_qualification_only_where_supported() {
    let op = DdlOperation::RenameIndex {
        table: table("t").table,
        name: "old\"`index".into(),
        new_name: "new\"`index".into(),
    };
    assert_eq!(
        preview(Engine::Postgres, std::slice::from_ref(&op))
            .unwrap()
            .sql(),
        "ALTER INDEX \"public\".\"old\"\"`index\" RENAME TO \"new\"\"`index\";"
    );
    assert_eq!(
        preview(Engine::MySql, &[op]).unwrap().sql(),
        "ALTER TABLE `public`.`t` RENAME INDEX `old\"``index` TO `new\"``index`;"
    );
}
#[test]
fn explain_is_single_statement_without_analyze() {
    assert!(
        explain_sql(Engine::Postgres, "-- heading\n SELECT ';'; -- trailing")
            .unwrap()
            .starts_with("EXPLAIN (FORMAT JSON)\n")
    );
    assert!(
        explain_sql(Engine::MySql, "SELECT 1")
            .unwrap()
            .starts_with("EXPLAIN FORMAT=JSON")
    );
    for sql in [
        "SELECT 1; DELETE FROM t",
        "ANALYZE t",
        "EXPLAIN ANALYZE SELECT 1",
        "DROP TABLE t",
        "",
    ] {
        assert!(explain_sql(Engine::Postgres, sql).is_err());
    }
}
#[test]
fn diff_is_empty_for_equal_schemas_and_marks_data_loss() {
    let t = table("t");
    assert!(
        diff::compare(
            Engine::Postgres,
            std::slice::from_ref(&t),
            std::slice::from_ref(&t)
        )
        .unwrap()
        .is_empty()
    );
    let plan = diff::compare(Engine::Postgres, &[t], &[]).unwrap();
    assert!(
        plan.statements
            .iter()
            .any(|s| s.destructive && s.sql.starts_with("DROP TABLE"))
    );
    assert!(!plan.sql().contains("CASCADE"));
    assert!(diff::compare(Engine::Postgres, &[table("t"), table("t")], &[]).is_err());
}
#[test]
fn diff_orders_cyclic_foreign_keys_after_all_creates() {
    let mut a = table("a");
    let mut b = table("b");
    for (t, other) in [
        (&mut a, &b.table.clone()),
        (
            &mut b,
            &TableRef {
                schema: "public".into(),
                name: "a".into(),
            },
        ),
    ] {
        t.foreign_keys.push(ForeignKeyDesign {
            name: format!("{}_fk", t.table.name),
            columns: vec!["id".into()],
            referenced_table: other.clone(),
            referenced_columns: vec!["id".into()],
            on_update: ReferentialAction::NoAction,
            on_delete: ReferentialAction::NoAction,
        });
    }
    let plan = diff::compare(Engine::Postgres, &[], &[a.clone(), b.clone()]).unwrap();
    assert!(plan.statements[0].sql.starts_with("CREATE TABLE"));
    assert!(plan.statements[1].sql.starts_with("CREATE TABLE"));
    assert!(plan.statements[2].sql.contains("ADD CONSTRAINT"));
    assert!(plan.statements[3].sql.contains("ADD CONSTRAINT"));
    let mut changed = a.clone();
    changed.columns[0].data_type = DataType::BigInt;
    let plan = diff::compare(Engine::Postgres, &[a, b.clone()], &[changed, b]).unwrap();
    assert!(plan.statements[0].sql.contains("DROP CONSTRAINT \"a_fk\""));
    assert!(plan.statements[1].sql.contains("DROP CONSTRAINT \"b_fk\""));
    assert!(
        plan.statements
            .last()
            .unwrap()
            .sql
            .contains("ADD CONSTRAINT \"b_fk\"")
    );
}
#[test]
fn diff_reports_column_order_without_rebuilding_constraints() {
    let mut before = table("ordered");
    before.columns.push(ColumnDesign {
        name: "second".into(),
        data_type: DataType::Text,
        nullable: true,
        default: None,
    });
    let mut after = before.clone();
    after.columns.reverse();
    let plan = diff::compare(Engine::Postgres, &[before], &[after]).unwrap();
    assert!(plan.is_empty());
    assert!(plan.warnings.iter().any(|s| s.contains("Column order")));
}

#[test]
fn diff_rejects_dangling_foreign_keys_in_desired_snapshot() {
    let parent = table("parent");
    let mut child = table("child");
    child.foreign_keys.push(ForeignKeyDesign {
        name: "parent_fk".into(),
        columns: vec!["id".into()],
        referenced_table: parent.table.clone(),
        referenced_columns: vec!["id".into()],
        on_update: ReferentialAction::NoAction,
        on_delete: ReferentialAction::NoAction,
    });
    assert!(
        diff::compare(
            Engine::Postgres,
            &[parent.clone(), child.clone()],
            std::slice::from_ref(&child)
        )
        .is_err()
    );
    child.foreign_keys[0].referenced_columns = vec!["missing".into()];
    assert!(diff::compare(Engine::Postgres, &[], &[parent, child]).is_err());
}

#[test]
fn namespaces_reject_unsupported_rename_and_quote_names() {
    let op = NamespaceOperation::RenameDatabase {
        name: "old\"db".into(),
        new_name: "new\"db".into(),
    };
    assert_eq!(
        namespace_sql(Engine::Postgres, &op).unwrap().sql(),
        "ALTER DATABASE \"old\"\"db\" RENAME TO \"new\"\"db\";"
    );
    assert!(namespace_sql(Engine::MySql, &op).is_err());
}
#[test]
fn accounts_roles_and_grants_quote_inputs_and_check_scope() {
    let account = MySqlAccount {
        user: "u`ser".into(),
        host: "%".into(),
    };
    assert_eq!(account.sql().unwrap(), "`u``ser`@`%`");
    let op = MySqlUserOperation::Create {
        account: account.clone(),
        password: "a'\\b".into(),
        locked: true,
    };
    assert!(
        mysql_user_sql(&op, MySqlStringMode::BackslashEscapes)
            .unwrap()
            .sql()
            .contains("IDENTIFIED BY 'a''\\\\b' ACCOUNT LOCK")
    );
    assert!(
        mysql_user_sql(&op, MySqlStringMode::NoBackslashEscapes)
            .unwrap()
            .sql()
            .contains("IDENTIFIED BY 'a''\\b'")
    );
    let role = PgRoleOperation::Create {
        name: "r\"ole".into(),
        options: PgRoleOptions {
            login: true,
            ..Default::default()
        },
        password: None,
    };
    assert!(
        pg_role_sql(&role)
            .unwrap()
            .sql()
            .starts_with("CREATE ROLE \"r\"\"ole\" WITH LOGIN NOSUPERUSER")
    );
    let mut grant = GrantOperation {
        target: GrantTarget::Table(table("t").table),
        grantee: Grantee::MySqlAccount(account),
        privileges: vec![Privilege::Select],
        revoke: false,
        grant_option: false,
    };
    assert!(
        grant_sql(Engine::MySql, &grant)
            .unwrap()
            .sql()
            .starts_with("GRANT SELECT ON `public`.`t` TO")
    );
    assert!(grant_sql(Engine::Postgres, &grant).is_err());
    grant.privileges = vec![Privilege::CreateUser];
    assert!(grant_sql(Engine::MySql, &grant).is_err());
}
#[test]
fn metrics_preserve_real_values_nulls_and_counter_kinds() {
    let column = |name: &str| Column {
        name: name.into(),
        data_type: "text".into(),
        nullable: true,
        primary_key: false,
        default_value: None,
    };
    let raw = vec![QueryResult {
        columns: vec![
            column("xact_commit"),
            column("numbackends"),
            column("stats_reset"),
            column("temp_bytes"),
        ],
        rows: vec![vec![
            Cell::Text("42".into()),
            Cell::Text("2".into()),
            Cell::Text("2026-10-15 00:00:00+00".into()),
            Cell::Null,
        ]],
        ..Default::default()
    }];
    let sample = decode_sample(Engine::Postgres, raw, 123).unwrap();
    assert_eq!(sample.metrics.len(), 2);
    assert_eq!(sample.sampled_at_unix_ms, 123);
    assert!(sample.counters_reset_at.is_some());
    assert!(
        sample
            .metrics
            .iter()
            .any(|m| m.name == "xact_commit" && m.value == 42 && m.kind == MetricKind::Counter)
    );
    assert!(
        sample
            .metrics
            .iter()
            .any(|m| m.name == "numbackends" && m.value == 2 && m.kind == MetricKind::Gauge)
    );
    let raw = vec![QueryResult {
        rows: vec![vec![
            Cell::Text("Bytes_sent".into()),
            Cell::Text("100".into()),
        ]],
        ..Default::default()
    }];
    let sample = decode_sample(Engine::MySql, raw, 124).unwrap();
    assert_eq!(sample.metrics[0].unit, "bytes");
    assert_eq!(sample.metrics[0].value, 100);
}
