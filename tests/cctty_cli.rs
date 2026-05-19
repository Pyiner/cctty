use assert_cmd::Command;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::time::Duration;
use wait_timeout::ChildExt;

mod support;
use support::FakeClaude;

#[test]
fn proxies_version_to_real_claude() {
    let fixture = FakeClaude::new();
    let mut cmd = Command::cargo_bin("cctty").unwrap();

    cmd.env("CCTTY_CLAUDE_PATH", fixture.path())
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains("fake claude 0.0.0"));
}

#[test]
fn stream_json_text_prompt_uses_tty_transcript() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000001";

    let output = Command::cargo_bin("cctty")
        .unwrap()
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .current_dir(workspace.path())
        .args([
            "--print",
            "--output-format",
            "stream-json",
            "--input-format",
            "text",
            "--session-id",
            session_id,
            "--model",
            "sonnet",
            "Say OK",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines = stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        json_types(&lines),
        ["system", "user", "assistant", "result"]
    );
    assert_eq!(lines[0]["session_id"], session_id);
    assert_eq!(lines[1]["message"]["content"], "Say OK");
    assert_eq!(
        lines[2]["message"]["content"][0]["text"],
        "FAKE_RESPONSE: Say OK"
    );
    assert_eq!(lines[3]["result"], "FAKE_RESPONSE: Say OK");
}

#[test]
fn passes_all_permission_modes_to_underlying_claude_tty() {
    for mode in [
        "acceptEdits",
        "auto",
        "bypassPermissions",
        "default",
        "dontAsk",
        "plan",
    ] {
        let fixture = FakeClaude::new();
        let workspace = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let argv_path = tempfile::NamedTempFile::new().unwrap();

        Command::cargo_bin("cctty")
            .unwrap()
            .env("CCTTY_CLAUDE_PATH", fixture.path())
            .env("CLAUDE_CONFIG_DIR", config_dir.path())
            .env("FAKE_CLAUDE_ARGV_PATH", argv_path.path())
            .current_dir(workspace.path())
            .args([
                "--print",
                "--output-format",
                "stream-json",
                "--permission-mode",
                mode,
                "Check mode",
            ])
            .assert()
            .success();

        let argv: Vec<String> =
            serde_json::from_str(&std::fs::read_to_string(argv_path.path()).unwrap()).unwrap();
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["--permission-mode", mode]),
            "permission mode {mode} not forwarded in argv {argv:?}"
        );
    }
}

#[test]
fn passes_agent_definition_flags_to_underlying_claude_tty() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let argv_path = tempfile::NamedTempFile::new().unwrap();
    let agents =
        r#"{"reviewer":{"description":"Review synthetic code","prompt":"Review carefully"}}"#;

    Command::cargo_bin("cctty")
        .unwrap()
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("FAKE_CLAUDE_ARGV_PATH", argv_path.path())
        .current_dir(workspace.path())
        .args([
            "--print",
            "--output-format",
            "stream-json",
            "--agents",
            agents,
            "--agent",
            "reviewer",
            "Use reviewer",
        ])
        .assert()
        .success();

    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(argv_path.path()).unwrap()).unwrap();
    assert!(argv.windows(2).any(|pair| pair == ["--agents", agents]));
    assert!(argv.windows(2).any(|pair| pair == ["--agent", "reviewer"]));
}

#[test]
fn no_session_persistence_removes_generated_transcript() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let persistent_config_dir = tempfile::tempdir().unwrap();

    Command::cargo_bin("cctty")
        .unwrap()
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", persistent_config_dir.path())
        .current_dir(workspace.path())
        .args([
            "--print",
            "--output-format",
            "stream-json",
            "--no-session-persistence",
            "No persistence",
        ])
        .assert()
        .success();

    assert!(
        !persistent_config_dir.path().join("projects").exists(),
        "persistent CLAUDE_CONFIG_DIR should not receive transcripts"
    );
}

#[test]
fn stream_json_permission_prompt_stdio_bridges_can_use_tool_request() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let argv_path = tempfile::NamedTempFile::new().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000003";
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("FAKE_CLAUDE_ARGV_PATH", argv_path.path())
        .current_dir(workspace.path())
        .args([
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--permission-prompt-tool",
            "stdio",
            "--session-id",
            session_id,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    writeln!(
        stdin,
        r#"{{"type":"user","message":{{"role":"user","content":"USE_FAKE_TOOL"}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut saw_permission_request = false;
    let mut saw_allowed_result = false;
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap() > 0 {
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        match value.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                saw_permission_request = true;
                assert_eq!(
                    value["request"]["subtype"],
                    Value::String("can_use_tool".to_owned())
                );
                assert_eq!(
                    value["request"]["tool_name"],
                    Value::String("Bash".to_owned())
                );
                assert_eq!(value["request"]["permission_suggestions"], Value::Null);
                assert_eq!(value["request"]["blocked_path"], Value::Null);
                let request_id = value["request_id"].as_str().unwrap();
                writeln!(
                    stdin,
                    "{}",
                    serde_json::json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "success",
                            "request_id": request_id,
                            "response": {
                                "behavior": "allow",
                                "toolUseID": "tool-1"
                            }
                        }
                    })
                )
                .unwrap();
                stdin.flush().unwrap();
            }
            Some("result") => {
                saw_allowed_result = value["result"] == "FAKE_TOOL_ALLOWED";
                break;
            }
            _ => {}
        }
        line.clear();
    }
    drop(stdin);

    assert!(
        saw_permission_request,
        "expected can_use_tool control_request"
    );
    assert!(saw_allowed_result, "expected allowed fake tool result");
    let status = child
        .wait_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("cctty did not exit after stdin closed");
        });
    assert!(status.success());

    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(argv_path.path()).unwrap()).unwrap();
    assert!(
        !argv.iter().any(|arg| arg == "--permission-prompt-tool"),
        "cctty should consume --permission-prompt-tool instead of forwarding it to interactive Claude: {argv:?}"
    );
}

#[test]
fn stream_json_permission_prompt_stdio_maps_denial_to_keyboard_form() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000004";
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .current_dir(workspace.path())
        .args([
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--permission-prompt-tool",
            "stdio",
            "--session-id",
            session_id,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    writeln!(
        stdin,
        r#"{{"type":"user","message":{{"role":"user","content":"USE_FAKE_TOOL"}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut saw_permission_request = false;
    let mut saw_denied_result = false;
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap() > 0 {
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        match value.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                saw_permission_request = true;
                let request_id = value["request_id"].as_str().unwrap();
                writeln!(
                    stdin,
                    "{}",
                    serde_json::json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "success",
                            "request_id": request_id,
                            "response": {
                                "behavior": "deny",
                                "message": "Use a safer command"
                            }
                        }
                    })
                )
                .unwrap();
                stdin.flush().unwrap();
            }
            Some("result") => {
                saw_denied_result = value["result"] == "FAKE_TOOL_DENIED: Use a safer command";
                break;
            }
            _ => {}
        }
        line.clear();
    }
    drop(stdin);

    assert!(
        saw_permission_request,
        "expected can_use_tool control_request"
    );
    assert!(saw_denied_result, "expected denied fake tool result");
    let status = child
        .wait_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("cctty did not exit after stdin closed");
        });
    assert!(status.success());
}

fn json_types(lines: &[Value]) -> Vec<&str> {
    lines
        .iter()
        .map(|line| line["type"].as_str().unwrap())
        .collect()
}
