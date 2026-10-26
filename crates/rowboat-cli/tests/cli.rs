use std::process::Command;

fn query_json(url: &str) {
    let output = Command::new(env!("CARGO_BIN_EXE_rowboatctl"))
        .env("ROWBOAT_PASSWORD", "rowboat-test")
        .args([
            "--url",
            url,
            "query",
            "--sql",
            "SELECT 42 AS answer, NULL AS missing; SELECT 'NULL' AS text_null;",
            "--format",
            "json",
        ])
        .output()
        .expect("start rowboatctl");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let results: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("multiple results must be one valid JSON document");
    assert_eq!(results[0][0]["answer"], "42");
    assert!(results[0][0]["missing"].is_null());
    assert_eq!(results[1][0]["text_null"], "NULL");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("rowboat-test"));
}

#[test]
#[ignore = "Requires task-owned Podman PostgreSQL"]
fn postgres_cli_multiple_results() {
    query_json("postgres://postgres@127.0.0.1:15435/rowboat?sslmode=disable");
}

#[test]
#[ignore = "Requires task-owned Podman MySQL"]
fn mysql_cli_multiple_results() {
    query_json("mysql://rowboat@127.0.0.1:13309/rowboat?sslmode=disable");
}

#[test]
fn invalid_export_format_is_rejected_before_connecting() {
    let output = Command::new(env!("CARGO_BIN_EXE_rowboatctl"))
        .env_remove("ROWBOAT_URL")
        .args(["query", "--sql", "SELECT 1", "--format", "sql"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("use export --format sql"));
}
