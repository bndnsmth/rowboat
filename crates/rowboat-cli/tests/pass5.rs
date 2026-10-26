use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

fn command(mysql: bool) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rowboatctl"));
    cmd.env_remove("ROWBOAT_URL")
        .env("ROWBOAT_PASSWORD", "rowboat-test")
        .args([
            "--url",
            if mysql {
                "mysql://root@127.0.0.1:13309/rowboat?sslmode=disable"
            } else {
                "postgres://postgres@127.0.0.1:15435/rowboat?sslmode=disable"
            },
        ]);
    cmd
}
fn invoke(mysql: bool, args: &[&str]) -> Result<String> {
    success(command(mysql).args(args).output()?)
}
fn success(out: Output) -> Result<String> {
    ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
}
fn query(mysql: bool, sql: &str) -> Result<Value> {
    Ok(serde_json::from_str(&invoke(
        mysql,
        &["query", "--sql", sql, "--format", "json"],
    )?)?)
}
fn name() -> String {
    format!(
        "kcli5_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
fn metrics(node: &Value) -> bool {
    node["actual"]
        .as_object()
        .is_some_and(|m| m.contains_key("Actual Rows"))
        || node["children"]
            .as_array()
            .is_some_and(|xs| xs.iter().any(metrics))
}
fn profiling(mysql: bool) -> Result<()> {
    let n = name();
    query(
        mysql,
        &format!("CREATE TABLE {n} (id INTEGER PRIMARY KEY); INSERT INTO {n} VALUES (1),(2),(3)"),
    )?;
    let test = (|| -> Result<()> {
        let sql = format!("SELECT * FROM {n} WHERE id>1");
        let report: Value = serde_json::from_str(&invoke(
            mysql,
            &["explain", &sql, "--analyze", "--format", "json"],
        )?)?;
        ensure!(report["mode"] == "analyze");
        ensure!(
            report["roots"]
                .as_array()
                .is_some_and(|roots| roots.iter().any(metrics)),
            "Missing actual rows: {report}"
        );
        ensure!(report["raw_format"] == if mysql { "tree" } else { "json" });
        ensure!(!report["raw_report"].as_str().unwrap().is_empty());
        let tree = invoke(mysql, &["explain", &sql, "--analyze", "--format", "tree"])?;
        ensure!(tree.contains("Actual query profile") && tree.contains("Actual Rows"));
        let estimate: Value =
            serde_json::from_str(&invoke(mysql, &["explain", &sql, "--format", "json"])?)?;
        ensure!(estimate["mode"] == "explain");
        ensure!(!estimate["roots"].as_array().unwrap().iter().any(metrics));
        for rejected in [
            format!("SELECT * FROM {n}; DELETE FROM {n}"),
            format!("DELETE FROM {n}"),
        ] {
            ensure!(
                !command(mysql)
                    .args(["explain", &rejected, "--analyze"])
                    .output()?
                    .status
                    .success()
            );
        }
        ensure!(query(mysql, &format!("SELECT count(*) AS n FROM {n}"))?[0]["n"] == "3");
        if !mysql {
            query(false, &format!("CREATE SEQUENCE {n}_once"))?;
            let once = invoke(
                false,
                &[
                    "explain",
                    &format!("SELECT nextval('{n}_once')"),
                    "--analyze",
                    "--format",
                    "json",
                ],
            );
            let seen = query(false, &format!("SELECT last_value FROM {n}_once"));
            query(false, &format!("DROP SEQUENCE {n}_once"))?;
            once?;
            ensure!(seen?[0]["last_value"] == "1", "Analyze replayed SQL");
        }
        Ok(())
    })();
    query(mysql, &format!("DROP TABLE {n}"))?;
    test
}

fn completion(mysql: bool) -> Result<()> {
    let n = name();
    let schema = if mysql { "rowboat" } else { "public" };
    query(
        mysql,
        &format!("CREATE TABLE {n} (id INTEGER PRIMARY KEY, native_label VARCHAR(40))"),
    )?;
    let test = (|| -> Result<()> {
        let sql = format!("SELECT c.na FROM {schema}.{n} c");
        let report: Value = serde_json::from_str(&invoke(
            mysql,
            &["complete-sql", &sql, "--caret", "11", "--schema", schema],
        )?)?;
        let items = report["items"].as_array().unwrap();
        ensure!(
            items.iter().any(|i| i["label"]
                .as_str()
                .is_some_and(|s| s.contains("native_label"))),
            "{report}"
        );
        ensure!(
            report["replacement"]["start"] == 9 && report["replacement"]["end"] == 11,
            "{report}"
        );
        let suppressed: Value = serde_json::from_str(&invoke(
            mysql,
            &["complete-sql", "SELECT 'c.na", "--schema", schema],
        )?)?;
        ensure!(suppressed["items"].as_array().unwrap().is_empty());
        let invalid = command(mysql)
            .args(["complete-sql", "SELECT 'é'", "--caret", "9"])
            .output()?;
        ensure!(!invalid.status.success(), "Accepted a caret inside UTF-8");
        Ok(())
    })();
    query(mysql, &format!("DROP TABLE {n}"))?;
    test
}

fn access(mysql: bool) -> Result<()> {
    let n = name();
    let role = format!("{n}_r");
    let schema = if mysql { "rowboat" } else { "public" };
    let identity = if mysql {
        format!("'{role}'@'%'")
    } else {
        role.clone()
    };
    query(
        mysql,
        &format!(
            "CREATE TABLE {n} (id INTEGER PRIMARY KEY); CREATE ROLE {identity}; GRANT SELECT ON {schema}.{n} TO {identity}"
        ),
    )?;
    let test = (|| -> Result<()> {
        let mut args = vec!["access", &role, "--schema", schema, "--table", &n];
        if mysql {
            args.extend(["--host", "%"]);
        }
        let report: Value = serde_json::from_str(&invoke(mysql, &args)?)?;
        ensure!(
            report["rows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["privilege"] == "SELECT" && row["effective"] == "Granted"),
            "{report}"
        );
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("change.json");
        std::fs::write(
            &path,
            serde_json::to_vec(
                &json!({"object":{"Table":{"schema":schema,"name":n}},"privileges":["Select"],"revoke":true,"grant_option":false}),
            )?,
        )?;
        args[0] = "access-review";
        args.extend(["--change", path.to_str().unwrap(), "--format", "sql"]);
        let preview = invoke(mysql, &args)?;
        ensure!(preview.contains("REVOKE SELECT") && preview.contains(&role));
        args[0] = "access";
        args.truncate(if mysql { 8 } else { 6 });
        let after: Value = serde_json::from_str(&invoke(mysql, &args)?)?;
        ensure!(after["rows"] == report["rows"], "Review changed privileges");
        let page: Value = serde_json::from_str(&invoke(mysql, &["principals", "--limit", "1"])?)?;
        ensure!(page["principals"].as_array().unwrap().len() <= 1);
        Ok(())
    })();
    query(mysql, &format!("DROP TABLE {n}; DROP ROLE {identity}"))?;
    test
}

#[test]
fn snippet_expansion_is_offline_and_never_executes_or_changes_state() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.json");
    let state = json!({"snippet_templates":[{"id":"native","name":"Native snippet","trigger":"sel","body":"SELECT ${1:column} FROM ${table:source} WHERE ${1:column} = ${value};${0}"}]});
    let bytes = serde_json::to_vec(&state)?;
    std::fs::write(&path, &bytes)?;
    let call = |values: &[&str]| -> Result<Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_rowboatctl"))
            .env_remove("ROWBOAT_URL")
            .env("ROWBOAT_DATA_DIR", dir.path())
            .args(["snippet-expand", "sel"])
            .args(values)
            .output()?)
    };
    let out: Value = serde_json::from_str(&success(call(&[
        "--value",
        "table=customers",
        "--value",
        "value='literal=1'",
    ])?)?)?;
    ensure!(out["text"] == "SELECT column FROM customers WHERE column = 'literal=1';");
    ensure!(out["tabstops"][0]["ranges"].as_array().unwrap().len() == 2);
    ensure!(
        !call(&["--value", "table=a", "--value", "table=b"])?
            .status
            .success()
    );
    ensure!(!call(&["--value", "unknown=a"])?.status.success());
    ensure!(std::fs::read(path)? == bytes);
    Ok(())
}

fn scoped_comparison(mysql: bool) -> Result<()> {
    let n = name();
    let src = format!("{n}s");
    let dst = format!("{n}d");
    let create = if mysql {
        "CREATE DATABASE"
    } else {
        "CREATE SCHEMA"
    };
    query(mysql, &format!("{create} {src}; {create} {dst}"))?;
    let test = (|| -> Result<()> {
        let scope = if mysql {
            query(
                true,
                &format!(
                    "CREATE TABLE {src}.items (id INTEGER PRIMARY KEY, amount INTEGER, total INTEGER GENERATED ALWAYS AS (amount * 2) STORED); CREATE TABLE {dst}.items (id INTEGER PRIMARY KEY, amount INTEGER, total INTEGER GENERATED ALWAYS AS (amount * 1) STORED); INSERT INTO {dst}.items (id,amount) VALUES (1,7)"
                ),
            )?;
            vec!["--columns-table", "items"]
        } else {
            query(
                false,
                &format!(
                    "CREATE SEQUENCE {src}.counter INCREMENT 3 MAXVALUE 999 CACHE 4 CYCLE; CREATE SEQUENCE {dst}.counter INCREMENT 1 MAXVALUE 999 CACHE 1 NO CYCLE; SELECT setval('{dst}.counter',7,true)"
                ),
            )?;
            vec!["--object-kind", "sequence", "--object-name", "counter"]
        };
        let compare = |format: &str| -> Result<String> {
            let mut cmd = command(mysql);
            cmd.env("ROWBOAT_SOURCE_PASSWORD", "rowboat-test");
            cmd.args([
                "compare-live",
                "--source-url",
                if mysql {
                    "mysql://root@127.0.0.1:13309/rowboat?sslmode=disable"
                } else {
                    "postgres://postgres@127.0.0.1:15435/rowboat?sslmode=disable"
                },
                "--source-schema",
                &src,
                "--destination-schema",
                &dst,
                "--format",
                format,
            ]);
            cmd.args(&scope);
            success(cmd.output()?)
        };
        let sql = compare("sql")?;
        ensure!(sql.contains("ALTER") && sql.contains(&dst));
        if mysql {
            ensure!(
                query(true, &format!("SELECT total FROM {dst}.items"))?[0]["total"] == "7",
                "Preview changed generated data"
            );
        } else {
            ensure!(
                query(false, &format!("SELECT last_value FROM {dst}.counter"))?[0]["last_value"]
                    == "7"
            );
        }
        query(mysql, &sql)?;
        if mysql {
            ensure!(query(true, &format!("SELECT total FROM {dst}.items"))?[0]["total"] == "14");
        } else {
            ensure!(
                query(false, &format!("SELECT last_value FROM {dst}.counter"))?[0]["last_value"]
                    == "7",
                "Attribute migration reset runtime counter"
            );
        }
        let report: Value = serde_json::from_str(&compare("json")?)?;
        if mysql {
            ensure!(
                report["plan"]["statements"].as_array().unwrap().is_empty(),
                "{report}"
            );
        } else {
            ensure!(
                report["differences"].as_array().unwrap().is_empty(),
                "{report}"
            );
        }
        Ok(())
    })();
    query(
        mysql,
        &if mysql {
            format!("DROP DATABASE {src}; DROP DATABASE {dst}")
        } else {
            format!("DROP SCHEMA {src} CASCADE; DROP SCHEMA {dst} CASCADE")
        },
    )?;
    test
}

#[test]
#[ignore = "Task-owned Podman PostgreSQL on 15435"]
fn postgres_scoped_attribute_comparison() -> Result<()> {
    scoped_comparison(false)
}
#[test]
#[ignore = "Task-owned Podman MySQL on 13309"]
fn mysql_scoped_column_comparison() -> Result<()> {
    scoped_comparison(true)
}

#[test]
#[ignore = "Task-owned Podman PostgreSQL on 15435"]
fn postgres_actual_profile() -> Result<()> {
    profiling(false)
}
#[test]
#[ignore = "Task-owned Podman MySQL on 13309"]
fn mysql_actual_profile() -> Result<()> {
    profiling(true)
}
#[test]
#[ignore = "Task-owned Podman PostgreSQL on 15435"]
fn postgres_contextual_completion() -> Result<()> {
    completion(false)
}
#[test]
#[ignore = "Task-owned Podman MySQL on 13309"]
fn mysql_contextual_completion() -> Result<()> {
    completion(true)
}
#[test]
#[ignore = "Task-owned Podman PostgreSQL on 15435"]
fn postgres_access_review() -> Result<()> {
    access(false)
}
#[test]
#[ignore = "Task-owned Podman MySQL on 13309"]
fn mysql_access_review() -> Result<()> {
    access(true)
}
