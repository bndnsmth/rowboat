use rowboat_core::{
    ConnectionProfile, ConnectionTransport, Engine, SqliteFileOptions, SqliteOpenMode, SshConfig,
};
use rowboat_services::*;
use serde_json::{Value, json};
use std::path::PathBuf;

fn local_directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("pass12_services_")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
}

fn file_profile(path: impl Into<PathBuf>) -> ConnectionProfile {
    ConnectionProfile {
        name: "SQLite file".into(),
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: path.into(),
            mode: SqliteOpenMode::OpenExisting,
        }),
        // File targets must work without even syntactically valid network fields.
        host: String::new(),
        port: 0,
        database: String::new(),
        username: String::new(),
        ssl_mode: String::new(),
        ..Default::default()
    }
}

#[test]
fn portable_file_targets_preserve_path_mode_readonly_and_never_touch_the_file() {
    let directory = local_directory();
    let path = directory.path().join("absent 日本語 ' #?.sqlite");
    for (mode, read_only) in [
        (SqliteOpenMode::OpenExisting, false),
        (SqliteOpenMode::OpenExisting, true),
        (SqliteOpenMode::CreateIfMissing, false),
    ] {
        let mut profile = file_profile(&path);
        profile.file.as_mut().unwrap().mode = mode;
        profile.read_only = read_only;
        profile.ssl_ca = Some("/not/a/real/certificate".into());
        for include_local_paths in [false, true] {
            validate_profile(&profile).unwrap();
            let encoded = export_connections_with_options(
                std::slice::from_ref(&profile),
                &ExportOptions {
                    include_local_paths,
                },
            )
            .unwrap();
            let document: Value = serde_json::from_str(&encoded).unwrap();
            assert_eq!(document["version"], 2);
            let wire = &document["connections"][0];
            for omitted in [
                "host", "port", "database", "username", "ssl_mode", "ssl_ca", "ssh", "password",
            ] {
                assert!(
                    wire.get(omitted).is_none(),
                    "unexpected network field {omitted}"
                );
            }
            let decoded = import_connections_with_options(
                &encoded,
                &ImportOptions {
                    id_policy: ImportIdPolicy::PreserveIds,
                },
            )
            .unwrap()
            .remove(0);
            assert_eq!(decoded.id, profile.id);
            assert_eq!(decoded.file.as_ref().unwrap().path, path);
            assert_eq!(decoded.file.as_ref().unwrap().mode, mode);
            assert_eq!(decoded.read_only, read_only);
            assert!(decoded.host.is_empty());
            assert_eq!(decoded.port, 0);
            assert!(decoded.ssl_ca.is_none());
            assert_ne!(import_connections(&encoded).unwrap()[0].id, profile.id);
            assert!(!path.exists());
        }
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn portable_versions_preserve_network_v1_legacy_and_mixed_file_profiles() {
    for engine in [Engine::Postgres, Engine::MySql] {
        let network = ConnectionProfile {
            engine,
            ..Default::default()
        };
        let encoded = export_connections(std::slice::from_ref(&network)).unwrap();
        let mut document: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(document["version"], 1);
        assert!(document["connections"][0].get("file").is_none());
        for magic in ["keel-connections", "rowboat-connections"] {
            document["format"] = magic.into();
            let imported = import_connections(&document.to_string()).unwrap();
            assert_eq!(imported[0].engine, engine);
            assert!(imported[0].file.is_none());
            assert_eq!(imported[0].host, network.host);
        }
        let file = file_profile("relative/path with spaces.sqlite");
        let mixed = export_connections(&[network, file]).unwrap();
        let mut document: Value = serde_json::from_str(&mixed).unwrap();
        assert_eq!(document["version"], 2);
        let decoded = import_connections(&mixed).unwrap();
        assert!(decoded[0].file.is_none());
        assert_eq!(
            decoded[1].file.as_ref().unwrap().path,
            PathBuf::from("relative/path with spaces.sqlite")
        );
        document["version"] = 1.into();
        assert!(import_connections(&document.to_string()).is_err());
        document["version"] = 2.into();
        document["format"] = "keel-connections".into();
        assert!(import_connections(&document.to_string()).is_err());
    }
}

#[test]
fn omitted_file_mode_defaults_to_open_existing_and_invalid_targets_are_rejected() {
    let profile = file_profile("missing.sqlite");
    let encoded = export_connections(std::slice::from_ref(&profile)).unwrap();
    let mut document: Value = serde_json::from_str(&encoded).unwrap();
    document["connections"][0]["file"]
        .as_object_mut()
        .unwrap()
        .remove("mode");
    let decoded = import_connections(&document.to_string()).unwrap().remove(0);
    assert_eq!(decoded.file.unwrap().mode, SqliteOpenMode::OpenExisting);
    for path in [
        "",
        " ",
        ":memory:",
        "file:/somewhere?mode=ro",
        "sqlite://somewhere",
        "nul\0file",
        "line\nfile",
    ] {
        assert!(
            validate_profile(&file_profile(path)).is_err(),
            "accepted {path:?}"
        );
    }
    assert!(validate_profile(&file_profile("x".repeat(4097))).is_err());
    let mut invalid = profile.clone();
    invalid.file = None;
    assert!(validate_profile(&invalid).is_err());
    invalid = profile.clone();
    invalid.read_only = true;
    invalid.file.as_mut().unwrap().mode = SqliteOpenMode::CreateIfMissing;
    assert!(validate_profile(&invalid).is_err());
    for engine in [Engine::Postgres, Engine::MySql] {
        let invalid = ConnectionProfile {
            engine,
            file: profile.file.clone(),
            ..Default::default()
        };
        assert!(validate_profile(&invalid).is_err());
    }
    let mut invalid = profile;
    invalid.transport = Some(ConnectionTransport::UnixSocket {
        path: "unused.socket".into(),
    });
    assert!(validate_profile(&invalid).is_err());
}

#[test]
fn strict_file_documents_reject_credentials_unknown_duplicate_and_mixed_target_fields() {
    let encoded = export_connections(&[file_profile("db.sqlite")]).unwrap();
    let base: Value = serde_json::from_str(&encoded).unwrap();
    for key in ["password", "host", "ssh", "transport", "future_field"] {
        let mut document = base.clone();
        document["connections"][0][key] = json!("do-not-echo-this-value");
        let error = import_connections(&document.to_string()).unwrap_err();
        assert!(!error.to_string().contains("do-not-echo"));
    }
    for key in ["password", "read_only", "future_field"] {
        let mut document = base.clone();
        document["connections"][0]["file"][key] = json!(true);
        assert!(import_connections(&document.to_string()).is_err());
    }
    for replacement in [
        "\"path\":\"other.sqlite\",\"path\"",
        "\"mode\":\"CreateIfMissing\",\"path\"",
    ] {
        assert!(import_connections(&encoded.replacen("\"path\"", replacement, 1)).is_err());
    }
    let mut wrong_engine = base;
    wrong_engine["connections"][0]["engine"] = "Postgres".into();
    assert!(import_connections(&wrong_engine.to_string()).is_err());
}

#[test]
fn saved_file_profiles_and_legacy_state_load_without_target_creation() {
    let directory = local_directory();
    let path = directory.path().join("never-created.sqlite");
    let store = Store::open_at(directory.path().join("state")).unwrap();
    let network = ConnectionProfile::default();
    let mut value = serde_json::to_value(&network).unwrap();
    value.as_object_mut().unwrap().remove("file");
    let legacy: ConnectionProfile = serde_json::from_value(value).unwrap();
    assert!(legacy.file.is_none());
    let file = file_profile(&path);
    store
        .save(&AppState {
            connections: vec![legacy, file.clone()],
            ..Default::default()
        })
        .unwrap();
    let loaded = store.load().unwrap();
    assert!(loaded.connections[0].file.is_none());
    assert_eq!(loaded.connections[1].file.as_ref().unwrap().path, path);
    assert_eq!(
        loaded.connections[1].file.as_ref().unwrap().mode,
        SqliteOpenMode::OpenExisting
    );
    assert!(!path.exists());
    let mut existing = vec![file.clone()];
    let before = serde_json::to_value(&existing).unwrap();
    assert!(merge_connections(&mut existing, vec![file]).is_err());
    assert_eq!(serde_json::to_value(&existing).unwrap(), before);
}

#[tokio::test]
async fn file_services_pass_through_or_fail_before_files_processes_and_network() {
    let directory = local_directory();
    let path = directory.path().join("not-created.sqlite");
    let profile = file_profile(&path);
    let (_guard, forwarded) = Tunnel::open(&profile).await.unwrap();
    assert_eq!(forwarded.file.as_ref().unwrap().path, path);
    assert!(forwarded.transport.is_none());
    assert!(builtin_metrics(Engine::Sqlite).is_empty());
    assert_eq!(
        default_backup_image(Engine::Postgres).unwrap(),
        "docker.io/library/postgres:18"
    );
    assert_eq!(
        default_backup_image(Engine::MySql).unwrap(),
        "docker.io/library/mysql:8.4"
    );
    let error = default_backup_image(Engine::Sqlite).unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<UnsupportedServiceOperation>()
            .unwrap()
            .operation,
        ServiceOperation::Backup
    );
    for operation in [ServiceOperation::SshTunnel, ServiceOperation::ConnectionUrl] {
        let error = service_support(Engine::Sqlite, operation).unwrap_err();
        assert_eq!(error.operation, operation);
        assert!(!error.reason.is_empty());
        for engine in [Engine::Postgres, Engine::MySql] {
            service_support(engine, operation).unwrap();
        }
    }
    for operation in [
        ServiceOperation::Backup,
        ServiceOperation::Restore,
        ServiceOperation::FormatSql,
    ] {
        for engine in [Engine::Sqlite, Engine::Postgres, Engine::MySql] {
            service_support(engine, operation).unwrap();
        }
    }
    let formatted =
        format_sql_for_engine(Engine::Sqlite, "SELECT [some field] --no space\n FROM t").unwrap();
    assert!(formatted.contains("[some field]"));
    assert!(formatted.contains("--no space\n"));
    let runtime = ClientRuntime::Podman {
        image: "must-not-be-used".into(),
    };
    for error in [
        backup(
            &profile,
            "",
            &directory.path().join("absent-parent/output"),
            &runtime,
        )
        .await
        .unwrap_err(),
        restore(
            &profile,
            "",
            &directory.path().join("absent-input"),
            &runtime,
        )
        .await
        .unwrap_err(),
        connection_url(&profile).unwrap_err(),
    ] {
        assert!(
            error
                .downcast_ref::<UnsupportedServiceOperation>()
                .is_some(),
            "{error}"
        );
    }
    let mut ssh_profile = profile.clone();
    ssh_profile.ssh = Some(SshConfig {
        host: "invalid".into(),
        port: 0,
        username: "".into(),
        identity_file: None,
    });
    let error = Tunnel::open(&ssh_profile).await.err().unwrap();
    assert!(
        error
            .downcast_ref::<UnsupportedServiceOperation>()
            .is_some()
    );
    let mut socket_profile = profile;
    socket_profile.transport = Some(ConnectionTransport::UnixSocket {
        path: "unused".into(),
    });
    assert!(Tunnel::open(&socket_profile).await.is_err());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn sqlite_url_forms_are_explicitly_unsupported_without_echoing_input() {
    for input in [
        "sqlite:data.db",
        "sqlite:///tmp/data.db?mode=rwc",
        "SQLITE://user:secret@host/db",
        "file:/tmp/data.db?mode=ro",
    ] {
        let error = parse_connection_url(input).unwrap_err();
        let typed = error.downcast_ref::<UnsupportedServiceOperation>().unwrap();
        assert_eq!(typed.operation, ServiceOperation::ConnectionUrl);
        assert_eq!(typed.engine, Engine::Sqlite);
        assert!(!error.to_string().contains("secret"));
        assert!(error.to_string().contains("explicit open mode"));
    }
}
