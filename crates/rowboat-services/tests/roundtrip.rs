use rowboat_core::{Cell, Column, ConnectionProfile, Engine, QueryResult, TableRef};
use rowboat_services::*;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

fn sample() -> QueryResult {
    let values = vec![
        Cell::Null,
        Cell::Text("NULL".into()),
        Cell::Text(String::new()),
        Cell::Text("\\N".into()),
        Cell::Text("\\x0123".into()),
        Cell::Text("\\\\prefix".into()),
        Cell::Text("comma,quote\"\r\n日本語 🦆".into()),
        Cell::Binary(vec![0, 10, 13, 127, 255]),
        Cell::Binary(vec![]),
    ];
    QueryResult {
        columns: values
            .iter()
            .enumerate()
            .map(|(i, _)| Column {
                name: format!("column{i}"),
                data_type: "text".into(),
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
fn csv_and_json_preserve_null_text_binary_and_escaping() {
    let original = sample();
    for imported in [
        import_csv(&export_csv(&original).unwrap()).unwrap(),
        import_json(&export_json(&original).unwrap()).unwrap(),
    ] {
        let expected: BTreeMap<_, _> = original
            .columns
            .iter()
            .map(|c| c.name.clone())
            .zip(original.rows[0].clone())
            .collect();
        assert_eq!(imported.len(), 1);
        assert_eq!(
            imported[0].iter().cloned().collect::<BTreeMap<_, _>>(),
            expected
        );
    }
}

#[test]
fn imports_reject_ambiguous_or_inconsistent_data() {
    for csv in [
        "a,b\n1\n",
        "a,b\n1,2,3\n",
        "a,a\n1,2\n",
        "a,\n1,2\n",
        "a\n\\xGG\n",
    ] {
        assert!(import_csv(csv).is_err(), "accepted {csv:?}");
    }
    for json in [
        r#"[{"a":1},{"b":2}]"#,
        r#"[{"a":1,"a":2}]"#,
        r#"[{"a":[]}]"#,
        r#"[{"a":{"x":1}}]"#,
        r#"[{"a":{"$binary":"GG"}}]"#,
        r#"{"a":1}"#,
        r#"[{"":1}]"#,
    ] {
        assert!(import_json(json).is_err(), "accepted {json}");
    }
    let row = import_json(r#"[{"bool":true,"num":42,"nil":null}]"#).unwrap();
    assert_eq!(
        row[0],
        vec![
            ("bool".into(), Cell::Text("true".into())),
            ("nil".into(), Cell::Null),
            ("num".into(), Cell::Text("42".into()))
        ]
    );
}

#[test]
fn empty_imports_and_exports() {
    assert!(import_csv("").unwrap().is_empty());
    assert!(import_json("[]").unwrap().is_empty());
    assert_eq!(export_csv(&QueryResult::default()).unwrap(), "");
    assert_eq!(export_json(&QueryResult::default()).unwrap(), "[]");
}

#[test]
fn exports_reject_bad_shapes_and_duplicate_headers() {
    let mut result = sample();
    result.rows[0].pop();
    assert!(export_csv(&result).is_err());
    assert!(export_json(&result).is_err());
    assert!(
        export_sql(
            Engine::Postgres,
            &TableRef {
                schema: "public".into(),
                name: "t".into()
            },
            &result
        )
        .is_err()
    );
    result = sample();
    result.columns[0].name = result.columns[1].name.clone();
    assert!(export_csv(&result).is_err());
}

#[test]
fn sql_export_quotes_identifiers_and_values_by_dialect() {
    let mut result = sample();
    result.columns[0].name = "a\"`b".into();
    result.rows[0][1] = Cell::Text("'); DROP TABLE users; --\\".into());
    let table = TableRef {
        schema: "s\"`".into(),
        name: "t\"`".into(),
    };
    let pg = export_sql(Engine::Postgres, &table, &result).unwrap();
    assert!(pg.starts_with("INSERT INTO \"s\"\"`\".\"t\"\"`\" (\"a\"\"`b\""));
    assert!(pg.contains("E'''); DROP TABLE users; --\\\\'"));
    assert!(pg.contains("decode('000a0d7fff', 'hex')"));
    let mysql = export_sql(Engine::MySql, &table, &result).unwrap();
    assert!(mysql.contains("`s\"```.`t\"```"));
    assert!(!mysql.contains("DROP TABLE"));
    assert!(mysql.contains("X'000a0d7fff'"));
}

#[test]
fn storage_roundtrip_is_private_atomic_and_capped() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_at(directory.path().join("private/nested")).unwrap();
    assert!(store.load().unwrap().connections.is_empty());
    let state = AppState {
        connections: vec![ConnectionProfile::default()],
        history: (0..HISTORY_LIMIT + 17)
            .map(|i| HistoryEntry {
                id: i.to_string(),
                sql: format!("SELECT {i}"),
                ..Default::default()
            })
            .collect(),
        snippets: vec![Snippet {
            id: "s1".into(),
            name: "日本語".into(),
            sql: "SELECT 'a'".into(),
        }],
        last_connection_id: Some("chosen".into()),
        ..Default::default()
    };
    store.save(&state).unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded.history.len(), HISTORY_LIMIT);
    assert_eq!(loaded.history[0].id, "17");
    assert_eq!(loaded.connections[0].id, state.connections[0].id);
    assert_eq!(loaded.snippets[0].name, "日本語");
    assert_eq!(loaded.last_connection_id.as_deref(), Some("chosen"));
    assert_eq!(
        fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let state_path = store.path().join("state.json");
    assert_eq!(
        fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        !fs::read_to_string(&state_path)
            .unwrap()
            .contains("password")
    );
    store.save(&AppState::default()).unwrap();
    assert!(store.load().unwrap().history.is_empty());
    assert_eq!(fs::read_dir(store.path()).unwrap().count(), 1);
}

#[test]
fn corrupt_state_and_symlinks_do_not_overwrite_unrelated_files() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_at(directory.path().join("data")).unwrap();
    let path = store.path().join("state.json");
    fs::write(&path, b"broken").unwrap();
    assert!(store.load().is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "broken");
    fs::remove_file(&path).unwrap();
    let target = directory.path().join("other");
    fs::write(&target, b"do not change").unwrap();
    symlink(&target, &path).unwrap();
    assert!(store.load().is_err());
    store.save(&AppState::default()).unwrap();
    assert_eq!(fs::read_to_string(target).unwrap(), "do not change");
    let link = directory.path().join("directory-link");
    symlink(store.path(), &link).unwrap();
    assert!(Store::open_at(link).is_err());
}

#[test]
fn old_state_gets_defaults_and_sql_formatting_works() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_at(directory.path()).unwrap();
    fs::write(store.path().join("state.json"), "{}").unwrap();
    assert!(store.load().unwrap().snippets.is_empty());
    let formatted = format_sql("select id,name from people where id=1");
    assert!(formatted.contains('\n'));
    assert!(formatted.contains("people"));
}

#[tokio::test]
async fn tunnel_without_ssh_is_noop_and_restore_rejects_read_only() {
    let profile = ConnectionProfile::default();
    let (_tunnel, forwarded) = Tunnel::open(&profile).await.unwrap();
    assert_eq!(forwarded.host, profile.host);
    assert_eq!(forwarded.port, profile.port);
    let profile = ConnectionProfile {
        read_only: true,
        ..profile
    };
    let error = restore(
        &profile,
        "",
        std::path::Path::new("/does-not-exist"),
        &ClientRuntime::Native,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("read-only"));
}

#[tokio::test]
async fn ephemeral_transport_never_persists_or_silently_bypasses_native_tls() {
    let directory = tempfile::tempdir().unwrap();
    let profile = ConnectionProfile {
        transport: Some(rowboat_core::ConnectionTransport::UnixSocket {
            path: directory.path().join("private.sock"),
        }),
        ..Default::default()
    };
    let encoded = serde_json::to_string(&profile).unwrap();
    assert!(!encoded.contains("transport"));
    assert!(!encoded.contains("private.sock"));
    let mut value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    value["transport"] = serde_json::json!({"UnixSocket": {"path": "/untrusted/socket"}});
    let decoded: ConnectionProfile = serde_json::from_value(value.clone()).unwrap();
    assert!(decoded.transport.is_none());
    let document =
        serde_json::json!({"format":"rowboat-connections", "version":1, "connections":[value]});
    assert!(import_connections(&document.to_string()).is_err());
    let destination = directory.path().join("backup.sql");
    let error = backup(&profile, "secret", &destination, &ClientRuntime::Native)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cannot preserve TLS hostname"));
    assert!(!destination.exists());
    std::fs::write(&destination, "SELECT 1;").unwrap();
    let error = restore(&profile, "secret", &destination, &ClientRuntime::Native)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cannot preserve TLS hostname"));
}

#[test]
fn credential_input_limits_fail_without_spawning_or_echoing_the_secret() {
    for password in ["secret\0tail".to_owned(), "secret".repeat(2000)] {
        let error = credential_set("validation-test", &password).unwrap_err();
        assert!(!format!("{error:?}").contains(&password));
    }
}

#[tokio::test]
async fn ssh_podman_backup_rejects_ip_literal_identity_without_starting_tools() {
    let directory = tempfile::tempdir().unwrap();
    for host in ["127.0.0.1", "2001:db8::1"] {
        let profile = ConnectionProfile {
            host: host.into(),
            ssl_mode: "verify-full".into(),
            transport: Some(rowboat_core::ConnectionTransport::UnixSocket {
                path: directory.path().join("not-a-socket"),
            }),
            ..Default::default()
        };
        let path = directory.path().join("backup.sql");
        let error = backup(
            &profile,
            "secret",
            &path,
            &ClientRuntime::Podman {
                image: "not-needed".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("requires a DNS hostname"));
        assert!(!path.exists());
    }
}
