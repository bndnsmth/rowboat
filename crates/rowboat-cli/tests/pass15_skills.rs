//! Built-in guidance must be available from a standalone binary without state,
//! credentials or a database. Examples run only against disposable SQLite files.
use anyhow::{Result, ensure};
use rowboat_core::QueryEvent;
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

const GUIDE_COMMAND: &str = "rowboatctl skills get core --full";

fn command(directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rowboatctl"));
    command
        .env_clear()
        .current_dir(directory)
        .env("HOME", directory)
        .env("ROWBOAT_DATA_DIR", directory.join("state"))
        .env("PATH", directory.join("no-executables"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/rowboat-pass15",
        );
    command
}

fn hostile_command(directory: &Path) -> Command {
    let mut command = command(directory);
    command
        .env("ROWBOAT_URL", "not-a-database-url:PASS15-SECRET")
        .env("ROWBOAT_PASSWORD", "PASS15-SECRET")
        .env("ROWBOAT_DATA_DIR", directory.join("blocked/state"))
        .env("ROWBOAT_SSH_KNOWN_HOSTS", directory.join("missing-trust"));
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        // Reading this as credential text would fail before any connection.
        command.env("ROWBOAT_PASSWORD", std::ffi::OsString::from_vec(vec![0xff]));
    }
    command
}

fn success(output: Output) -> Result<String> {
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[test]
fn skills_are_offline_even_with_invalid_url_credentials_and_unusable_state() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("blocked"), "preserve me")?;
    for args in [
        vec!["skills", "list"],
        vec!["skills", "list", "--json"],
        vec!["skills", "get", "core"],
        vec!["skills", "get", "core", "--full"],
        vec!["--connection", "unused", "skills", "get", "core"],
        vec![
            "--database-file",
            "never-created.sqlite",
            "--create",
            "skills",
            "list",
        ],
    ] {
        let output = hostile_command(directory.path()).args(args).output()?;
        ensure!(
            output.stderr.is_empty(),
            "Offline success emitted diagnostics"
        );
        let text = success(output)?;
        ensure!(text.contains("core"));
        ensure!(!text.contains("PASS15-SECRET"));
    }
    ensure!(std::fs::read_to_string(directory.path().join("blocked"))? == "preserve me");
    ensure!(std::fs::read_dir(directory.path())?.count() == 1);
    Ok(())
}

#[test]
#[cfg(unix)]
fn non_utf8_connection_environment_cannot_block_offline_skill_or_help() -> Result<()> {
    use std::os::unix::ffi::OsStringExt;
    let directory = tempfile::tempdir()?;
    for args in [
        vec!["skills", "list"],
        vec!["skills", "get", "core", "--full"],
        vec!["--help"],
        vec!["query", "--help"],
        vec!["query", "-h"],
        vec!["skills", "get", "--help"],
        vec!["help", "query"],
        vec!["help", "skills", "get"],
    ] {
        let text = success(
            hostile_command(directory.path())
                .env(
                    "ROWBOAT_URL",
                    std::ffi::OsString::from_vec(b"PASS15-SECRET\xff".to_vec()),
                )
                .args(args)
                .output()?,
        )?;
        ensure!(text.contains("rowboatctl"));
        ensure!(!text.contains("PASS15-SECRET"));
    }
    ensure!(std::fs::read_dir(directory.path())?.count() == 0);
    Ok(())
}

#[test]
fn skill_discovery_and_quickstart_are_concise_and_full_is_complete() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let list = success(
        command(directory.path())
            .args(["skills", "list"])
            .output()?,
    )?;
    ensure!(list.contains("core") && list.contains(GUIDE_COMMAND));
    ensure!(list.lines().count() <= 5);
    let json: Value = serde_json::from_str(&success(
        command(directory.path())
            .args(["skills", "list", "--json"])
            .output()?,
    )?)?;
    ensure!(json.as_array().is_some_and(|items| items.len() == 1));
    ensure!(json[0]["id"] == "core" && json[0]["command"] == GUIDE_COMMAND);
    let short = success(
        command(directory.path())
            .args(["skills", "get", "core"])
            .output()?,
    )?;
    let full = success(
        command(directory.path())
            .args(["skills", "get", "core", "--full"])
            .output()?,
    )?;
    ensure!(short.len() < 4000 && full.len() > short.len() * 2);
    ensure!(short.contains(GUIDE_COMMAND));
    for text in [&short, &full] {
        for contract in [
            "json-envelope",
            "--timeout 30s",
            "--read-only",
            "truncated",
            "--create",
            "rollback guarantee",
        ] {
            ensure!(text.contains(contract), "Guide missing {contract}");
        }
    }
    Ok(())
}

#[test]
fn unknown_skill_fails_with_available_ids_without_stdout_or_state() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("blocked"), "preserve me")?;
    for id in ["missing", "CORE"] {
        let output = hostile_command(directory.path())
            .args(["skills", "get", id, "--full"])
            .output()?;
        ensure!(!output.status.success());
        ensure!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr)?;
        ensure!(error.contains("Unknown skill"));
        ensure!(error.contains("Available IDs: core"));
        ensure!(error.contains("rowboatctl skills list"));
        ensure!(!error.contains("PASS15-SECRET"));
    }
    ensure!(std::fs::read_dir(directory.path())?.count() == 1);
    Ok(())
}

#[test]
fn root_and_query_help_discover_the_full_guide_offline() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("blocked"), "preserve me")?;
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["query", "--help"],
        vec!["query", "-h"],
        vec!["help", "query"],
    ] {
        let query_help = args.contains(&"query");
        let help = success(hostile_command(directory.path()).args(args).output()?)?;
        ensure!(help.contains(GUIDE_COMMAND));
        ensure!(help.contains(if query_help {
            "Usage: rowboatctl query"
        } else {
            "Usage: rowboatctl [OPTIONS] <COMMAND>"
        }));
        ensure!(help.contains("ROWBOAT_URL"));
        ensure!(help.contains("ROWBOAT_SSH_KNOWN_HOSTS"));
        ensure!(!help.contains("PASS15-SECRET"));
        ensure!(!help.contains("missing-trust"));
    }
    for args in [
        vec!["skills", "--help"],
        vec!["skills", "list", "--help"],
        vec!["skills", "get", "--help"],
        vec!["help", "skills", "get"],
    ] {
        let subcommand = if args.contains(&"get") {
            " get"
        } else if args.contains(&"list") {
            " list"
        } else {
            ""
        };
        let help = success(hostile_command(directory.path()).args(args).output()?)?;
        ensure!(help.contains(&format!("Usage: rowboatctl skills{subcommand} ")));
        ensure!(!help.contains("PASS15-SECRET"));
        ensure!(help.contains("ROWBOAT_URL"));
    }
    Ok(())
}

#[test]
fn copied_binary_serves_the_guide_without_companion_files() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let standalone = directory.path().join("rowboatctl");
    std::fs::copy(env!("CARGO_BIN_EXE_rowboatctl"), &standalone)?;
    let output = Command::new(&standalone)
        .env_clear()
        .current_dir(directory.path())
        .env("HOME", directory.path().join("absent-home"))
        .env("ROWBOAT_DATA_DIR", directory.path().join("absent-state"))
        .args(["skills", "get", "core", "--full"])
        .output()?;
    let full = success(output)?;
    ensure!(full.contains("complete agent guide"));
    ensure!(full.contains("rows_returned") && full.contains("json-envelope"));
    ensure!(std::fs::read_dir(directory.path())?.count() == 1);
    Ok(())
}

#[test]
fn documented_sqlite_exploration_query_file_and_stream_commands_work() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let db = directory.path().join("project.sqlite");
    let run = |args: &[&str]| -> Result<Output> {
        Ok(command(directory.path())
            .arg("--database-file")
            .arg(&db)
            .args(args)
            .output()?)
    };
    // Confirm the guide's existing-file rule before explicitly creating fixtures.
    ensure!(!run(&["--read-only", "tables"])?.status.success());
    ensure!(!db.exists());
    success(run(&[
        "--create",
        "query",
        "--sql",
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); \
         INSERT INTO users VALUES (1,'Ada'), (2,'Grace'); \
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id))",
    ])?)?;
    for args in [
        vec!["--read-only", "tables"],
        vec!["--read-only", "structure", "users", "--schema", "main"],
        vec!["--read-only", "relationships", "users", "--schema", "main"],
    ] {
        let _: Value = serde_json::from_str(&success(run(&args)?)?)?;
    }
    let sql = "SELECT id, name FROM users ORDER BY id LIMIT 10";
    ensure!(include_str!("../skills/core.md").contains(sql));
    let envelope: Value = serde_json::from_str(&success(run(&[
        "--read-only",
        "query",
        "--sql",
        sql,
        "--format",
        "json-envelope",
        "--limit",
        "10",
        "--timeout",
        "30s",
        "--connect-timeout",
        "10s",
    ])?)?)?;
    ensure!(envelope["version"] == 1 && envelope["ok"] == true);
    ensure!(envelope["status"] == "complete");
    ensure!(envelope["results"][0]["complete"] == true);
    ensure!(envelope["results"][0]["truncated"] == false);
    ensure!(envelope["results"][0]["rows"][0] == serde_json::json!(["1", "Ada"]));

    std::fs::write(directory.path().join("report.sql"), sql)?;
    let file_result: Value = serde_json::from_str(&success(run(&[
        "--read-only",
        "query",
        "--file",
        "./report.sql",
        "--format",
        "json-envelope",
        "--limit",
        "100",
        "--timeout",
        "30s",
    ])?)?)?;
    ensure!(file_result["results"][0]["rows"] == envelope["results"][0]["rows"]);

    let stream = success(run(&[
        "--read-only",
        "query-stream",
        "--sql",
        "SELECT id, name FROM users ORDER BY id LIMIT 5000",
        "--limit",
        "5000",
        "--batch-rows",
        "256",
        "--timeout",
        "30s",
        "--connect-timeout",
        "10s",
    ])?)?;
    let events = stream
        .lines()
        .map(serde_json::from_str::<QueryEvent>)
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(matches!(events.last(), Some(QueryEvent::Complete { .. })));
    ensure!(!directory.path().join("state").exists());
    Ok(())
}
