//! Real CLI subprocesses against task-owned SQLite files; no host server needed.
use anyhow::{Result, ensure};
use rowboat_core::{Cell, QueryEvent};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    directory: tempfile::TempDir,
    database: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("pass12 cli 'quoted' database.sqlite");
        Ok(Self {
            directory,
            database,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rowboatctl"));
        command
            .env("ROWBOAT_DATA_DIR", self.directory.path().join("state"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/nonexistent/pass12-no-keyring",
            )
            .env_remove("ROWBOAT_URL")
            .env_remove("KEEL_URL")
            .env_remove("ROWBOAT_PASSWORD")
            .env_remove("KEEL_PASSWORD")
            .env_remove("ROWBOAT_SSH_KNOWN_HOSTS")
            .env_remove("KEEL_SSH_KNOWN_HOSTS");
        // File sources must not even parse network credential environment values.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            command.env("ROWBOAT_PASSWORD", std::ffi::OsString::from_vec(vec![0xff]));
        }
        command
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        Ok(self
            .command()
            .arg("--database-file")
            .arg(&self.database)
            .args(args)
            .output()?)
    }

    fn query(&self, sql: &str) -> Result<Value> {
        Ok(serde_json::from_str(&success(
            self.run(&["query", "--sql", sql, "--format", "json"])?,
        )?)?)
    }

    fn create(&self) -> Result<()> {
        success(self.run(&["--create", "query", "--sql",
            "CREATE TABLE items (id INTEGER PRIMARY KEY, label TEXT); INSERT INTO items VALUES (1,'one'),(2,'two'),(3,NULL)"] )?)?;
        Ok(())
    }
}

fn success(output: Output) -> Result<String> {
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn failure(output: Output) -> Result<String> {
    ensure!(
        !output.status.success(),
        "Unexpected success: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(String::from_utf8(output.stderr)?)
}

#[test]
fn file_open_is_existing_by_default_and_creation_is_explicit() -> Result<()> {
    let fixture = Fixture::new()?;
    failure(fixture.run(&["query", "--sql", "SELECT 1"])?)?;
    ensure!(
        !fixture.database.exists(),
        "Opening missing file created it"
    );
    failure(fixture.run(&["--read-only", "query", "--sql", "SELECT 1"])?)?;
    ensure!(!fixture.database.exists());
    fixture.create()?;
    ensure!(fixture.database.is_file());
    ensure!(fixture.query("SELECT label FROM items WHERE id=2")?[0]["label"] == "two");
    ensure!(
        !fixture.directory.path().join("state").exists(),
        "Direct file opening touched profile state"
    );
    success(fixture.run(&["ping"])?)?;
    Ok(())
}

#[test]
fn file_flags_reject_conflicting_sources_before_any_creation() -> Result<()> {
    let fixture = Fixture::new()?;
    for args in [
        vec!["--create", "--url", "postgres://u@localhost/db", "ping"],
        vec!["--create", "--connection", "unused", "ping"],
        vec!["--create", "--read-only", "ping"],
        vec!["--create", "--ssh-known-hosts", "missing", "ping"],
    ] {
        failure(fixture.run(&args)?)?;
        ensure!(!fixture.database.exists());
    }
    failure(fixture.command().args(["--create", "ping"]).output()?)?;
    failure(
        fixture
            .command()
            .args(["--create", "--url", "mysql://u@localhost/db", "ping"])
            .output()?,
    )?;
    ensure!(!fixture.directory.path().join("state").exists());
    Ok(())
}

#[test]
fn readonly_file_queries_cannot_write_or_disable_policy() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.create()?;
    success(fixture.run(&["--read-only", "query", "--sql", "SELECT * FROM items"])?)?;
    for sql in [
        "INSERT INTO items VALUES (4,'blocked')",
        "PRAGMA query_only=OFF; UPDATE items SET label='blocked' WHERE id=1",
        "CREATE TABLE forbidden (id INTEGER)",
    ] {
        failure(fixture.run(&["--read-only", "query", "--sql", sql])?)?;
    }
    failure(fixture.run(&["--read-only", "query-stream", "--sql", "DELETE FROM items"])?)?;
    ensure!(fixture.query("SELECT count(*) AS n FROM items")?[0]["n"] == "3");
    ensure!(fixture.query("SELECT label FROM items WHERE id=1")?[0]["label"] == "one");
    Ok(())
}

#[test]
fn sqlite_stream_delivers_bounded_batches_and_terminal_results() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.create()?;
    let text = success(fixture.run(&[
        "query-stream",
        "--sql",
        "SELECT id,label FROM items ORDER BY id; SELECT x'00ff' AS payload, NULL AS absent",
        "--batch-rows",
        "1",
        "--unlimited",
    ])?)?;
    let events = text
        .lines()
        .map(serde_json::from_str::<QueryEvent>)
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(matches!(
        events.last(),
        Some(QueryEvent::Complete {
            result_count: 2,
            ..
        })
    ));
    let mut rows = Vec::new();
    let mut ends = 0;
    for event in events {
        match event {
            QueryEvent::Rows { rows: batch, .. } => {
                ensure!(batch.len() <= 1);
                rows.extend(batch);
            }
            QueryEvent::ResultEnd { truncated, .. } => {
                ensure!(!truncated);
                ends += 1;
            }
            QueryEvent::Error { message, .. } => anyhow::bail!("{message}"),
            _ => {}
        }
    }
    ensure!(ends == 2 && rows.len() == 4);
    ensure!(rows[0] == vec![Cell::Text("1".into()), Cell::Text("one".into())]);
    ensure!(rows[3] == vec![Cell::Binary(vec![0, 255]), Cell::Null]);
    let limited = success(fixture.run(&[
        "query-stream",
        "--sql",
        "SELECT * FROM items",
        "--limit",
        "1",
    ])?)?;
    let events = limited
        .lines()
        .map(serde_json::from_str::<QueryEvent>)
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(events.iter().any(|event| matches!(
        event,
        QueryEvent::ResultEnd {
            rows_emitted: 1,
            truncated: true,
            ..
        }
    )));
    Ok(())
}

#[test]
fn sqlite_catalog_structure_browse_and_exports_use_main_namespace() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.create()?;
    let tables: Value = serde_json::from_str(&success(fixture.run(&["tables"])?)?)?;
    ensure!(
        tables
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["table"] == json!({"schema":"main", "name":"items"})),
        "{tables}"
    );
    let structure: Value = serde_json::from_str(&success(fixture.run(&["structure", "items"])?)?)?;
    ensure!(
        structure["columns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "id" && c["primary_key"] == true),
        "{structure}"
    );
    let filter = fixture.directory.path().join("filter.json");
    std::fs::write(
        &filter,
        serde_json::to_vec(&json!({"Leaf":{"column":"id","op":"Greater","value":{"Text":"1"}}}))?,
    )?;
    let rows: Value = serde_json::from_str(&success(fixture.run(&[
        "browse",
        "items",
        "--filter",
        filter.to_str().unwrap(),
    ])?)?)?;
    ensure!(rows.as_array().unwrap().len() == 2, "{rows}");
    let sql = success(fixture.run(&["export", "items", "--format", "sql"])?)?;
    ensure!(sql.contains("INSERT INTO \"main\".\"items\""), "{sql}");
    let original = fixture.query("SELECT * FROM items ORDER BY id")?;
    fixture.query("DELETE FROM items")?;
    fixture.query(&sql)?;
    ensure!(
        fixture.query("SELECT * FROM items ORDER BY id")? == original,
        "SQL export did not round-trip"
    );
    let output = fixture.directory.path().join("export.csv");
    success(fixture.run(&[
        "export-stream",
        "items",
        "--output",
        output.to_str().unwrap(),
    ])?)?;
    let csv = std::fs::read_to_string(&output)?;
    ensure!(
        csv.contains("id,label") && csv.contains("1,one") && csv.lines().count() == 4,
        "{csv}"
    );
    failure(fixture.run(&[
        "export-stream",
        "items",
        "--output",
        output.to_str().unwrap(),
    ])?)?;
    ensure!(std::fs::read_to_string(output)? == csv);
    Ok(())
}

#[test]
fn sqlite_completion_quotes_names_and_respects_catalog_scope() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.create()?;
    fixture.query("CREATE TABLE \"Order Items\" (\"Item Name\" TEXT, id INTEGER PRIMARY KEY)")?;
    let sql = "SELECT o.It FROM main.\"Order Items\" o";
    let completion: Value = serde_json::from_str(&success(fixture.run(&[
        "complete-sql",
        sql,
        "--caret",
        "11",
        "--schema",
        "main",
    ])?)?)?;
    ensure!(
        completion["search_path"]
            .as_array()
            .unwrap()
            .contains(&json!("main")),
        "{completion}"
    );
    ensure!(
        completion["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["insert_text"] == "\"Item Name\""),
        "{completion}"
    );
    let table: Value = serde_json::from_str(&success(
        fixture.run(&["complete-sql", "SELECT * FROM main.Ord"])?,
    )?)?;
    ensure!(
        table["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["insert_text"] == "\"Order Items\""),
        "{table}"
    );
    let unknown: Value = serde_json::from_str(&success(
        fixture.run(&["complete-sql", "SELECT * FROM missing.Ord"])?,
    )?)?;
    ensure!(unknown["items"].as_array().unwrap().is_empty(), "{unknown}");
    let attached = fixture
        .directory
        .path()
        .join("completion-must-not-create.sqlite");
    let editor = format!(
        "ATTACH DATABASE '{}' AS extra; SELECT * FROM extra.Ord",
        attached.display()
    );
    success(fixture.run(&["complete-sql", &editor])?)?;
    ensure!(!attached.exists(), "Completion executed editor SQL");
    ensure!(fixture.query("SELECT count(*) AS n FROM items")?[0]["n"] == "3");
    failure(fixture.run(&["complete-sql", "SELECT 'é'", "--caret", "9"])?)?;
    Ok(())
}

#[test]
fn unsupported_sqlite_tools_fail_before_opening_files_or_starting_clients() -> Result<()> {
    let fixture = Fixture::new()?;
    let destination = fixture.directory.path().join("must-not-exist.sql");
    let clients = fixture.directory.path().join("clients");
    let client_marker = fixture.directory.path().join("client-started");
    std::fs::create_dir(&clients)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["podman", "ssh"] {
            let client = clients.join(name);
            std::fs::write(
                &client,
                "#!/bin/sh\n: > \"$PASS12_CLIENT_MARKER\"\nexit 77\n",
            )?;
            std::fs::set_permissions(client, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    for args in [
        vec!["activity"],
        vec!["metrics"],
        vec!["metric-sample", "SELECT 1"],
        vec!["principals"],
        vec!["grants", "unused"],
        vec!["account-info", "unused"],
        vec!["access", "unused"],
        vec!["objects", "role"],
        vec!["object-definition", "view", "unused"],
        vec![
            "object-review",
            "view",
            "unused",
            "--patch",
            destination.to_str().unwrap(),
        ],
        vec![
            "schema-snapshot",
            "--schema",
            "main",
            "--output",
            destination.to_str().unwrap(),
        ],
        vec!["explain", "SELECT 1", "--analyze"],
    ] {
        let mut create_args = vec!["--create"];
        create_args.extend(args);
        let error = failure(
            fixture
                .command()
                .env("PATH", &clients)
                .env("PASS12_CLIENT_MARKER", &client_marker)
                .arg("--database-file")
                .arg(&fixture.database)
                .args(&create_args)
                .output()?,
        )?;
        let reason = error.to_lowercase();
        ensure!(
            reason.contains("sqlite")
                && (reason.contains("unsupported")
                    || reason.contains("unavailable")
                    || reason.contains("not implemented")),
            "{error}"
        );
        ensure!(
            !fixture.database.exists(),
            "Unsupported command created database: {create_args:?}"
        );
        ensure!(!destination.exists(), "Unsupported command created output");
        ensure!(
            !client_marker.exists(),
            "Unsupported command started an external client"
        );
    }
    ensure!(!fixture.directory.path().join("state").exists());
    Ok(())
}

#[test]
fn sqlite_saved_profiles_are_offline_private_and_open_without_keyring() -> Result<()> {
    let fixture = Fixture::new()?;
    let state_dir = fixture.directory.path().join("state");
    success(
        fixture
            .command()
            .args([
                "profiles",
                "add-file",
                "--name",
                "SQLite reader",
                "--read-only",
            ])
            .arg(&fixture.database)
            .output()?,
    )?;
    ensure!(
        !fixture.database.exists(),
        "Saving metadata created a database"
    );
    let state_bytes = std::fs::read(state_dir.join("state.json"))?;
    let profiles: Value =
        serde_json::from_str(&success(fixture.command().arg("connections").output()?)?)?;
    ensure!(profiles[0]["read_only"] == true);
    fixture.create()?;
    let read = success(
        fixture
            .command()
            .args([
                "--connection",
                "SQLite reader",
                "query",
                "--sql",
                "SELECT label FROM items WHERE id=1",
                "--format",
                "json",
            ])
            .output()?,
    )?;
    let read: Value = serde_json::from_str(&read)?;
    ensure!(read[0]["label"] == "one");
    failure(
        fixture
            .command()
            .args([
                "--connection",
                "SQLite reader",
                "query",
                "--sql",
                "DELETE FROM items",
            ])
            .output()?,
    )?;
    let portable = fixture.directory.path().join("profiles.json");
    success(
        fixture
            .command()
            .args(["profiles", "export", "--output"])
            .arg(&portable)
            .output()?,
    )?;
    let exported: Value = serde_json::from_slice(&std::fs::read(&portable)?)?;
    ensure!(exported["connections"][0]["file"]["path"] == fixture.database.to_str().unwrap());
    ensure!(exported["connections"][0].get("host").is_none());
    ensure!(exported["connections"][0].get("username").is_none());
    let imported_dir = fixture.directory.path().join("imported-state");
    success(
        fixture
            .command()
            .env("ROWBOAT_DATA_DIR", &imported_dir)
            .args(["profiles", "import"])
            .arg(&portable)
            .output()?,
    )?;
    let imported: Value = serde_json::from_slice(&std::fs::read(imported_dir.join("state.json"))?)?;
    ensure!(imported["connections"][0]["id"] != profiles[0]["id"]);
    ensure!(imported["connections"][0]["read_only"] == true);
    success(
        fixture
            .command()
            .env("ROWBOAT_DATA_DIR", &imported_dir)
            .args([
                "--connection",
                "SQLite reader",
                "query",
                "--sql",
                "SELECT * FROM items",
            ])
            .output()?,
    )?;
    ensure!(std::fs::read(state_dir.join("state.json"))? == state_bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            std::fs::metadata(state_dir.join("state.json"))?
                .permissions()
                .mode()
                & 0o077
                == 0
        );
    }
    Ok(())
}

#[test]
fn sqlite_import_preview_does_not_write_before_explicit_apply() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.create()?;
    let file = fixture.directory.path().join("rows.json");
    std::fs::write(&file, r#"[{"id":"4","label":"four"}]"#)?;
    let preview = success(fixture.run(&[
        "import",
        "items",
        file.to_str().unwrap(),
        "--format",
        "json",
    ])?)?;
    ensure!(preview.contains("INSERT INTO"));
    ensure!(fixture.query("SELECT count(*) AS n FROM items")?[0]["n"] == "3");
    success(fixture.run(&[
        "import",
        "items",
        file.to_str().unwrap(),
        "--format",
        "json",
        "--apply",
    ])?)?;
    ensure!(fixture.query("SELECT label FROM items WHERE id=4")?[0]["label"] == "four");
    let stream_file = fixture.directory.path().join("rows.jsonl");
    std::fs::write(&stream_file, "{\"id\":\"5\",\"label\":\"five\"}\n")?;
    success(fixture.run(&[
        "import-stream",
        "items",
        stream_file.to_str().unwrap(),
        "--format",
        "jsonl",
    ])?)?;
    ensure!(fixture.query("SELECT count(*) AS n FROM items")?[0]["n"] == "4");
    success(fixture.run(&[
        "import-stream",
        "items",
        stream_file.to_str().unwrap(),
        "--format",
        "jsonl",
        "--apply",
    ])?)?;
    ensure!(fixture.query("SELECT count(*) AS n FROM items")?[0]["n"] == "5");
    Ok(())
}

#[test]
fn sqlite_native_backup_restore_roundtrip_and_rejections() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.create()?;
    fixture.query("PRAGMA journal_mode=WAL")?;
    let backup = fixture.directory.path().join("private snapshot.sqlite");
    success(fixture.run(&["--read-only", "backup", backup.to_str().unwrap()])?)?;
    let snapshot = std::fs::read(&backup)?;
    ensure!(snapshot.starts_with(b"SQLite format 3\0"));
    failure(fixture.run(&["backup", backup.to_str().unwrap()])?)?;
    ensure!(std::fs::read(&backup)? == snapshot);
    fixture.query("UPDATE items SET label='changed' WHERE id=1")?;
    failure(fixture.run(&["--read-only", "restore", backup.to_str().unwrap()])?)?;
    ensure!(fixture.query("SELECT label FROM items WHERE id=1")?[0]["label"] == "changed");
    success(fixture.run(&["restore", backup.to_str().unwrap()])?)?;
    ensure!(fixture.query("SELECT label FROM items WHERE id=1")?[0]["label"] == "one");
    let invalid = fixture.directory.path().join("not a snapshot.sql");
    std::fs::write(&invalid, "DROP TABLE items;")?;
    failure(fixture.run(&["restore", invalid.to_str().unwrap()])?)?;
    ensure!(fixture.query("SELECT count(*) AS n FROM items")?[0]["n"] == "3");
    for command in ["backup", "restore"] {
        failure(fixture.run(&[command, fixture.database.to_str().unwrap()])?)?;
        let error = failure(fixture.run(&[
            command,
            backup.to_str().unwrap(),
            "--image",
            "must-not-run",
        ])?)?;
        ensure!(error.contains("does not use --image"), "{error}");
    }
    ensure!(std::fs::read(&backup)? == snapshot);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(std::fs::metadata(&backup)?.permissions().mode() & 0o077 == 0);
    }
    ensure!(!fixture.directory.path().join("state").exists());
    Ok(())
}

#[test]
fn sqlite_backup_and_offline_format_never_create_source() -> Result<()> {
    let fixture = Fixture::new()?;
    let backup = fixture.directory.path().join("absent snapshot.sqlite");
    failure(fixture.run(&["--create", "backup", backup.to_str().unwrap()])?)?;
    ensure!(!fixture.database.exists() && !backup.exists());
    let sql = fixture.directory.path().join("sqlite.sql");
    std::fs::write(
        &sql,
        "select [name], x'CAFE', :value from [users] where [name] = 'it''s  -- literal';",
    )?;
    let formatted = success(fixture.run(&["--create", "format", sql.to_str().unwrap()])?)?;
    ensure!(formatted.contains("'it''s  -- literal'"));
    ensure!(formatted.contains("[name]") && formatted.contains(":value"));
    ensure!(!fixture.database.exists());
    let roundtrip = fixture.directory.path().join("formatted.sql");
    std::fs::write(&roundtrip, &formatted)?;
    ensure!(success(fixture.run(&["format", roundtrip.to_str().unwrap()])?)? == formatted);
    Ok(())
}

#[test]
fn sqlite_stream_import_formats_and_atomic_failure() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.create()?;
    for (format, contents, expected) in [
        ("csv", "id,label\n4,four\n5,five\n", 5),
        (
            "jsonl",
            "{\"id\":6,\"label\":null}\n{\"id\":7,\"label\":\"line\\nvalue\"}\n",
            7,
        ),
        (
            "json",
            "[{\"id\":8,\"label\":\"eight\"},{\"id\":9,\"label\":\"nine\"}]",
            9,
        ),
    ] {
        let input = fixture.directory.path().join(format!("rows.{format}"));
        std::fs::write(&input, contents)?;
        success(fixture.run(&[
            "import-stream",
            "items",
            input.to_str().unwrap(),
            "--format",
            format,
        ])?)?;
        ensure!(
            fixture.query("SELECT count(*) AS n FROM items")?[0]["n"] == (expected - 2).to_string()
        );
        success(fixture.run(&[
            "import-stream",
            "items",
            input.to_str().unwrap(),
            "--format",
            format,
            "--apply",
        ])?)?;
        ensure!(fixture.query("SELECT count(*) AS n FROM items")?[0]["n"] == expected.to_string());
    }
    ensure!(fixture.query("SELECT label FROM items WHERE id=6")?[0]["label"].is_null());
    ensure!(fixture.query("SELECT label FROM items WHERE id=7")?[0]["label"] == "line\nvalue");
    let input = fixture.directory.path().join("partial.csv");
    std::fs::write(&input, "id,label\n10,ten\n1,duplicate\n")?;
    failure(fixture.run(&[
        "import-stream",
        "items",
        input.to_str().unwrap(),
        "--format",
        "csv",
        "--apply",
    ])?)?;
    ensure!(fixture.query("SELECT count(*) AS n FROM items")?[0]["n"] == "9");
    std::fs::write(&input, "id,label\n10,ten\n")?;
    failure(fixture.run(&[
        "--read-only",
        "import-stream",
        "items",
        input.to_str().unwrap(),
        "--format",
        "csv",
        "--apply",
    ])?)?;
    ensure!(fixture.query("SELECT count(*) AS n FROM items")?[0]["n"] == "9");
    Ok(())
}
