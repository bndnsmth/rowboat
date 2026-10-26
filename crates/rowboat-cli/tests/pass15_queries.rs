//! Actual CLI processes, private SQLite files and explicitly opted-in task servers.
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read},
    net::TcpListener,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

const LONG: &str = "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n";
const MANY: &str = "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT x, printf('%01000d',x) FROM n";

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rowboatctl"));
    for variable in [
        "ROWBOAT_URL",
        "KEEL_URL",
        "ROWBOAT_PASSWORD",
        "KEEL_PASSWORD",
        "ROWBOAT_SSH_KNOWN_HOSTS",
        "KEEL_SSH_KNOWN_HOSTS",
    ] {
        command.env_remove(variable);
    }
    command.env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/pass15");
    command
}

struct Fixture {
    directory: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        let output = fixture
            .command()
            .args([
                "--create",
                "query",
                "--sql",
                "CREATE TABLE witness(id INTEGER PRIMARY KEY)",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        fixture
    }
    fn command(&self) -> Command {
        let mut cmd = command();
        cmd.env("ROWBOAT_DATA_DIR", self.directory.path().join("state"))
            .arg("--database-file")
            .arg(self.directory.path().join("fixture.sqlite"));
        cmd
    }
    fn query(&self, sql: &str, extra: &[&str]) -> Output {
        self.command()
            .args(["query", "--format", "json-envelope", "--sql", sql])
            .args(extra)
            .output()
            .unwrap()
    }
    fn count(&self) -> Value {
        envelope(&self.query("SELECT count(*) FROM witness", &[]))["results"][0]["rows"][0][0]
            .clone()
    }
}
fn envelope(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{error}: {output:?}"))
}
struct Process(Child);
impl Process {
    fn spawn(command: &mut Command) -> Self {
        Self(
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    }
    fn signal(&self, signal: i32) {
        assert_eq!(unsafe { libc::kill(self.0.id() as i32, signal) }, 0);
    }
    fn wait(&mut self) -> Output {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                start.elapsed() < Duration::from_secs(6),
                "CLI did not exit within bounded cleanup"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(mut pipe) = self.0.stdout.take() {
            pipe.read_to_end(&mut stdout).unwrap();
        }
        if let Some(mut pipe) = self.0.stderr.take() {
            pipe.read_to_end(&mut stderr).unwrap();
        }
        Output {
            status,
            stdout,
            stderr,
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn envelope_preserves_aliases_precision_binary_null_and_result_metadata() {
    let fixture = Fixture::new();
    let output = fixture.query("SELECT 9223372036854775807 AS same, 'second' AS same, x'00ff80' AS blob, NULL AS absent; SELECT 1 WHERE 0; INSERT INTO witness VALUES(1),(2); SELECT id FROM witness ORDER BY id", &["--limit", "1"]);
    assert!(output.status.success(), "{output:?}");
    let value = envelope(&output);
    assert_eq!(value["version"], 1);
    assert_eq!(value["ok"], true);
    assert_eq!(value["status"], "complete");
    assert_eq!(value["error"], Value::Null);
    let results = value["results"].as_array().unwrap();
    assert_eq!(results.len(), 4);
    assert_eq!(results[0]["columns"][0]["name"], "same");
    assert_eq!(results[0]["columns"][1]["name"], "same");
    assert_eq!(
        results[0]["rows"],
        json!([["9223372036854775807", "second", {"$binary":"00ff80"}, null]])
    );
    assert_eq!(results[1]["rows_returned"], 0);
    assert_eq!(results[2]["rows_affected"], 2);
    assert_eq!(results[3]["rows_seen"], 2);
    assert_eq!(results[3]["rows_returned"], 1);
    assert_eq!(results[3]["truncated"], true);
    for (index, result) in results.iter().enumerate() {
        assert_eq!(result["index"], index);
        assert_eq!(result["complete"], true);
        assert!(result["elapsed_ms"].is_u64());
    }
}

#[test]
fn late_error_retains_completed_write_without_replaying_or_claiming_rollback() {
    let fixture = Fixture::new();
    let output = fixture.query(
        "INSERT INTO witness VALUES(1); SELECT * FROM definitely_missing_pass15",
        &[],
    );
    assert_eq!(output.status.code(), Some(1));
    let value = envelope(&output);
    assert_eq!(value["status"], "error");
    assert_eq!(value["error"]["code"], "query_error");
    assert_eq!(value["results"][0]["complete"], true);
    assert_eq!(value["results"][0]["rows_affected"], 1);
    assert_eq!(fixture.count(), "1");
}

#[test]
fn empty_sql_limit_zero_and_transaction_result_end_are_honest() {
    let fixture = Fixture::new();
    let empty = fixture.query(" ", &[]);
    assert!(empty.status.success());
    assert_eq!(envelope(&empty)["results"], json!([]));
    let output = fixture.query("SELECT 1 UNION ALL SELECT 2", &["--limit", "0"]);
    let value = envelope(&output);
    assert_eq!(value["results"][0]["rows"], json!([]));
    assert_eq!(value["results"][0]["rows_seen"], 2);
    assert_eq!(value["results"][0]["truncated"], true);
    let transaction = fixture.query("BEGIN; INSERT INTO witness VALUES(1)", &[]);
    assert!(transaction.status.success());
    assert_eq!(envelope(&transaction)["results"][1]["complete"], true);
    assert_eq!(fixture.count(), "0", "ResultEnd must not imply COMMIT");
}

#[test]
fn legacy_json_shape_and_query_only_envelope_are_preserved() {
    let fixture = Fixture::new();
    for (sql, expected) in [
        ("SELECT 42 AS n", json!([{"n":"42"}])),
        (
            "SELECT 1 AS a; SELECT NULL AS b",
            json!([[{"a":"1"}],[{"b":null}]]),
        ),
    ] {
        let output = fixture
            .command()
            .args(["query", "--sql", sql, "--format", "json"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(envelope(&output), expected);
    }
    let output = fixture
        .command()
        .args(["browse", "witness", "--format", "json-envelope"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn input_and_connection_failures_have_credential_free_envelopes() {
    for args in [
        vec!["query", "--sql", "SELECT 1", "--format", "json-envelope"],
        vec![
            "--url",
            "postgres://user:do-not-leak-pass15@127.0.0.1:1/db",
            "query",
            "--sql",
            "SELECT 1",
            "--format",
            "json-envelope",
        ],
        vec![
            "--url",
            "redis://user:do-not-leak-pass15@localhost/db",
            "query",
            "--sql",
            "SELECT 1",
            "--format",
            "json-envelope",
        ],
    ] {
        let output = command().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(envelope(&output)["error"]["code"], "connection_error");
        assert!(
            !format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .contains("do-not-leak-pass15")
        );
    }
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args([
            "query",
            "--file",
            "/nonexistent/pass15.sql",
            "--format",
            "json-envelope",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(envelope(&output)["error"]["code"], "input_error");
}

#[test]
fn duration_validation_and_default_unlimited() {
    let fixture = Fixture::new();
    for duration in [
        "-1s",
        "1.5s",
        "1",
        "18446744073709551616ms",
        "18446744073709551615m",
        "100000m",
        "+1s",
    ] {
        let output = fixture.query("SELECT 1", &["--timeout", duration]);
        assert_eq!(output.status.code(), Some(2), "{duration}: {output:?}");
    }
    for duration in ["0", "0ms", "10s", "1m", "500ms"] {
        assert!(
            fixture
                .query("SELECT 1", &["--timeout", duration])
                .status
                .success()
        );
    }
    assert!(fixture.query("SELECT 1", &[]).status.success());
}

#[test]
fn sqlite_timeout_retains_prior_write_and_releases_lock_for_next_query() {
    let fixture = Fixture::new();
    for id in 1..=3 {
        let sql = format!("INSERT INTO witness VALUES({id}); {LONG}");
        let mut process = Process::spawn(fixture.command().args([
            "query",
            "--sql",
            &sql,
            "--format",
            "json-envelope",
            "--timeout",
            "100ms",
        ]));
        let output = process.wait();
        assert_eq!(output.status.code(), Some(124), "{output:?}");
        let value = envelope(&output);
        assert_eq!(value["status"], "timeout");
        assert_eq!(value["error"]["code"], "timeout");
        assert_eq!(value["results"][0]["complete"], true);
        assert_eq!(value["results"][0]["rows_affected"], 1);
        assert!(
            value["results"]
                .as_array()
                .unwrap()
                .iter()
                .skip(1)
                .all(|result| result["complete"] == false)
        );
        assert_eq!(fixture.count(), id.to_string());
    }
}

#[test]
fn sqlite_sigint_sigterm_cancel_both_query_modes_and_allow_followup() {
    let fixture = Fixture::new();
    for streaming in [false, true] {
        for (signal, code) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
            let mut command = fixture.command();
            command.args([
                if streaming { "query-stream" } else { "query" },
                "--sql",
                LONG,
            ]);
            if !streaming {
                command.args(["--format", "json-envelope"]);
            }
            let mut process = Process::spawn(&mut command);
            std::thread::sleep(Duration::from_millis(200));
            process.signal(signal);
            let output = process.wait();
            assert_eq!(output.status.code(), Some(code), "{output:?}");
            let value = if streaming {
                String::from_utf8(output.stdout.clone())
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .next_back()
                    .unwrap()
            } else {
                envelope(&output)
            };
            assert_eq!(
                if streaming {
                    &value["code"]
                } else {
                    &value["error"]["code"]
                },
                "cancelled"
            );
            assert_eq!(fixture.count(), "0");
        }
    }
}

#[test]
fn partial_rows_survive_error_and_timeout_without_implied_result_end() {
    let fixture = Fixture::new();
    let sql = "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000) SELECT CASE WHEN x=700 THEN abs(-9223372036854775808) ELSE x END FROM n";
    let output = fixture.query(sql, &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let value = envelope(&output);
    assert_eq!(value["results"][0]["complete"], false);
    for field in ["rows_seen", "rows_affected", "elapsed_ms", "truncated"] {
        assert_eq!(value["results"][0][field], Value::Null);
    }
    assert!(value["results"][0]["rows_returned"].as_u64().unwrap() >= 256);
    let output = fixture.query(MANY, &["--timeout", "100ms", "--limit", "3"]);
    assert_eq!(output.status.code(), Some(124));
    assert_eq!(envelope(&output)["results"][0]["complete"], false);
}

#[test]
fn aggregate_row_budget_is_preserved_across_result_boundaries() {
    let fixture = Fixture::new();
    // Each result fits the cell bound, but the combined emitted payload reaches
    // the core's global 64 MiB limit. Later results must not reset that budget.
    let sql = "SELECT printf('%012000000d',1);".repeat(6);
    let output = fixture.query(&sql, &["--timeout", "10s"]);
    assert_eq!(output.status.code(), Some(0));
    let value = envelope(&output);
    assert_eq!(value["error"], Value::Null);
    assert_eq!(value["results"][0]["complete"], true);
    let returned: usize = value["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["rows_returned"].as_u64().unwrap() as usize)
        .sum();
    assert_eq!(returned, 5);
    assert_eq!(value["results"][5]["truncated"], true);
    assert_eq!(value["results"][5]["rows_seen"], 1);
    assert_eq!(fixture.count(), "0");
}

#[test]
fn stalled_stdout_deadline_and_signal_exit_without_blocking_writer_shutdown() {
    let fixture = Fixture::new();
    for signal in [None, Some(libc::SIGINT), Some(libc::SIGTERM)] {
        let mut process = Process::spawn(fixture.command().args([
            "query-stream",
            "--sql",
            MANY,
            "--unlimited",
            "--timeout",
            if signal.is_some() { "0" } else { "100ms" },
        ]));
        // Leave stdout unread: enough data fills the actual OS pipe.
        if let Some(signal) = signal {
            std::thread::sleep(Duration::from_millis(200));
            process.signal(signal);
        }
        let output = process.wait();
        assert_eq!(
            output.status.code(),
            Some(match signal {
                Some(libc::SIGINT) => 130,
                Some(_) => 143,
                None => 124,
            })
        );
        assert_eq!(fixture.count(), "0");
    }
}

#[test]
fn buffered_json_output_backpressure_keeps_deadline_and_no_second_document() {
    let fixture = Fixture::new();
    for format in ["json", "json-envelope"] {
        let mut process = Process::spawn(fixture.command().args([
            "query",
            "--sql",
            "SELECT printf('%01000000d',1) AS large",
            "--format",
            format,
            "--timeout",
            "300ms",
        ]));
        let output = process.wait();
        assert_eq!(output.status.code(), Some(124), "{output:?}");
        assert!(!output.stdout.is_empty());
        assert!(
            serde_json::from_slice::<Value>(&output.stdout).is_err(),
            "A stalled pipe must not fabricate a complete document"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("Query deadline exceeded"),
            "Never append an error object to an incomplete document"
        );
    }
}

#[test]
fn broken_pipe_cancels_even_while_waiting_for_first_row() {
    let fixture = Fixture::new();
    for sql in [LONG, MANY] {
        let mut process =
            Process::spawn(
                fixture
                    .command()
                    .args(["query-stream", "--sql", sql, "--unlimited"]),
            );
        std::thread::sleep(Duration::from_millis(100));
        drop(process.0.stdout.take());
        assert_eq!(process.wait().status.code(), Some(1));
        assert_eq!(fixture.count(), "0");
    }
}

#[test]
fn stalled_handshake_timeout_and_signals_cover_setup() {
    for (signal, code) in [
        (None, 124),
        (Some(libc::SIGINT), 130),
        (Some(libc::SIGTERM), 143),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "postgres://user:handshake-secret@{}/db?sslmode=disable",
            listener.local_addr().unwrap()
        );
        let mut process = Process::spawn(command().args([
            "--url",
            &url,
            "query",
            "--sql",
            "SELECT 1",
            "--format",
            "json-envelope",
            "--connect-timeout",
            if signal.is_some() { "0" } else { "100ms" },
        ]));
        let (_socket, _) = listener.accept().unwrap();
        if let Some(signal) = signal {
            process.signal(signal);
        }
        let output = process.wait();
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        assert_eq!(envelope(&output)["results"], json!([]));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("handshake-secret"));
    }
}

#[test]
fn blocked_input_file_is_bounded_including_runtime_shutdown() {
    use std::os::unix::ffi::OsStrExt;
    let fixture = Fixture::new();
    let path = fixture.directory.path().join("input.fifo");
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    let mut process = Process::spawn(
        fixture
            .command()
            .args([
                "query",
                "--format",
                "json-envelope",
                "--connect-timeout",
                "100ms",
                "--file",
            ])
            .arg(path),
    );
    let output = process.wait();
    assert_eq!(output.status.code(), Some(124));
    assert_eq!(envelope(&output)["status"], "timeout");
}

#[cfg(target_os = "linux")]
struct KeyringFixture {
    directory: tempfile::TempDir,
}
#[cfg(target_os = "linux")]
impl KeyringFixture {
    fn new(url: &str, script: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        let helper = fixture.directory.path().join("secret-tool");
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let saved = fixture
            .command()
            .env("ROWBOAT_URL", url)
            .args([
                "profiles",
                "add-url",
                "--name",
                "Pass15 private keyring fixture",
            ])
            .output()
            .unwrap();
        assert!(saved.status.success(), "{saved:?}");
        fixture
    }
    fn command(&self) -> Command {
        let mut command = command();
        command
            .env("PATH", self.directory.path())
            .env("HOME", self.directory.path())
            .env("ROWBOAT_DATA_DIR", self.directory.path().join("state"))
            .env(
                "PASS15_HELPER_PID",
                self.directory.path().join("helper.pid"),
            );
        command
    }
    fn query(&self) -> Command {
        let mut command = self.command();
        command.args([
            "--connection",
            "Pass15 private keyring fixture",
            "query",
            "--sql",
            "SELECT 1",
            "--format",
            "json-envelope",
        ]);
        command
    }
}

#[test]
#[cfg(target_os = "linux")]
fn setup_timeout_and_signals_explicitly_reap_keyring_helper() {
    for (signal, code) in [
        (None, 124),
        (Some(libc::SIGINT), 130),
        (Some(libc::SIGTERM), 143),
    ] {
        let fixture = KeyringFixture::new(
            "postgres://unused@127.0.0.1:1/unused?sslmode=disable",
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$PASS15_HELPER_PID\"\nexec /usr/bin/sleep 30\n",
        );
        let mut process = Process::spawn(fixture.query().args([
            "--connect-timeout",
            if signal.is_some() { "0" } else { "300ms" },
        ]));
        let started = Instant::now();
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(fixture.directory.path().join("helper.pid"))
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                break pid;
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "helper did not start"
            );
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "CLI exited before helper started"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        if let Some(signal) = signal {
            process.signal(signal);
        }
        let output = process.wait();
        let gone = !std::path::Path::new(&format!("/proc/{pid}")).exists();
        // Test failure must not leave its own sleeper behind.
        if !gone {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        assert!(
            gone,
            "keyring helper PID {pid} still exists (including a zombie) after CLI exit"
        );
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        let value = envelope(&output);
        assert_eq!(
            value["status"],
            if signal.is_some() {
                "cancelled"
            } else {
                "timeout"
            }
        );
        assert_eq!(value["results"], json!([]));
    }
}

#[test]
#[cfg(target_os = "linux")]
fn async_keyring_lookup_preserves_unicode_and_trailing_newlines_exactly() {
    use std::io::Write;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "postgres://fixture@{}/unused?sslmode=disable",
        listener.local_addr().unwrap()
    );
    let fixture = KeyringFixture::new(&url, "#!/bin/sh\nprintf 'pass15-秘密\\n\\n'\n");
    let mut process = Process::spawn(fixture.query().args(["--connect-timeout", "3s"]));
    let (mut socket, _) = listener.accept().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    // A private protocol stub requests cleartext auth only to inspect the exact
    // bytes passed by the async lookup; it is not a user or live database.
    let mut length = [0; 4];
    socket.read_exact(&mut length).unwrap();
    let length = u32::from_be_bytes(length) as usize;
    assert!((8..4096).contains(&length));
    let mut startup = vec![0; length - 4];
    socket.read_exact(&mut startup).unwrap();
    socket.write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 3]).unwrap();
    let mut header = [0; 5];
    socket.read_exact(&mut header).unwrap();
    assert_eq!(header[0], b'p');
    let length = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    assert!((5..8192).contains(&length));
    let mut password = vec![0; length - 4];
    socket.read_exact(&mut password).unwrap();
    assert_eq!(password, "pass15-秘密\n\n\0".as_bytes());
    drop(socket);
    let output = process.wait();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(envelope(&output)["error"]["code"], "connection_error");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("秘密"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("秘密"));
}

#[test]
fn streaming_late_cancel_after_result_end_keeps_original_tags_and_no_complete() {
    let fixture = Fixture::new();
    let sql = format!("INSERT INTO witness VALUES(1); {LONG}");
    let mut process = Process::spawn(fixture.command().args(["query-stream", "--sql", &sql]));
    let mut reader = BufReader::new(process.0.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["event"] == "result_end" {
            assert_eq!(event["rows_affected"], 1);
            break;
        }
    }
    process.signal(libc::SIGTERM);
    let output = process.wait();
    assert_eq!(output.status.code(), Some(143));
    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    let events = rest
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(events.iter().all(|event| event["event"] != "complete"));
    assert_eq!(events.last().unwrap()["code"], "cancelled");
    assert_eq!(fixture.count(), "1");
}

fn network(mysql: bool) -> Command {
    let mut cmd = command();
    cmd.env("ROWBOAT_PASSWORD", "rowboat-test").args([
        "--url",
        if mysql {
            "mysql://rowboat@127.0.0.1:13309/rowboat?sslmode=disable"
        } else {
            "postgres://postgres@127.0.0.1:15435/rowboat?sslmode=disable"
        },
    ]);
    cmd
}

#[test]
#[ignore = "explicit task-owned PostgreSQL15435 and MySQL13309 fixtures"]
fn network_deadlines_cancel_only_owned_query_and_preserve_prior_results() {
    for mysql in [false, true] {
        let sql = if mysql {
            "SELECT 42 AS n; SELECT SLEEP(30)"
        } else {
            "SELECT 42 AS n; SELECT repeat('x',10000); SELECT pg_sleep(30)"
        };
        let mut process = Process::spawn(network(mysql).args([
            "query",
            "--sql",
            sql,
            "--format",
            "json-envelope",
            "--timeout",
            "200ms",
        ]));
        let output = process.wait();
        assert_eq!(output.status.code(), Some(124), "{output:?}");
        let value = envelope(&output);
        assert_eq!(value["results"][0]["rows"][0][0], "42");
        assert_eq!(value["results"][0]["complete"], true);
        let followup = network(mysql)
            .args(["query", "--sql", "SELECT 1", "--format", "json-envelope"])
            .output()
            .unwrap();
        assert!(followup.status.success(), "{followup:?}");
    }
}

#[test]
#[ignore = "explicit task-owned PostgreSQL15435 and MySQL13309 fixtures"]
fn network_signals_do_not_cancel_an_independent_peer() {
    for mysql in [false, true] {
        for (signal, code) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
            let mut peer = Process::spawn(network(mysql).args([
                "query",
                "--sql",
                if mysql {
                    "SELECT SLEEP(0.5)"
                } else {
                    "SELECT pg_sleep(0.5)"
                },
                "--format",
                "json-envelope",
                "--timeout",
                "3s",
            ]));
            let mut owned = Process::spawn(network(mysql).args([
                "query-stream",
                "--sql",
                if mysql {
                    "SELECT SLEEP(30)"
                } else {
                    "SELECT pg_sleep(30)"
                },
            ]));
            std::thread::sleep(Duration::from_millis(150));
            owned.signal(signal);
            let output = owned.wait();
            assert_eq!(output.status.code(), Some(code), "{output:?}");
            let output = peer.wait();
            assert!(output.status.success(), "{output:?}");
            assert_eq!(envelope(&output)["status"], "complete");
        }
    }
}
