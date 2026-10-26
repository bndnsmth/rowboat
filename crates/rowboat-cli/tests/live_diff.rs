use anyhow::{Result, ensure};
use serde_json::Value;
use std::{
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

fn invoke(url: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rowboatctl"))
        .env_remove("ROWBOAT_URL")
        .env_remove("ROWBOAT_SOURCE_URL")
        .env("ROWBOAT_PASSWORD", "rowboat-test")
        .env("ROWBOAT_SOURCE_PASSWORD", "rowboat-test")
        .args(["--url", url])
        .args(args)
        .output()
        .unwrap()
}

fn text(output: Output) -> Result<String> {
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn query(url: &str, sql: &str) -> Result<String> {
    text(invoke(url, &["query", "--sql", sql, "--format", "json"]))
}

fn roundtrip(mysql: bool) -> Result<()> {
    let url = if mysql {
        "mysql://root@127.0.0.1:13309/rowboat?sslmode=disable"
    } else {
        "postgres://postgres@127.0.0.1:15435/rowboat?sslmode=disable"
    };
    let id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let source = format!("kc{id}_src");
    let destination = format!("kc{id}_dst");
    let namespace = if mysql { "DATABASE" } else { "SCHEMA" };
    query(
        url,
        &format!(
            "CREATE {namespace} {source}; CREATE {namespace} {destination}; CREATE TABLE {source}.entries (id INTEGER PRIMARY KEY, body VARCHAR(30))"
        ),
    )?;
    let snapshot: Value = serde_json::from_str(&text(invoke(
        url,
        &["schema-snapshot", "--schema", &source],
    ))?)?;
    ensure!(snapshot.to_string().contains("entries"));
    let args = [
        "compare-live",
        "--source-url",
        url,
        "--source-schema",
        &source,
        "--destination-schema",
        &destination,
    ];
    let report: Value = serde_json::from_str(&text(invoke(url, &args))?)?;
    ensure!(report["blockers"].as_array().is_some_and(Vec::is_empty));
    ensure!(!report["plan"]["statements"].as_array().unwrap().is_empty());
    let count: Value = serde_json::from_str(&query(
        url,
        &format!(
            "SELECT count(*) AS n FROM information_schema.tables WHERE table_schema='{destination}'"
        ),
    )?)?;
    ensure!(count[0]["n"] == "0", "Comparison unexpectedly executed DDL");
    let mut sql_args = args.to_vec();
    sql_args.extend(["--format", "sql"]);
    let sql = text(invoke(url, &sql_args))?;
    ensure!(sql.contains(&destination));
    query(url, &sql)?;
    let equal: Value = serde_json::from_str(&text(invoke(url, &args))?)?;
    ensure!(
        equal["plan"]["statements"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "Applied migration did not converge: {equal}"
    );
    query(
        url,
        &format!(
            "ALTER TABLE {source}.entries ADD COLUMN computed INTEGER GENERATED ALWAYS AS (id + 1) STORED"
        ),
    )?;
    let blocked = invoke(url, &args);
    ensure!(
        !blocked.status.success(),
        "Unsupported schema was reported as a usable migration"
    );
    let blocked: Value = serde_json::from_slice(&blocked.stdout)?;
    ensure!(!blocked["blockers"].as_array().unwrap().is_empty());
    ensure!(
        blocked["plan"]["statements"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    let blocked_sql = invoke(url, &sql_args);
    ensure!(
        !blocked_sql.status.success() && blocked_sql.stdout.is_empty(),
        "Blocked comparison emitted executable SQL"
    );
    query(
        url,
        &format!(
            "DROP TABLE {source}.entries; DROP TABLE {destination}.entries; DROP {namespace} {source}; DROP {namespace} {destination}"
        ),
    )?;
    Ok(())
}

#[test]
#[ignore = "Requires task-owned Podman PostgreSQL on 15435"]
fn postgres_live_diff_cli_previews_and_converges() -> Result<()> {
    roundtrip(false)
}

#[test]
#[ignore = "Requires task-owned Podman MySQL root on 13309"]
fn mysql_live_diff_cli_previews_and_converges() -> Result<()> {
    roundtrip(true)
}

#[test]
fn live_diff_requires_explicit_source_and_namespace_mapping() {
    let output = invoke(
        "postgres://nobody@127.0.0.1:1/none",
        &[
            "compare-live",
            "--source-schema",
            "public",
            "--destination-schema",
            "public",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--source-connection"));
}
