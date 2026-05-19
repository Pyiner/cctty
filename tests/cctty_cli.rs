use assert_cmd::Command;
use serde_json::Value;

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

fn json_types(lines: &[Value]) -> Vec<&str> {
    lines
        .iter()
        .map(|line| line["type"].as_str().unwrap())
        .collect()
}
