use rowboat_core::{ConnectionProfile, Engine, SshConfig};
use rowboat_services::*;
use serde_json::{Value, json};

fn profile(id: &str) -> ConnectionProfile {
    ConnectionProfile {
        id: id.into(),
        name: "Same name".into(),
        ..Default::default()
    }
}

fn document(profiles: &[ConnectionProfile]) -> Value {
    json!({ "format": "rowboat-connections", "version": 1, "connections": profiles })
}

fn preserved(input: &str) -> anyhow::Result<Vec<ConnectionProfile>> {
    import_connections_with_options(
        input,
        &ImportOptions {
            id_policy: ImportIdPolicy::PreserveIds,
        },
    )
}

#[test]
fn rebrand_reads_both_v1_envelopes_and_exports_only_rowboat() {
    let source = profile("keel-existing-user-id");
    let exported = export_connections(std::slice::from_ref(&source)).unwrap();
    let mut document: Value = serde_json::from_str(&exported).unwrap();
    assert_eq!(document["format"], "rowboat-connections");
    assert_eq!(document["version"], 1);
    for magic in ["keel-connections", "rowboat-connections"] {
        document["format"] = magic.into();
        assert_eq!(preserved(&document.to_string()).unwrap()[0].id, source.id);
        assert_ne!(
            import_connections(&document.to_string()).unwrap()[0].id,
            source.id
        );
        document["version"] = 3.into();
        assert!(preserved(&document.to_string()).is_err());
        document["version"] = 1.into();
    }
    document["format"] = "unrelated-connections".into();
    assert!(preserved(&document.to_string()).is_err());
}

#[test]
fn portable_export_preserves_policy_but_omits_local_paths() {
    let mut original = profile("original");
    original.ssl_mode = "verify-full".into();
    original.ssl_ca = Some("/private/ca.pem".into());
    original.ssl_cert = Some("/private/cert.pem".into());
    original.ssl_key = Some("/private/client.key".into());
    original.read_only = true;
    original.ssh = Some(SshConfig {
        host: "bastion.example".into(),
        port: 2222,
        username: "jump".into(),
        identity_file: Some("/private/identity".into()),
    });
    let exported = export_connections(&[original.clone()]).unwrap();
    for forbidden in [
        "/private",
        "ssl_ca",
        "ssl_cert",
        "ssl_key",
        "identity_file",
        "password",
    ] {
        assert!(!exported.contains(forbidden), "export leaked {forbidden}");
    }
    let restored = import_connections(&exported).unwrap().remove(0);
    assert_ne!(restored.id, original.id);
    assert_eq!(restored.ssl_mode, "verify-full");
    assert!(restored.read_only);
    assert_eq!(restored.ssh.as_ref().unwrap().port, 2222);
    assert!(restored.ssl_ca.is_none());
    assert!(restored.ssh.unwrap().identity_file.is_none());
    let included = export_connections_with_options(
        &[original.clone()],
        &ExportOptions {
            include_local_paths: true,
        },
    )
    .unwrap();
    let restored = preserved(&included).unwrap().remove(0);
    assert_eq!(
        serde_json::to_value(restored).unwrap(),
        serde_json::to_value(original).unwrap()
    );
}

#[test]
fn every_default_import_gets_new_ids_and_same_names_are_allowed() {
    let source = [profile("first"), profile("second")];
    let exported = export_connections(&source).unwrap();
    let first = import_connections(&exported).unwrap();
    let second = import_connections(&exported).unwrap();
    let all_ids: std::collections::HashSet<_> = source
        .iter()
        .chain(&first)
        .chain(&second)
        .map(|p| &p.id)
        .collect();
    assert_eq!(all_ids.len(), 6);
    assert!(first.iter().all(|p| uuid::Uuid::parse_str(&p.id).is_ok()));
    let mut existing = source.to_vec();
    assert_eq!(
        merge_connections(&mut existing, first).unwrap(),
        ImportReport {
            added: 2,
            skipped: 0,
            replaced: 0
        }
    );
    assert_eq!(existing.len(), 4);
}

#[test]
fn duplicate_source_ids_are_rejected_before_regeneration() {
    let duplicate = [profile("same"), profile("same")];
    assert!(export_connections(&duplicate).is_err());
    let input = document(&duplicate).to_string();
    assert!(import_connections(&input).is_err());
    assert!(preserved(&input).is_err());
}

#[test]
fn merge_policy_is_explicit_and_reject_is_atomic() {
    let existing = vec![profile("old")];
    let mut replacement = profile("old");
    replacement.host = "other.example".into();
    let incoming = vec![profile("new"), replacement];
    let mut reject = existing.clone();
    assert!(merge_connections(&mut reject, incoming.clone()).is_err());
    assert_eq!(
        serde_json::to_value(&reject).unwrap(),
        serde_json::to_value(&existing).unwrap()
    );
    let mut skip = existing.clone();
    assert_eq!(
        merge_connections_with_policy(&mut skip, incoming.clone(), MergePolicy::Skip).unwrap(),
        ImportReport {
            added: 1,
            skipped: 1,
            replaced: 0
        }
    );
    assert_eq!(skip[0].host, existing[0].host);
    let mut replace = existing;
    assert_eq!(
        merge_connections_with_policy(&mut replace, incoming, MergePolicy::Replace).unwrap(),
        ImportReport {
            added: 1,
            skipped: 0,
            replaced: 1
        }
    );
    assert_eq!(replace[0].host, "other.example");
    assert_eq!(replace[1].id, "new");
}

#[test]
fn every_merge_policy_rejects_invalid_lists_without_partial_changes() {
    for policy in [MergePolicy::Reject, MergePolicy::Skip, MergePolicy::Replace] {
        let mut existing = vec![profile("old")];
        let mut invalid = profile("invalid");
        invalid.port = 0;
        assert!(
            merge_connections_with_policy(&mut existing, vec![profile("valid"), invalid], policy)
                .is_err()
        );
        assert_eq!(existing.len(), 1);
        assert!(
            merge_connections_with_policy(
                &mut existing,
                vec![profile("duplicate"), profile("duplicate")],
                policy
            )
            .is_err()
        );
        existing.push(profile("old"));
        assert!(merge_connections_with_policy(&mut existing, vec![], policy).is_err());
    }
}

#[test]
fn strict_documents_reject_secrets_unknown_fields_and_duplicate_json_keys() {
    let base = document(&[profile("one")]);
    for field in [
        "password",
        "Password",
        "secret",
        "credentials",
        "future_field",
    ] {
        let mut value = base.clone();
        value["connections"][0][field] = json!("VERY_SECRET");
        let error = import_connections(&value.to_string()).unwrap_err();
        assert!(!format!("{error:?}").contains("VERY_SECRET"));
    }
    let mut value = base.clone();
    value["password"] = json!("VERY_SECRET");
    assert!(import_connections(&value.to_string()).is_err());
    value = base.clone();
    value["connections"][0]["ssh"] =
        json!({"host":"jump.example", "port":22, "username":"jump", "password":"VERY_SECRET"});
    assert!(import_connections(&value.to_string()).is_err());
    let duplicate_envelope =
        base.to_string()
            .replacen("\"version\":1", "\"version\":1,\"version\":1", 1);
    assert!(import_connections(&duplicate_envelope).is_err());
    let duplicate_profile =
        base.to_string()
            .replacen("\"id\":\"one\"", "\"id\":\"one\",\"id\":\"two\"", 1);
    assert!(import_connections(&duplicate_profile).is_err());
}

#[test]
fn corrupt_or_incompatible_documents_are_rejected() {
    for input in ["", "{broken", "[]", "null", "{\"connections\":[]}"] {
        assert!(import_connections(input).is_err());
    }
    let base = document(&[profile("one")]);
    for (field, value) in [
        ("version", json!(3)),
        ("version", json!(1.1)),
        ("format", json!("other")),
    ] {
        let mut bad = base.clone();
        bad[field] = value;
        assert!(import_connections(&bad.to_string()).is_err());
    }
    for (field, value) in [
        ("engine", json!("SQLite")),
        ("port", json!(65536)),
        ("port", json!(-1)),
        ("read_only", json!("false")),
        ("ssl_mode", json!("secret-value")),
    ] {
        let mut bad = base.clone();
        bad["connections"][0][field] = value;
        let error = import_connections(&bad.to_string()).unwrap_err();
        assert!(!format!("{error:?}").contains("secret-value"));
    }
    assert!(
        import_connections(&export_connections(&[]).unwrap())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn validates_required_fields_dns_tls_and_ssh_without_reading_paths() {
    for mutate in [
        (|p: &mut ConnectionProfile| p.id.clear()) as fn(&mut ConnectionProfile),
        |p| p.name = " ".into(),
        |p| p.database.clear(),
        |p| p.username.clear(),
        |p| p.port = 0,
        |p| p.host = "bad host".into(),
        |p| p.host = "-option".into(),
        |p| p.host = "bad..example".into(),
        |p| p.host = "[::1]".into(),
        |p| p.host = "host:5432".into(),
        |p| p.host = "999.999.999.999".into(),
        |p| p.ssl_mode = "allow".into(),
        |p| p.ssl_key = Some(String::new()),
        |p| p.database = "x\0y".into(),
        |p| p.group = "a\nb".into(),
    ] {
        let mut bad = profile("valid");
        mutate(&mut bad);
        assert!(validate_profile(&bad).is_err(), "accepted {bad:?}");
    }
    let mut good = profile("good");
    good.host = "2001:db8::1".into();
    good.ssl_ca = Some("/nonexistent/ca.pem".into());
    good.ssh = Some(SshConfig {
        host: "localhost".into(),
        port: 22,
        username: "jump".into(),
        identity_file: Some("/nonexistent/id".into()),
    });
    validate_profile(&good).unwrap();
    good.ssh.as_mut().unwrap().port = 0;
    assert!(validate_profile(&good).is_err());
    good.ssh.as_mut().unwrap().port = 22;
    good.ssh.as_mut().unwrap().username = "bad user".into();
    assert!(validate_profile(&good).is_err());
}

#[test]
fn urls_extract_encoded_password_and_redact_debug_and_serialization() {
    let parsed = parse_connection_url("postgresql://us%40er:SUPER%3Asecret%40%2F%25%20%E2%98%83@[2001:db8::1]:5433/db%2F%E6%97%A5%E6%9C%AC?sslmode=verify-full&sslrootcert=%2Ftmp%2Fca%20file.pem").unwrap();
    assert_eq!(parsed.profile.host, "2001:db8::1");
    assert_eq!(parsed.profile.port, 5433);
    assert_eq!(parsed.profile.username, "us@er");
    assert_eq!(parsed.profile.database, "db/日本");
    assert_eq!(parsed.password.as_deref(), Some("SUPER:secret@/% ☃"));
    assert_eq!(parsed.profile.ssl_ca.as_deref(), Some("/tmp/ca file.pem"));
    assert!(!format!("{parsed:?}").contains("SUPER"));
    assert!(format!("{parsed:?}").contains("REDACTED"));
    let serialized = serde_json::to_string(&parsed).unwrap();
    assert!(!serialized.contains("SUPER"));
    assert!(!serialized.contains("password"));
    let output = connection_url(&parsed.profile).unwrap();
    assert!(!output.contains("SUPER"));
    let again = parse_connection_url(&output).unwrap();
    assert!(again.password.is_none());
    assert_eq!(again.profile.database, parsed.profile.database);
    assert_eq!(again.profile.username, parsed.profile.username);
    assert_eq!(again.profile.ssl_ca, parsed.profile.ssl_ca);
}

#[test]
fn url_roundtrips_both_engines_all_tls_modes_unicode_and_reserved_characters() {
    for engine in [Engine::Postgres, Engine::MySql] {
        for mode in ["disable", "prefer", "require", "verify-ca", "verify-full"] {
            let original = ConnectionProfile {
                engine,
                username: "日本@:/?#%+ user".into(),
                database: "日本@:/?#%+ db".into(),
                ssl_mode: mode.into(),
                ssl_ca: Some("/tmp/ca+ root".into()),
                ssl_cert: Some("/tmp/client?cert".into()),
                ssl_key: Some("/tmp/key#file".into()),
                ..profile("original")
            };
            let parsed = parse_connection_url(&connection_url(&original).unwrap()).unwrap();
            assert_eq!(parsed.profile.engine, engine);
            assert_eq!(parsed.profile.username, original.username);
            assert_eq!(parsed.profile.database, original.database);
            assert_eq!(parsed.profile.ssl_mode, mode);
            assert_eq!(parsed.profile.ssl_ca, original.ssl_ca);
            assert_eq!(parsed.profile.ssl_cert, original.ssl_cert);
            assert_eq!(parsed.profile.ssl_key, original.ssl_key);
            assert!(parsed.password.is_none());
        }
    }
}

#[test]
fn url_defaults_mysql_tls_aliases_and_empty_password_are_explicit() {
    let pg = parse_connection_url("postgres://u@localhost/db").unwrap();
    assert_eq!(pg.profile.port, 5432);
    assert_eq!(pg.profile.ssl_mode, "prefer");
    assert!(pg.password.is_none());
    let mysql =
        parse_connection_url("mysql://u:@localhost/db?ssl-mode=VERIFY_IDENTITY&ssl-ca=%2Fca")
            .unwrap();
    assert_eq!(mysql.profile.engine, Engine::MySql);
    assert_eq!(mysql.profile.port, 3306);
    assert_eq!(mysql.profile.ssl_mode, "verify-full");
    assert_eq!(mysql.password.as_deref(), Some(""));
    let plus = parse_connection_url("mysql://u:p+q@localhost/db?ssl-ca=%2Fa+b").unwrap();
    assert_eq!(plus.password.as_deref(), Some("p+q"));
    assert_eq!(plus.profile.ssl_ca.as_deref(), Some("/a b"));
}

#[test]
fn malformed_urls_never_leak_the_input_or_secret_in_errors() {
    for input in [
        "postgresql://u:SUPERSECRET@host:0/db",
        "postgresql://u:SUPERSECRET@host:65536/db",
        "postgresql://u:SUPERSECRET@host:/db",
        "postgresql://u:SUPERSECRET@host/db/extra",
        "postgresql://u:SUPERSECRET@host/db#fragment",
        "postgresql://u:SUPERSECRET@host/db?password=x",
        "postgresql://u:SUPERSECRET@host/db?sslmode=prefer&sslmode=require",
        "mysql://u:SUPERSECRET@host/db?sslmode=prefer&ssl-mode=REQUIRED",
        "postgresql://u:SUPERSECRET@host/db?sslmode=invalid",
        "postgresql://u:SUPERSECRET@host/db?sslcert=",
        "postgresql://u:SUPERSECRET@host/db?sslmode=%FF",
        "postgresql://u:SUPERSECRET@host/db%XY",
        "postgresql://u:SUPERSECRET@host/%00",
        "postgresql://u:SUPERSECRET@/db",
        "postgresql://u:SUPERSECRET@host/",
        "postgresql://u:SUPERSECRET@host",
        "postgresql://u:SUPERSECRET@host/db\n",
        "postgresql://u:SUPERSECRET@host name/db",
        "postgresql://u:SUPERSECRET@%2Ftmp/db",
        "postgresql://u:SUPERSECRET@host,other/db",
        "postgresql://u:SUPERSECRET@host/db?sslmode",
        "postgresql://u:SUPERSECRET@host/db?sslmode=prefer&",
        "postgresql://u:SUPERSECRET@@host/db",
        "postgresql://u:SUPERSECRET%00@host/db",
        "sqlite://u:SUPERSECRET@host/db",
        "postgresql://host/db",
        "mysql://@host/db",
        "not-a-url",
    ] {
        let error = parse_connection_url(input).unwrap_err();
        assert!(!format!("{error:#}").contains("SUPERSECRET"));
        assert!(!format!("{error:?}").contains("SUPERSECRET"));
    }
}

#[test]
fn urls_refuse_to_silently_drop_read_only_or_ssh_policy() {
    let mut original = profile("original");
    original.read_only = true;
    assert!(connection_url(&original).is_err());
    original.read_only = false;
    original.ssh = Some(SshConfig {
        host: "jump.example".into(),
        port: 22,
        username: "jump".into(),
        identity_file: None,
    });
    assert!(connection_url(&original).is_err());
}

#[test]
fn url_normalization_preserves_database_dot_segments_and_international_hosts() {
    for database in [".", "..", "../name", "db/"] {
        let original = ConnectionProfile {
            database: database.into(),
            host: "bücher.example".into(),
            ..profile("id")
        };
        let parsed = parse_connection_url(&connection_url(&original).unwrap()).unwrap();
        assert_eq!(parsed.profile.database, database);
        assert_eq!(parsed.profile.host, "xn--bcher-kva.example");
    }
    assert_eq!(
        parse_connection_url("postgres://u@bücher.example/db")
            .unwrap()
            .profile
            .host,
        "xn--bcher-kva.example"
    );
}
