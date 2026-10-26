//! Public CLI against private SQLite files; no discovered profiles or servers.
use serde_json::Value;
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/pass16-explain");
        std::fs::create_dir_all(&root).unwrap();
        let fixture = Self(tempfile::tempdir_in(root).unwrap());
        let output = fixture.command().args([
            "--create", "query", "--sql",
            "CREATE TABLE items(id INTEGER PRIMARY KEY, category TEXT); CREATE INDEX category_index ON items(category); INSERT INTO items VALUES (1, 'first');",
        ]).output().unwrap();
        success(&output);
        fixture
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rowboatctl"));
        for variable in [
            "ROWBOAT_URL",
            "KEEL_URL",
            "ROWBOAT_PASSWORD",
            "KEEL_PASSWORD",
        ] {
            command.env_remove(variable);
        }
        command
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/nonexistent/pass16-explain",
            )
            .env("ROWBOAT_DATA_DIR", self.0.path().join("state"))
            .arg("--database-file")
            .arg(self.0.path().join("fixture.sqlite"));
        command
    }
    fn explain(&self, sql: &str, format: &str) -> Output {
        self.command()
            .args(["explain", sql, "--format", format])
            .output()
            .unwrap()
    }
    fn unchanged(&self) {
        let output = self
            .command()
            .args([
                "query",
                "--sql",
                "SELECT id, category FROM items",
                "--format",
                "json",
            ])
            .output()
            .unwrap();
        success(&output);
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result, serde_json::json!([{"id":"1", "category":"first"}]));
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn sqlite_cli_raw_json_tree_and_read_only_inspection_preserve_native_plan() {
    let fixture = Fixture::new();
    let sql = "SELECT * FROM items WHERE category='first'";
    let raw = fixture.explain(sql, "raw");
    success(&raw);
    let raw: Value = serde_json::from_slice(&raw.stdout).unwrap();
    assert_eq!(raw[0]["columns"].as_array().unwrap().len(), 4);
    assert_eq!(raw[0]["truncated"], false);
    let output = fixture.explain(sql, "json");
    success(&output);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["raw_format"], "sqlite_query_plan");
    assert_eq!(report["mode"], "explain");
    assert_eq!(report["raw_json"], "");
    assert_eq!(report["timing"], serde_json::json!({}));
    let native: Value = serde_json::from_str(report["raw_report"].as_str().unwrap()).unwrap();
    assert_eq!(native[0]["detail"], raw[0]["rows"][0][3]["Text"]);
    assert_eq!(native[0], report["roots"][0]["raw"]);
    assert_eq!(report["roots"][0]["access"], "SEARCH");
    assert_eq!(report["roots"][0]["relation"], Value::Null);
    assert_eq!(report["roots"][0]["estimates"], serde_json::json!({}));
    assert_eq!(report["roots"][0]["actual"], serde_json::json!({}));
    let output = fixture.explain(sql, "tree");
    success(&output);
    let tree = String::from_utf8(output.stdout).unwrap();
    assert!(tree.contains("SQLite query strategy"));
    assert!(tree.contains("no runtime measurements"));
    assert!(tree.contains(native[0]["detail"].as_str().unwrap()));
    let output = fixture
        .command()
        .args(["--read-only", "explain", sql, "--format", "json"])
        .output()
        .unwrap();
    success(&output);
    fixture.unchanged();
}

#[test]
fn sqlite_cli_dml_empty_plans_and_errors_never_execute_source_sql() {
    let fixture = Fixture::new();
    for sql in [
        "INSERT INTO items VALUES (2, 'second')",
        "UPDATE items SET category='changed'",
        "DELETE FROM items",
        "WITH x AS (SELECT 2) INSERT INTO items SELECT 2, 'second' FROM x",
        "SELECT abs(-9223372036854775808)",
    ] {
        for format in ["raw", "json", "tree"] {
            let output = fixture.explain(sql, format);
            success(&output);
            if sql == "DELETE FROM items" && format == "tree" {
                let text = String::from_utf8(output.stdout).unwrap();
                assert!(text.contains("no query-plan rows"));
                assert!(!text.contains("No supported tree decoded"));
            }
            fixture.unchanged();
        }
    }
    for sql in [
        "SELECT 1; DELETE FROM items",
        "SELECT 1; --hidden\r\nDELETE FROM items",
        "SELECT 'ends\\'; DELETE FROM items; --'",
        "SELECT 1 /* unterminated",
    ] {
        let output = fixture.explain(sql, "json");
        assert!(!output.status.success(), "{sql:?}");
        fixture.unchanged();
    }
    let output = fixture
        .command()
        .args(["explain", "SELECT 1", "--analyze"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not executing EXPLAIN ANALYZE"));
    fixture.unchanged();
}

#[test]
fn sqlite_cli_tree_escapes_native_control_characters_without_losing_json_details() {
    let fixture = Fixture::new();
    let sql = "SELECT * FROM items AS \"odd\u{1b}[2J name\"";
    let output = fixture.explain(sql, "json");
    success(&output);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["roots"][0]["operation"]
            .as_str()
            .unwrap()
            .contains('\u{1b}')
    );
    let output = fixture.explain(sql, "tree");
    success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains('\u{1b}'));
    assert!(text.contains("\\u{1b}"));
}
