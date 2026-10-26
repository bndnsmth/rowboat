use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn url(mysql: bool) -> &'static str {
    if mysql {
        "mysql://root@127.0.0.1:13309/rowboat?sslmode=disable"
    } else {
        "postgres://postgres@127.0.0.1:15435/rowboat?sslmode=disable"
    }
}
fn command(mysql: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rowboatctl"));
    command
        .env_remove("ROWBOAT_URL")
        .env("ROWBOAT_PASSWORD", "rowboat-test")
        .args(["--url", url(mysql)]);
    command
}
fn invoke(mysql: bool, args: &[&str]) -> Output {
    command(mysql).args(args).output().unwrap()
}
fn text(output: Output) -> Result<String> {
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}
fn query(mysql: bool, sql: &str) -> Result<Value> {
    Ok(serde_json::from_str(&text(invoke(
        mysql,
        &["query", "--sql", sql, "--format", "json"],
    ))?)?)
}
fn name() -> String {
    format!(
        "kcli4_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn objects_and_filters(mysql: bool) -> Result<()> {
    let name = name();
    let view = format!("{name}_view");
    let schema = if mysql { "rowboat" } else { "public" };
    query(
        mysql,
        &format!(
            "CREATE TABLE {name} (id INTEGER PRIMARY KEY, label VARCHAR(80)); INSERT INTO {name} VALUES (1,'one'),(2,'two'),(3,NULL); CREATE VIEW {view} AS SELECT id,label FROM {name}"
        ),
    )?;
    let directory = tempfile::tempdir()?;
    let filter = directory.path().join("filter.json");
    std::fs::write(
        &filter,
        serde_json::to_vec(
            &json!({"Or":[{"And":[{"Leaf":{"column":"id","op":"Greater","value":{"Text":"1"}}},{"Leaf":{"column":"label","op":"IsNotNull","value":"Null"}}]},{"Leaf":{"column":"id","op":"Eq","value":{"Text":"1"}}}]}),
        )?,
    )?;
    let rows: Value = serde_json::from_str(&text(invoke(
        mysql,
        &[
            "browse",
            &name,
            "--schema",
            schema,
            "--filter",
            filter.to_str().unwrap(),
        ],
    ))?)?;
    ensure!(rows.as_array().unwrap().len() == 2);
    let definition: Value = serde_json::from_str(&text(invoke(
        mysql,
        &["object-definition", "view", &view, "--schema", schema],
    ))?)?;
    ensure!(definition.to_string().contains(&name));
    let patch = directory.path().join("patch.json");
    std::fs::write(
        &patch,
        serde_json::to_vec(
            &json!({"ViewQuery":{"query":format!("SELECT id,label FROM {name} WHERE id > 1")}}),
        )?,
    )?;
    let sql = text(invoke(
        mysql,
        &[
            "object-review",
            "view",
            &view,
            "--schema",
            schema,
            "--patch",
            patch.to_str().unwrap(),
            "--format",
            "sql",
        ],
    ))?;
    ensure!(
        query(mysql, &format!("SELECT count(*) AS n FROM {view}"))?[0]["n"] == "3",
        "Review executed DDL"
    );
    query(mysql, &sql)?;
    ensure!(query(mysql, &format!("SELECT count(*) AS n FROM {view}"))?[0]["n"] == "2");
    let report: Value = serde_json::from_str(&text(invoke(
        mysql,
        &[
            "explain",
            &format!("SELECT * FROM {name} WHERE id=2"),
            "--format",
            "json",
        ],
    ))?)?;
    ensure!(!report["roots"].as_array().unwrap().is_empty());
    ensure!(report["raw_json"].as_str().is_some());
    let tree = text(invoke(
        mysql,
        &[
            "explain",
            &format!("SELECT * FROM {name} WHERE id=2"),
            "--format",
            "tree",
        ],
    ))?;
    ensure!(tree.contains("Estimated query plan") && tree.contains(&name));
    let attack = invoke(
        mysql,
        &[
            "explain",
            &format!("SELECT * FROM {name}; DELETE FROM {name}"),
            "--format",
            "tree",
        ],
    );
    ensure!(!attack.status.success());
    ensure!(query(mysql, &format!("SELECT count(*) AS n FROM {name}"))?[0]["n"] == "3");
    query(mysql, &format!("DROP VIEW {view}; DROP TABLE {name}"))?;
    Ok(())
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn progressive(mysql: bool) -> Result<()> {
    let sql = if mysql {
        "WITH RECURSIVE n AS (SELECT 1 AS id UNION ALL SELECT id+1 FROM n WHERE id<300) SELECT id,REPEAT('x',4096) AS payload,SLEEP(0.01) FROM n"
    } else {
        "SELECT id,repeat('x',4096) AS payload,pg_sleep(0.01) FROM generate_series(1,300) id"
    };
    let mut child = Process(
        command(mysql)
            .args([
                "query-stream",
                "--sql",
                sql,
                "--batch-rows",
                "1",
                "--unlimited",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let stdout = child.0.stdout.take().unwrap();
    let (send, recv) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut rows = 0usize;
        let mut complete = false;
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            let event: Value = serde_json::from_str(&line).unwrap();
            if event["event"] == "rows" {
                if rows == 0 {
                    let _ = send.send(());
                }
                rows += event["rows"].as_array().unwrap().len();
            }
            if event["event"] == "complete" {
                complete = true;
            }
        }
        (rows, complete)
    });
    recv.recv_timeout(Duration::from_secs(5))?;
    ensure!(
        child.0.try_wait()?.is_none(),
        "All rows were buffered until process completion"
    );
    ensure!(child.0.wait()?.success());
    let (rows, complete) = reader.join().unwrap();
    ensure!(rows == 300 && complete);
    let error = invoke(
        mysql,
        &[
            "query-stream",
            "--sql",
            "SELECT 1; SELECT * FROM rowboat_missing_stream_target",
            "--batch-rows",
            "1",
        ],
    );
    ensure!(!error.status.success());
    let events = String::from_utf8(error.stdout)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(events.iter().any(|e| e["event"] == "rows"));
    ensure!(events.last().unwrap()["event"] == "error");
    ensure!(!events.iter().any(|e| e["event"] == "complete"));

    let slow = if mysql {
        "WITH RECURSIVE n AS (SELECT 1 AS id UNION ALL SELECT id+1 FROM n WHERE id<300) SELECT id,REPEAT('x',4096),SLEEP(0.1) FROM n"
    } else {
        "SELECT id,repeat('x',4096),pg_sleep(0.1) FROM generate_series(1,300) id"
    };
    let mut cancelled = Process(
        command(mysql)
            .args(["query-stream", "--sql", slow, "--batch-rows", "1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let stdout = cancelled.0.stdout.take().unwrap();
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut events = vec![];
        for line in BufReader::new(stdout).lines() {
            let event: Value = serde_json::from_str(&line.unwrap()).unwrap();
            if event["event"] == "rows" {
                let _ = send.send(());
            }
            events.push(event);
        }
        events
    });
    receive.recv_timeout(Duration::from_secs(5))?;
    // Signal only the task-owned CLI after observing its active query's first row.
    ensure!(unsafe { libc::kill(cancelled.0.id() as i32, libc::SIGINT) } == 0);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = cancelled.0.try_wait()? {
            ensure!(!status.success());
            break;
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "CLI cancellation did not complete promptly"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let events = reader.join().unwrap();
    ensure!(
        events
            .last()
            .is_some_and(|event| event["event"] == "error" && event["cancelled"] == true)
    );
    ensure!(!events.iter().any(|event| event["event"] == "complete"));
    Ok(())
}

fn transfers(mysql: bool) -> Result<()> {
    let table = name();
    let bytes = if mysql { "VARBINARY(40)" } else { "BYTEA" };
    query(
        mysql,
        &format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, label TEXT, bytes {bytes})"),
    )?;
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("large.csv");
    let mut csv = String::from("id,label,bytes\n");
    for id in 0..1500 {
        csv.push_str(&format!("{id},row {id} 日本語,\\x00ff\n"));
    }
    std::fs::write(&source, &csv)?;
    let args = [
        "import-stream",
        &table,
        source.to_str().unwrap(),
        "--format",
        "csv",
    ];
    let preview: Value = serde_json::from_str(&text(invoke(mysql, &args))?)?;
    ensure!(
        preview["rows"] == 1500 && preview["sample_only"] == true && preview["applied"] == false
    );
    ensure!(preview["sample"].as_array().unwrap().len() <= 50);
    ensure!(query(mysql, &format!("SELECT count(*) AS n FROM {table}"))?[0]["n"] == "0");
    // The malformed final row arrives well after multiple insertion batches.
    std::fs::write(&source, format!("{csv}0,duplicate,\\N\n"))?;
    let mut apply = args.to_vec();
    apply.push("--apply");
    let failed = invoke(mysql, &apply);
    ensure!(!failed.status.success());
    ensure!(
        query(mysql, &format!("SELECT count(*) AS n FROM {table}"))?[0]["n"] == "0",
        "Late import failure left earlier chunks committed"
    );
    std::fs::write(&source, &csv)?;
    text(invoke(mysql, &apply))?;
    ensure!(query(mysql, &format!("SELECT count(*) AS n FROM {table}"))?[0]["n"] == "1500");
    let exported = directory.path().join("rows.jsonl");
    text(invoke(
        mysql,
        &[
            "export-stream",
            &table,
            "--format",
            "jsonl",
            "--output",
            exported.to_str().unwrap(),
        ],
    ))?;
    let exported_bytes = std::fs::read(&exported)?;
    let exported_text = String::from_utf8(exported_bytes.clone())?;
    let rows = exported_text
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(rows.len() == 1500);
    ensure!(rows.iter().all(|row| row["bytes"]["$binary"] == "00ff"));
    ensure!(rows.iter().any(|row| row["label"] == "row 1499 日本語"));
    ensure!(
        !invoke(
            mysql,
            &[
                "export-stream",
                &table,
                "--format",
                "jsonl",
                "--output",
                exported.to_str().unwrap()
            ]
        )
        .status
        .success()
    );
    ensure!(
        std::fs::read(&exported)? == exported_bytes,
        "Existing export was overwritten"
    );
    query(mysql, &format!("DROP TABLE {table}"))?;
    Ok(())
}

#[test]
#[ignore = "Requires disposable Podman PostgreSQL 15435"]
fn postgres_cli_objects_filters_and_plans() -> Result<()> {
    objects_and_filters(false)
}
#[test]
#[ignore = "Requires disposable Podman MySQL 13309"]
fn mysql_cli_objects_filters_and_plans() -> Result<()> {
    objects_and_filters(true)
}
#[test]
#[ignore = "Requires disposable Podman PostgreSQL 15435"]
fn postgres_cli_progressive_events_and_partial_failure() -> Result<()> {
    progressive(false)
}
#[test]
#[ignore = "Requires disposable Podman MySQL 13309"]
fn mysql_cli_progressive_events_and_partial_failure() -> Result<()> {
    progressive(true)
}

#[test]
#[ignore = "Requires disposable Podman PostgreSQL 15435"]
fn postgres_cli_bounded_transfer_is_atomic() -> Result<()> {
    transfers(false)
}
#[test]
#[ignore = "Requires disposable Podman MySQL 13309"]
fn mysql_cli_bounded_transfer_is_atomic() -> Result<()> {
    transfers(true)
}
