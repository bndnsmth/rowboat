use anyhow::{Result, ensure};
use serde_json::Value;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn command(mysql: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rowboatctl"));
    command
        .env_remove("ROWBOAT_URL")
        .env_remove("KEEL_URL")
        .env("ROWBOAT_PASSWORD", "rowboat-test")
        .args([
            "--url",
            if mysql {
                "mysql://root@127.0.0.1:13309/rowboat?sslmode=disable"
            } else {
                "postgres://postgres@127.0.0.1:15435/rowboat?sslmode=disable"
            },
        ]);
    command
}

fn success(output: Output) -> Result<Value> {
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn metric_sampling(mysql: bool) -> Result<()> {
    let metric = |sql: &str| success(command(mysql).args(["metric-sample", sql]).output()?);
    let value = metric("SELECT CAST(12345678901234567890.1234 AS DECIMAL(24,4))")?;
    ensure!(value["state"]["kind"] == "value");
    ensure!(value["state"]["decimal"] == "12345678901234567890.1234");
    ensure!(value["sampled_at_unix_ms"].as_u64().is_some_and(|x| x > 0));
    ensure!(metric("SELECT 1 WHERE 1=0")?["state"]["kind"] == "empty");
    for sql in [
        "SELECT NULL",
        "SELECT 'not a number'",
        "SELECT 1 UNION ALL SELECT 2",
    ] {
        ensure!(metric(sql)?["state"]["kind"] == "invalid", "{sql}");
    }
    for sql in [
        "CREATE TABLE rowboat_cli6_forbidden (id INTEGER)",
        "SELECT 1; SELECT 2",
        "COMMIT",
    ] {
        let result = command(mysql).args(["metric-sample", sql]).output()?;
        ensure!(
            !result.status.success(),
            "Unexpected metric execution: {sql}"
        );
        ensure!(
            result.stdout.is_empty(),
            "Failed sampling emitted a success document"
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires disposable Rowboat PostgreSQL Podman fixture"]
fn postgres_scalar_metric_command() -> Result<()> {
    metric_sampling(false)
}

#[test]
#[ignore = "requires disposable Rowboat MySQL Podman fixture"]
fn mysql_scalar_metric_command() -> Result<()> {
    metric_sampling(true)
}

fn query(mysql: bool, sql: &str) -> Result<Value> {
    success(
        command(mysql)
            .args(["query", "--sql", sql, "--format", "json"])
            .output()?,
    )
}

fn unique_name() -> String {
    format!(
        "rowboat_cli6_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn json_array_import(mysql: bool) -> Result<()> {
    let name = unique_name();
    query(
        mysql,
        &format!("CREATE TABLE {name}(id INTEGER PRIMARY KEY, body TEXT, amount DECIMAL(24,4))"),
    )?;
    let check = (|| -> Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("rows.json");
        std::fs::write(
            &file,
            r#"[{"id":1,"body":"line1\nline2 café","amount":12345678901234567890.1234},{"id":2,"body":null,"amount":0}]"#,
        )?;
        let path = file.to_str().unwrap();
        let preview = success(
            command(mysql)
                .args(["import-stream", &name, path, "--format", "json-array"])
                .output()?,
        )?;
        ensure!(preview["rows"] == 2 && preview["applied"] == false);
        ensure!(query(mysql, &format!("SELECT count(*) AS n FROM {name}"))?[0]["n"] == "0");
        let applied = command(mysql)
            .args(["import-stream", &name, path, "--format", "json", "--apply"])
            .output()?;
        ensure!(
            applied.status.success(),
            "{}",
            String::from_utf8_lossy(&applied.stderr)
        );
        let rows = query(
            mysql,
            &format!("SELECT body,amount FROM {name} ORDER BY id"),
        )?;
        ensure!(rows[0]["body"] == "line1\nline2 café");
        ensure!(rows[0]["amount"] == "12345678901234567890.1234");
        ensure!(rows[1]["body"].is_null());
        std::fs::write(&file, r#"[{"id":3,"body":"must not persist","amount":1},]"#)?;
        let failed = command(mysql)
            .args([
                "import-stream",
                &name,
                path,
                "--format",
                "json-array",
                "--apply",
            ])
            .output()?;
        ensure!(!failed.status.success());
        ensure!(query(mysql, &format!("SELECT count(*) AS n FROM {name}"))?[0]["n"] == "2");
        Ok(())
    })();
    let cleanup = query(mysql, &format!("DROP TABLE {name}"));
    check?;
    cleanup?;
    Ok(())
}

#[test]
#[ignore = "requires disposable Rowboat PostgreSQL Podman fixture"]
fn postgres_streaming_json_array_cli() -> Result<()> {
    json_array_import(false)
}

#[test]
#[ignore = "requires disposable Rowboat MySQL Podman fixture"]
fn mysql_streaming_json_array_cli() -> Result<()> {
    json_array_import(true)
}

fn completion_path(mysql: bool) -> Result<()> {
    let name = unique_name();
    let extra = format!("{name}_extra");
    let namespace = if mysql { "DATABASE" } else { "SCHEMA" };
    query(
        mysql,
        &format!("CREATE TABLE {name}(id INTEGER PRIMARY KEY, body TEXT)"),
    )?;
    let check = (|| -> Result<()> {
        query(
            mysql,
            &format!(
                "CREATE {namespace} {extra}; CREATE TABLE {extra}.{name}(id INTEGER PRIMARY KEY, bogus TEXT)"
            ),
        )?;
        let sql = format!("SELECT t.bo FROM {name} t");
        let report = success(
            command(mysql)
                .args(["complete-sql", &sql, "--caret", "11"])
                .output()?,
        )?;
        let path = report["search_path"].as_array().unwrap();
        ensure!(
            path.iter()
                .any(|s| s == if mysql { "rowboat" } else { "public" })
        );
        ensure!(
            report["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["label"].as_str().is_some_and(|s| s.contains("body")))
        );
        // An extra catalog namespace cannot replace the real path's earlier table.
        let scoped = success(
            command(mysql)
                .args(["complete-sql", &sql, "--caret", "11", "--schema", &extra])
                .output()?,
        )?;
        let items = scoped["items"].as_array().unwrap();
        ensure!(
            items
                .iter()
                .any(|x| x["label"].as_str().is_some_and(|s| s.contains("body")))
        );
        ensure!(
            !items
                .iter()
                .any(|x| x["label"].as_str().is_some_and(|s| s.contains("bogus")))
        );
        Ok(())
    })();
    let cascade = if mysql { "" } else { " CASCADE" };
    let cleanup = query(
        mysql,
        &format!("DROP TABLE {name}; DROP {namespace} IF EXISTS {extra}{cascade}"),
    );
    check?;
    cleanup?;
    Ok(())
}

#[test]
#[ignore = "requires disposable Rowboat PostgreSQL Podman fixture"]
fn postgres_live_search_path_completion_cli() -> Result<()> {
    completion_path(false)
}

#[test]
#[ignore = "requires disposable Rowboat MySQL Podman fixture"]
fn mysql_current_database_completion_cli() -> Result<()> {
    completion_path(true)
}

fn definition_comparison(mysql: bool) -> Result<()> {
    let source = unique_name();
    let destination = format!("{source}_dst");
    let namespace = if mysql { "DATABASE" } else { "SCHEMA" };
    query(
        mysql,
        &format!("CREATE {namespace} {source}; CREATE {namespace} {destination}"),
    )?;
    let check = (|| -> Result<()> {
        for (schema, threshold) in [(&source, 1), (&destination, 0)] {
            query(
                mysql,
                &format!(
                    "CREATE TABLE {schema}.items(id INTEGER PRIMARY KEY,body TEXT); INSERT INTO {schema}.items VALUES(1,'one'),(2,'two'); CREATE VIEW {schema}.names AS SELECT id,body FROM {schema}.items WHERE id>{threshold}"
                ),
            )?;
            let definition = if mysql {
                format!(
                    "CREATE FUNCTION {schema}.bump(n INTEGER) RETURNS INTEGER DETERMINISTIC NO SQL RETURN n + {threshold}"
                )
            } else {
                format!(
                    "CREATE FUNCTION {schema}.bump(n INTEGER) RETURNS INTEGER LANGUAGE SQL AS 'SELECT n + {threshold}'"
                )
            };
            query(mysql, &definition)?;
        }
        let compare = |kind: &str, name: &str, scope: &str, format: &str| -> Result<Output> {
            let mut cmd = command(mysql);
            cmd.env("ROWBOAT_SOURCE_PASSWORD", "rowboat-test").args([
                "compare-live",
                "--source-url",
                if mysql {
                    "mysql://root@127.0.0.1:13309/rowboat?sslmode=disable"
                } else {
                    "postgres://postgres@127.0.0.1:15435/rowboat?sslmode=disable"
                },
                "--source-schema",
                &source,
                "--destination-schema",
                &destination,
                "--object-kind",
                kind,
                "--object-name",
                name,
                "--definition-scope",
                scope,
                "--format",
                format,
            ]);
            if kind == "function" && !mysql {
                cmd.args(["--signature", "n integer"]);
            }
            Ok(cmd.output()?)
        };
        for (kind, name, scope) in [
            ("view", "names", "view-query"),
            ("function", "bump", "routine-body"),
        ] {
            let report = success(compare(kind, name, scope, "json")?)?;
            ensure!(
                report["blockers"].as_array().is_some_and(Vec::is_empty),
                "{report}"
            );
            ensure!(
                report["differences"]
                    .as_array()
                    .is_some_and(|d| !d.is_empty()),
                "{report}"
            );
            let sql = compare(kind, name, scope, "sql")?;
            ensure!(
                sql.status.success(),
                "{}",
                String::from_utf8_lossy(&sql.stderr)
            );
            ensure!(String::from_utf8(sql.stdout)?.contains(&destination));
        }
        // Review must not change either destination's data-visible definition.
        ensure!(
            query(
                mysql,
                &format!("SELECT count(*) AS n FROM {destination}.names")
            )?[0]["n"]
                == "2"
        );
        ensure!(query(mysql, &format!("SELECT {destination}.bump(7) AS n"))?[0]["n"] == "7");
        query(
            mysql,
            &format!(
                "CREATE OR REPLACE VIEW {source}.names AS SELECT id,upper(body) AS body FROM {source}.items"
            ),
        )?;
        let blocked = compare("view", "names", "view-query", "sql")?;
        ensure!(
            !blocked.status.success() && blocked.stdout.is_empty(),
            "Unsupported definition emitted migration SQL"
        );
        Ok(())
    })();
    let cascade = if mysql { "" } else { " CASCADE" };
    let cleanup = query(
        mysql,
        &format!("DROP {namespace} {source}{cascade}; DROP {namespace} {destination}{cascade}"),
    );
    check?;
    cleanup?;
    Ok(())
}

#[test]
#[ignore = "requires disposable Rowboat PostgreSQL Podman fixture"]
fn postgres_review_only_definition_comparison_cli() -> Result<()> {
    definition_comparison(false)
}

#[test]
#[ignore = "requires disposable Rowboat MySQL Podman fixture"]
fn mysql_review_only_definition_comparison_cli() -> Result<()> {
    definition_comparison(true)
}
