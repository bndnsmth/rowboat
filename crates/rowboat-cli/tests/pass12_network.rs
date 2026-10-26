//! Compatibility checks against parent-owned disposable Podman fixtures only.
use anyhow::{Result, ensure};
use rowboat_core::{Engine, QueryEvent, sql::quote_ident};
use serde_json::Value;
use std::process::{Command, Output};

fn invoke(engine: Engine, args: &[&str]) -> Result<Output> {
    let url = match engine {
        Engine::Postgres => "postgres://postgres@127.0.0.1:15435/rowboat?sslmode=disable",
        Engine::MySql => "mysql://root@127.0.0.1:13309/rowboat?sslmode=disable",
        _ => unreachable!("This fixture is network-only"),
    };
    Ok(Command::new(env!("CARGO_BIN_EXE_rowboatctl"))
        .env_remove("ROWBOAT_URL")
        .env_remove("KEEL_URL")
        .env_remove("ROWBOAT_SSH_KNOWN_HOSTS")
        .env_remove("KEEL_SSH_KNOWN_HOSTS")
        .env("ROWBOAT_PASSWORD", "rowboat-test")
        .args(["--url", url])
        .args(args)
        .output()?)
}

fn success(output: Output) -> Result<String> {
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn query(engine: Engine, sql: &str) -> Result<Value> {
    Ok(serde_json::from_str(&success(invoke(
        engine,
        &["query", "--sql", sql, "--format", "json"],
    )?)?)?)
}

fn compatibility(engine: Engine) -> Result<()> {
    let name = format!(
        "pass12_cli_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let quoted = quote_ident(engine, &name);
    let column = quote_ident(engine, "Quoted Name");
    query(
        engine,
        &format!(
            "CREATE TABLE {quoted} (id INTEGER PRIMARY KEY, {column} VARCHAR(40)); INSERT INTO {quoted} VALUES (1,'one'),(2,'two')"
        ),
    )?;
    let result = (|| -> Result<()> {
        success(invoke(engine, &["ping"])?)?;
        let browse: Value = serde_json::from_str(&success(invoke(engine, &["browse", &name])?)?)?;
        ensure!(browse.as_array().unwrap().len() == 2);
        let structure: Value =
            serde_json::from_str(&success(invoke(engine, &["structure", &name])?)?)?;
        ensure!(
            structure["columns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == "Quoted Name")
        );
        let sql = format!("SELECT c.Qu FROM {quoted} c");
        let completion: Value = serde_json::from_str(&success(invoke(
            engine,
            &["complete-sql", &sql, "--caret", "11"],
        )?)?)?;
        ensure!(
            completion["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["insert_text"] == column),
            "{completion}"
        );
        let sql = format!("SELECT * FROM {quoted} ORDER BY id");
        let text = success(invoke(
            engine,
            &["query-stream", "--sql", &sql, "--batch-rows", "1"],
        )?)?;
        let events = text
            .lines()
            .map(serde_json::from_str::<QueryEvent>)
            .collect::<Result<Vec<_>, _>>()?;
        ensure!(matches!(
            events.last(),
            Some(QueryEvent::Complete {
                result_count: 1,
                ..
            })
        ));
        let expected_schema = if engine == Engine::Postgres {
            "public"
        } else {
            "rowboat"
        };
        let exported = success(invoke(engine, &["export", &name, "--format", "sql"])?)?;
        ensure!(exported.contains(&format!(
            "{}.{}",
            quote_ident(engine, expected_schema),
            quoted
        )));
        let account_args: &[&str] = if engine == Engine::Postgres {
            &["account-info", "postgres"]
        } else {
            &["account-info", "root", "--host", "%"]
        };
        success(invoke(engine, account_args)?)?;
        Ok(())
    })();
    let cleanup = query(engine, &format!("DROP TABLE {quoted}"));
    result?;
    cleanup?;
    Ok(())
}

#[test]
#[ignore = "Parent-owned disposable PostgreSQL on 15435"]
fn postgres_adapter_cli_compatibility() -> Result<()> {
    compatibility(Engine::Postgres)
}

#[test]
#[ignore = "Parent-owned disposable MySQL on 13309"]
fn mysql_adapter_cli_compatibility() -> Result<()> {
    compatibility(Engine::MySql)
}
