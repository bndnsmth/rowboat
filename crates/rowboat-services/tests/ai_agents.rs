//! Executable fixtures only. Never launches a user's agent or model account.
use rowboat_services::ai::{self, AgentControl, AgentProvider, AiPreferences};
use std::{os::unix::fs::PermissionsExt, path::Path};

fn executable(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
#[tokio::test]
async fn all_adapters_delegate_reviewed_context_and_decode_advice() {
    let fixture = tempfile::tempdir().unwrap();
    for provider in AgentProvider::ALL {
        let agent = fixture
            .path()
            .join(format!("fake {}", provider.executable()));
        let input = fixture.path().join("input");
        let arguments = fixture.path().join("arguments");
        let working = fixture.path().join("working");
        let record = match provider {
            AgentProvider::Codex => {
                r#"{"type":"thread.started","thread_id":"fixture"}
{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Code Mode is unavailable because code-mode host is disabled. Code mode will fail closed; enable `features.code_mode_host` and install `codex-code-mode-host`."}}
{"type":"turn.started"}
{"type":"item.completed","item":{"type":"agent_message","text":"Measured evidence required.\n```sql\nSELECT 1;\n```"}}
{"type":"turn.completed"}"#
            }
            AgentProvider::Claude => {
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Measured evidence required.\n```sql\nSELECT 1;\n```"}]}}"#
            }
            AgentProvider::Opencode => {
                r#"{"type":"text","part":{"text":"Measured evidence required.\n```sql\nSELECT 1;\n```"}}"#
            }
        };
        executable(
            &agent,
            &format!(
                r##"#!/bin/sh
case " $* " in
  *" --version "*) printf '%s\n' '2.1.312 (Claude Code)'; exit 0 ;;
  *" --help "*) printf '%s\n' '--ignore-user-config --ignore-rules --ephemeral --sandbox --json --safe-mode --tools --strict-mcp-config --no-session-persistence --permission-prompts --standalone'; exit 0 ;;
esac
printf '%s\n' "$@" > '{}'
pwd > '{}'
if test -f context.txt; then cat context.txt > '{}'; else cat > '{}'; fi
printf '%s\n' '{}'
"##,
                arguments.display(),
                working.display(),
                input.display(),
                input.display(),
                record
            ),
        );
        let settings = AiPreferences {
            enabled: true,
            provider,
            executable: agent.to_string_lossy().to_string(),
            ..Default::default()
        };
        let context = "reviewed-fixture-only $(not-a-shell-command)";
        let mut chunks = String::new();
        let advice = ai::analyze(&settings, context, &AgentControl::default(), |text| {
            chunks.push_str(&text)
        })
        .await
        .unwrap();
        assert_eq!(advice, chunks);
        assert!(!advice.contains("Code Mode is unavailable"));
        assert_eq!(ai::suggested_sql(&advice), vec!["SELECT 1;"]);
        assert!(std::fs::read_to_string(&input).unwrap().contains(context));
        assert!(
            !std::fs::read_to_string(&arguments)
                .unwrap()
                .contains(context)
        );
        let directory = std::fs::read_to_string(&working).unwrap();
        assert!(
            !Path::new(directory.trim()).exists(),
            "Private request directory must be cleaned up"
        );
    }
}
#[tokio::test]
async fn incompatible_and_disabled_agents_fail_without_submitting_context() {
    let fixture = tempfile::tempdir().unwrap();
    let agent = fixture.path().join("incompatible-agent");
    let marker = fixture.path().join("submitted");
    executable(
        &agent,
        &format!(
            "#!/bin/sh\ncase \" $* \" in *\" --help \"*) echo old-agent; exit 0 ;; esac\ntouch '{}'\n",
            marker.display()
        ),
    );
    let mut settings = AiPreferences {
        enabled: true,
        executable: agent.to_string_lossy().to_string(),
        ..Default::default()
    };
    let error = ai::analyze(
        &settings,
        "reviewed fixture",
        &AgentControl::default(),
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("restricted automation"));
    assert!(!marker.exists());
    settings.enabled = false;
    assert!(
        ai::analyze(
            &settings,
            "reviewed fixture",
            &AgentControl::default(),
            |_| {}
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("disabled")
    );
    assert!(!marker.exists());
}

#[tokio::test]
async fn claude_uses_supported_version_baseline_not_truncated_help() {
    let fixture = tempfile::tempdir().unwrap();
    let agent = fixture.path().join("claude-version-fixture");
    for (version, accepted) in [
        ("2.1.311", false),
        ("2.1.312", true),
        ("2.2.0", true),
        ("3.0.0", false),
        ("unknown", false),
    ] {
        executable(
            &agent,
            &format!(
                "#!/bin/sh\ncase \" $* \" in *\" --version \"*) echo '{} (Claude Code)'; exit 0 ;; esac\nexit 1\n",
                version
            ),
        );
        let settings = AiPreferences {
            provider: AgentProvider::Claude,
            executable: agent.to_string_lossy().to_string(),
            ..Default::default()
        };
        assert_eq!(
            ai::check_agent(&settings, &AgentControl::default())
                .await
                .is_ok(),
            accepted,
            "version {version}"
        );
    }
}
