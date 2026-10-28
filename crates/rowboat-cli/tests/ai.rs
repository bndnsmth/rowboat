use std::{os::unix::fs::PermissionsExt, process::Command};

#[tokio::test]
async fn trusted_assistant_fake_agents_investigate_only_an_explicit_disposable_target() {
    use rowboat_core::{ConnectionProfile, Engine, SqliteFileOptions, SqliteOpenMode};
    use rowboat_services::ai::{self, AgentControl, AgentProvider, AgentUpdate, AiPreferences};
    // Keep a disposable file beneath the test cwd so a genuinely relative path
    // differs from the agent's private cwd. Never change the process-wide cwd.
    let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let file = root.path().join("trusted-fixture.sqlite");
    let output = command(root.path())
        .arg("--database-file")
        .arg(&file)
        .args([
            "--create",
            "query",
            "--sql",
            "CREATE TABLE items(id INTEGER PRIMARY KEY); INSERT INTO items VALUES(7)",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let profile = ConnectionProfile {
        id: "trusted-fixture".into(),
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: file,
            mode: SqliteOpenMode::OpenExisting,
        }),
        ..Default::default()
    };
    for (provider, mode, relative) in AgentProvider::ALL.into_iter().flat_map(|provider| {
        [
            (SqliteOpenMode::OpenExisting, false),
            (SqliteOpenMode::CreateIfMissing, false),
            (SqliteOpenMode::OpenExisting, true),
            (SqliteOpenMode::CreateIfMissing, true),
        ]
        .map(move |(mode, relative)| (provider, mode, relative))
    }) {
        let mut target = profile.clone();
        let file = target.file.as_mut().unwrap();
        file.mode = mode;
        if relative {
            file.path = file
                .path
                .strip_prefix(std::env::current_dir().unwrap())
                .unwrap()
                .to_path_buf();
        }
        let agent = root.path().join(format!("fake-{}", provider.executable()));
        let capture = root.path().join("prompt.txt");
        let query_result = root.path().join("result.json");
        let cwd = root.path().join("cwd.txt");
        let (tool, response) = match provider {
            AgentProvider::Codex => (
                r#"{"type":"item.started","item":{"type":"command_execution","command":"not-rendered"}}"#,
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"Fixture investigated; suggested SQL remains unexecuted."}}"#,
            ),
            AgentProvider::Claude => (
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Fixture investigated; suggested SQL remains unexecuted."}]}}"#,
            ),
            AgentProvider::Opencode => (
                r#"{"type":"tool_use","part":{"type":"tool"}}"#,
                r#"{"type":"text","part":{"text":"Fixture investigated; suggested SQL remains unexecuted."}}"#,
            ),
        };
        std::fs::write(&agent, format!(r##"#!/bin/sh
case " $* " in
 *" --version "*) echo '2.1.312 (Claude Code)'; exit 0 ;;
 *" --help "*) echo '--ignore-user-config --ignore-rules --ephemeral --sandbox --json --standalone'; exit 0 ;;
esac
if test -f context.txt; then cat context.txt > '{}'; else cat > '{}'; fi
pwd > '{}'
env ROWBOAT_DATA_DIR="$PWD/connection" '{}' --connection trusted-fixture --read-only query --sql 'SELECT id FROM items LIMIT 1' --format json-envelope --limit 1 --timeout 5s > '{}' || exit 3
if env ROWBOAT_DATA_DIR="$PWD/connection" '{}' --connection trusted-fixture --read-only query --sql 'CREATE TABLE forbidden_write(id INTEGER)' >/dev/null 2>/dev/null; then exit 4; fi
printf '%s\n' '{}' '{}'
"##, capture.display(), capture.display(), cwd.display(), env!("CARGO_BIN_EXE_rowboatctl"), query_result.display(), env!("CARGO_BIN_EXE_rowboatctl"), tool, response)).unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut settings = AiPreferences {
            enabled: true,
            provider,
            executable: agent.to_string_lossy().to_string(),
            ..Default::default()
        };
        settings
            .investigation_connections
            .insert(profile.id.clone());
        let mut events = 0;
        let response = ai::assist(
            &settings,
            "Explicit disposable context",
            Some(target),
            &AgentControl::default(),
            |update| {
                if let AgentUpdate::Tool(_) = update {
                    events += 1;
                }
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{provider:?}, {mode:?}, relative={relative}: {error}"));
        assert_eq!(events, 1);
        assert!(response.starts_with("Fixture investigated"));
        let prompt = std::fs::read_to_string(&capture).unwrap();
        assert!(
            prompt.contains("skills get core --full")
                && prompt.contains("--read-only")
                && prompt.contains("NEW connection")
        );
        assert!(
            !prompt.contains(root.path().to_str().unwrap())
                || !prompt.contains("trusted-fixture.sqlite"),
            "The target's file path belongs in the private profile, not the model prompt"
        );
        let result: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&query_result).unwrap()).unwrap();
        assert_eq!(result["results"][0]["rows"][0][0], "7");
        assert!(!std::path::Path::new(std::fs::read_to_string(&cwd).unwrap().trim()).exists());
    }
}
fn command(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rowboatctl"));
    for key in ["ROWBOAT_URL", "KEEL_URL", "KEEL_DATA_DIR"] {
        command.env_remove(key);
    }
    command.env("ROWBOAT_DATA_DIR", root.join("state"));
    command
}
#[test]
fn ai_cli_packages_offline_requires_consent_and_never_forwards_database_secrets() {
    let root = tempfile::tempdir().unwrap();
    let sql = root.path().join("query.sql");
    let plan = root.path().join("plan.json");
    let context = root.path().join("context.json");
    std::fs::write(&sql, "SELECT 1;").unwrap();
    std::fs::write(&plan, "{\"fixture_plan\":true}").unwrap();
    let output = command(root.path())
        .args(["ai", "context", "--engine", "sqlite", "--sql"])
        .arg(&sql)
        .arg("--plan")
        .arg(&plan)
        .arg("--output")
        .arg(&context)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !root.path().join("state").exists(),
        "Offline context packaging must not discover saved profiles"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&context).unwrap()).unwrap();
    assert_eq!(parsed["sql"], "SELECT 1;");
    assert_eq!(parsed["captured_plan"]["fixture_plan"], true);
    let agent = root.path().join("fake-agent");
    let capture = root.path().join("captured");
    let environment = root.path().join("environment");
    std::fs::write(&agent, format!(r##"#!/bin/sh
case " $* " in *" --help "*) echo '--ignore-user-config --ignore-rules --ephemeral --sandbox --json'; exit 0 ;; esac
cat > '{}'
printf '%s:%s:%s' "${{ROWBOAT_PASSWORD-unset}}" "${{PGPASSWORD-unset}}" "${{DATABASE_URL-unset}}" > '{}'
echo '{{"type":"item.completed","item":{{"type":"agent_message","text":"Advice only"}}}}'
"##, capture.display(), environment.display())).unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut denied = command(root.path());
    denied
        .args(["ai", "analyze", "--context"])
        .arg(&context)
        .args(["--agent", "codex", "--executable"])
        .arg(&agent);
    let output = denied.output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--confirm-share"));
    assert!(!capture.exists());
    let output = command(root.path())
        .env("ROWBOAT_PASSWORD", "fixture-secret")
        .env("PGPASSWORD", "fixture-secret")
        .env("DATABASE_URL", "fixture-secret")
        .args(["ai", "analyze", "--context"])
        .arg(&context)
        .args(["--agent", "codex", "--executable"])
        .arg(&agent)
        .arg("--confirm-share")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Advice only"
    );
    assert_eq!(
        std::fs::read_to_string(environment).unwrap(),
        "unset:unset:unset"
    );
    assert!(
        std::fs::read_to_string(capture)
            .unwrap()
            .contains("SELECT 1;")
    );
}
#[test]
fn ai_cli_respects_saved_connection_block_without_connecting() {
    let root = tempfile::tempdir().unwrap();
    let profile = rowboat_core::ConnectionProfile {
        name: "Blocked fixture".into(),
        ..Default::default()
    };
    let mut state = rowboat_services::AppState::default();
    state.preferences.ai.enabled = true;
    state
        .preferences
        .ai
        .disabled_connections
        .insert(profile.id.clone());
    state.connections.push(profile);
    rowboat_services::Store::open_at(root.path().join("state"))
        .unwrap()
        .save(&state)
        .unwrap();
    let output = command(root.path())
        .args([
            "--connection",
            "Blocked fixture",
            "ai",
            "analyze",
            "--context",
            "/nonexistent/fixture",
            "--agent",
            "codex",
            "--confirm-share",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("disabled for this saved connection"));
}
