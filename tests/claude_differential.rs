use std::io::Read;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;
use wait_timeout::ChildExt;
#[test]
#[ignore = "requires real Claude CLI auth and spends one Claude call"]
fn live_stream_json_shape_matches_claude_print() {
    if std::env::var("CCTTY_LIVE_CLAUDE_DIFF").ok().as_deref() != Some("1") {
        eprintln!("set CCTTY_LIVE_CLAUDE_DIFF=1 to run this live differential");
        return;
    }

    let prompt = "Reply exactly CCTTY_DIFF_OK and use no other words.";
    let claude_workspace = tempfile::tempdir().unwrap();
    let cctty_workspace = tempfile::tempdir().unwrap();

    let claude = run_jsonl(
        "claude",
        claude_workspace.path(),
        &[
            "--print",
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "text",
            "--permission-mode",
            "bypassPermissions",
            "--max-turns",
            "1",
            prompt,
        ],
    );
    let cctty = run_jsonl(
        env!("CARGO_BIN_EXE_cctty"),
        cctty_workspace.path(),
        &[
            "--print",
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "text",
            "--permission-mode",
            "bypassPermissions",
            "--max-turns",
            "1",
            prompt,
        ],
    );

    assert!(has_type(&claude, "system"), "claude output: {claude:?}");
    assert!(has_type(&claude, "assistant"), "claude output: {claude:?}");
    assert!(has_type(&claude, "result"), "claude output: {claude:?}");
    assert!(has_type(&cctty, "system"), "cctty output: {cctty:?}");
    assert!(has_type(&cctty, "assistant"), "cctty output: {cctty:?}");
    assert!(has_type(&cctty, "result"), "cctty output: {cctty:?}");
    assert!(assistant_text(&claude).contains("CCTTY_DIFF_OK"));
    assert!(assistant_text(&cctty).contains("CCTTY_DIFF_OK"));
}

fn run_jsonl(bin: &str, cwd: &std::path::Path, args: &[&str]) -> Vec<Value> {
    let mut child = Command::new(bin)
        .current_dir(cwd)
        .env("CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK", "1")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to run {bin}: {error}"));
    let status = child
        .wait_timeout(Duration::from_secs(120))
        .unwrap_or_else(|error| panic!("failed waiting for {bin}: {error}"));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        if let Some(mut pipe) = child.stdout.take() {
            let _ = pipe.read_to_end(&mut stdout);
        }
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_end(&mut stderr);
        }
        panic!(
            "{bin} timed out\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    }
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_end(&mut stdout)
            .unwrap_or_else(|error| panic!("failed reading {bin} stdout: {error}"));
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_end(&mut stderr)
            .unwrap_or_else(|error| panic!("failed reading {bin} stderr: {error}"));
    }
    let status = status.unwrap();

    assert!(
        status.success(),
        "{bin} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );

    String::from_utf8(stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

fn has_type(lines: &[Value], expected: &str) -> bool {
    lines
        .iter()
        .any(|line| line.get("type").and_then(Value::as_str) == Some(expected))
}

fn assistant_text(lines: &[Value]) -> String {
    let mut out = String::new();
    for line in lines {
        if line.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(items) = line
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
        {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(text) = item.get("text").and_then(Value::as_str)
                {
                    out.push_str(text);
                }
            }
        }
    }
    out
}
