use rowboat_admin::access::*;
use rowboat_core::{Engine, TableRef};

fn operation(engine: Engine) -> GrantOperation {
    GrantOperation {
        target: GrantTarget::Table(TableRef {
            schema: "exact schema".into(),
            name: "a ON x`\"".into(),
        }),
        grantee: match engine {
            Engine::Postgres => Grantee::PgRole("role\"name".into()),
            Engine::MySql => Grantee::MySqlAccount(MySqlAccount {
                user: "user`name".into(),
                host: "host%".into(),
            }),
            Engine::Sqlite => panic!("This fixture tests installed server grant providers"),
        },
        privileges: vec![Privilege::Select, Privilege::Update],
        revoke: false,
        grant_option: true,
    }
}

#[test]
fn column_preview_keeps_exact_quoted_principal_table_and_columns() {
    let pg = column_grant_sql(
        Engine::Postgres,
        &operation(Engine::Postgres),
        &["a\"b".into()],
    )
    .unwrap();
    assert_eq!(
        pg.statements[0].sql,
        "GRANT SELECT (\"a\"\"b\"), UPDATE (\"a\"\"b\") ON TABLE \"exact schema\".\"a ON x`\"\"\" TO \"role\"\"name\" WITH GRANT OPTION;"
    );
    let my = column_grant_sql(Engine::MySql, &operation(Engine::MySql), &["a`b".into()]).unwrap();
    assert_eq!(
        my.statements[0].sql,
        "GRANT SELECT (`a``b`), UPDATE (`a``b`) ON `exact schema`.`a ON x``\"` TO `user``name`@`host%` WITH GRANT OPTION;"
    );
    assert!(my.warnings.iter().any(|w| w.contains("table scope")));
}

#[test]
fn column_preview_rejects_noncolumn_privileges_and_grant_option_revoke() {
    let mut op = operation(Engine::Postgres);
    op.privileges = vec![Privilege::Delete];
    assert!(column_grant_sql(Engine::Postgres, &op, &["x".into()]).is_err());
    op.privileges = vec![Privilege::Select];
    op.revoke = true;
    assert!(column_grant_sql(Engine::Postgres, &op, &["x".into()]).is_err());
    op.grant_option = false;
    assert!(column_grant_sql(Engine::Postgres, &op, &[]).is_err());
    assert!(column_grant_sql(Engine::Postgres, &op, &["x\0y".into()]).is_err());
    assert!(
        column_grant_sql(Engine::Postgres, &op, &["x".into()])
            .unwrap()
            .statements[0]
            .sql
            .starts_with("REVOKE SELECT (\"x\")")
    );
}

#[test]
fn new_privileges_remain_dialect_and_scope_checked() {
    let mut op = operation(Engine::Postgres);
    op.privileges = vec![Privilege::Truncate, Privilege::Maintain];
    assert!(grant_sql(Engine::Postgres, &op).is_ok());
    let mut my = operation(Engine::MySql);
    my.privileges = op.privileges;
    assert!(grant_sql(Engine::MySql, &my).is_err());
    my.target = GrantTarget::Global;
    assert!(grant_sql(Engine::MySql, &my).is_err());
    my.target = GrantTarget::Schema("literal_%".into());
    my.privileges = vec![
        Privilege::Event,
        Privilege::LockTables,
        Privilege::CreateTemporaryTables,
    ];
    let off =
        grant_sql_with_mysql_mode(Engine::MySql, &my, MySqlDatabaseGrantMode::WildcardPatterns)
            .unwrap();
    let on = grant_sql_with_mysql_mode(Engine::MySql, &my, MySqlDatabaseGrantMode::LiteralNames)
        .unwrap();
    assert!(off.statements[0].sql.contains("`literal\\_\\%`.*"));
    assert!(on.statements[0].sql.contains("`literal_%`.*"));
}
