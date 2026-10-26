use rowboat_core::{
    Cell, Column, ConnectionProfile, Database, Engine, QueryResult, SqliteFileOptions,
    SqliteOpenMode, TableRef,
};
use rowboat_services::*;
use std::collections::BTreeMap;

fn fixture() -> (tempfile::TempDir, ConnectionProfile) {
    let directory = tempfile::Builder::new()
        .prefix("pass12_services_")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    let profile = ConnectionProfile {
        name: "Services file fixture".into(),
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: directory.path().join("quoted ' 日本語.sqlite"),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    (directory, profile)
}

fn mixed() -> QueryResult {
    let values = vec![
        Cell::Null,
        Cell::Text("NULL".into()),
        Cell::Text(String::new()),
        Cell::Text("900719925474099312345678901234567890.000000000000001".into()),
        Cell::Text("quote' backslash\\\n日本語\0tail".into()),
        Cell::Text("'); DROP TABLE important; --".into()),
        Cell::Binary(vec![0, 1, 127, 255]),
        Cell::Binary(vec![]),
    ];
    QueryResult {
        columns: values
            .iter()
            .enumerate()
            .map(|(n, _)| Column {
                name: format!("column{n}\"`[]"),
                data_type: "TEXT".into(),
                nullable: true,
                primary_key: false,
                default_value: None,
            })
            .collect(),
        rows: vec![values],
        ..Default::default()
    }
}

#[test]
fn mixed_cells_preserve_null_binary_unicode_and_precision_in_sql_csv_json() {
    let result = mixed();
    let expected: BTreeMap<_, _> = result
        .columns
        .iter()
        .map(|c| c.name.clone())
        .zip(result.rows[0].clone())
        .collect();
    for decoded in [
        import_csv(&export_csv(&result).unwrap()).unwrap(),
        import_json(&export_json(&result).unwrap()).unwrap(),
    ] {
        assert_eq!(
            decoded[0].iter().cloned().collect::<BTreeMap<_, _>>(),
            expected
        );
    }
    let table = TableRef {
        schema: "main".into(),
        name: "odd \" table".into(),
    };
    let sql = export_sql(Engine::Sqlite, &table, &result).unwrap();
    assert!(sql.starts_with("INSERT INTO \"main\".\"odd \"\" table\" (\"column0\"\"`[]\""));
    assert!(sql.contains("NULL"));
    assert!(sql.contains("X'00017fff'"));
    assert!(sql.contains("X''"));
    assert!(
        sql.contains("'''); DROP TABLE important; --'"),
        "The payload must remain quoted data with its apostrophe doubled"
    );
    assert!(
        !sql.contains('\0'),
        "SQLite SQL text cannot embed NUL; core must render a text expression"
    );
    assert_eq!(
        rowboat_core::sql::split_statements(Engine::Sqlite, &sql)
            .unwrap()
            .len(),
        1
    );
    // PostgreSQL rejects text NUL; MySQL and SQLite render it losslessly.
    assert!(export_sql(Engine::Postgres, &table, &result).is_err());
    assert!(export_sql(Engine::MySql, &table, &result).is_ok());
}

#[tokio::test]
async fn sqlite_exported_sql_executes_with_exact_public_cell_fidelity() {
    let (_directory, profile) = fixture();
    validate_profile(&profile).unwrap();
    let db = Database::connect(&profile, "").await.unwrap();
    let result = mixed();
    let table = TableRef {
        schema: "main".into(),
        name: "odd \" table".into(),
    };
    // No affinity: preserve Cell::Text numeric spelling rather than asking
    // SQLite to coerce it. Cell does not carry native numeric storage classes.
    let definitions = result
        .columns
        .iter()
        .map(|c| rowboat_core::sql::quote_ident(Engine::Sqlite, &c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let target = rowboat_core::sql::qualified_table(Engine::Sqlite, &table);
    db.query(&format!("CREATE TABLE {target} ({definitions})"), 10)
        .await
        .unwrap();
    db.query(
        "CREATE TABLE important(value TEXT); INSERT INTO important VALUES ('sentinel preserved')",
        10,
    )
    .await
    .unwrap();
    db.query(&export_sql(Engine::Sqlite, &table, &result).unwrap(), 10)
        .await
        .unwrap();
    let sentinel = db.query("SELECT value FROM important", 10).await.unwrap();
    assert_eq!(
        sentinel[0].rows,
        vec![vec![Cell::Text("sentinel preserved".into())]],
        "Exported payload must not execute its embedded DROP TABLE statement"
    );
    let selected = db
        .query(&format!("SELECT * FROM {target}"), 10)
        .await
        .unwrap();
    assert_eq!(selected[0].rows, result.rows);
    let types = db
        .query(
            &format!(
                "SELECT typeof({}), typeof({}), typeof({}) FROM {target}",
                definitions.split(", ").nth(3).unwrap(),
                definitions.split(", ").nth(4).unwrap(),
                definitions.split(", ").nth(6).unwrap()
            ),
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        types[0].rows[0],
        [
            Cell::Text("text".into()),
            Cell::Text("text".into()),
            Cell::Text("blob".into())
        ]
    );
}

#[tokio::test]
async fn imported_open_mode_reaches_core_without_implicit_creation_or_truncation() {
    let (_directory, mut profile) = fixture();
    let path = profile.file.as_ref().unwrap().path.clone();
    profile.file.as_mut().unwrap().mode = SqliteOpenMode::OpenExisting;
    let wire = export_connections(std::slice::from_ref(&profile)).unwrap();
    let imported = import_connections(&wire).unwrap().remove(0);
    assert!(Database::connect(&imported, "").await.is_err());
    assert!(!path.exists());
    profile.file.as_mut().unwrap().mode = SqliteOpenMode::CreateIfMissing;
    let created = Database::connect(&profile, "").await.unwrap();
    created
        .query(
            "CREATE TABLE preserved(value); INSERT INTO preserved VALUES ('original')",
            10,
        )
        .await
        .unwrap();
    drop(created);
    let reopened = Database::connect(&profile, "").await.unwrap();
    assert_eq!(
        reopened
            .query("SELECT value FROM preserved", 10)
            .await
            .unwrap()[0]
            .rows[0],
        [Cell::Text("original".into())]
    );
    drop(reopened);
    profile.file.as_mut().unwrap().mode = SqliteOpenMode::OpenExisting;
    profile.read_only = true;
    let wire = export_connections(&[profile]).unwrap();
    let imported = import_connections(&wire).unwrap().remove(0);
    let readonly = Database::connect(&imported, "").await.unwrap();
    assert_eq!(
        readonly
            .query("SELECT value FROM preserved", 10)
            .await
            .unwrap()[0]
            .rows
            .len(),
        1
    );
    assert!(
        readonly
            .query("INSERT INTO preserved VALUES ('blocked')", 10)
            .await
            .is_err()
    );
}
