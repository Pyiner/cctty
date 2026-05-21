use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

#[test]
#[ignore = "requires real Claude CLI auth and spends real Claude calls"]
fn live_permission_prompt_stdio_honors_project_ask_rules() {
    if std::env::var("CCTTY_LIVE_PERMISSION").ok().as_deref() != Some("1") {
        eprintln!("set CCTTY_LIVE_PERMISSION=1 to run this live permission test");
        return;
    }

    let allowed = run_live_permission_case("cctty-live-permission-allow", "allow");
    assert_eq!(
        allowed.request["request"]["input"]["command"],
        "printf cctty-live-permission-allow"
    );
    assert!(allowed.tool_result.contains("cctty-live-permission-allow"));
    assert!(allowed.assistant_text.contains("Done"));
    assert_eq!(allowed.result["is_error"], false);

    let denied = run_live_permission_case("cctty-live-permission-deny", "deny");
    assert_eq!(
        denied.request["request"]["input"]["command"],
        "printf cctty-live-permission-deny"
    );
    assert!(denied.tool_result.contains("rejected"));
    assert!(!denied.tool_result.contains("cctty-live-permission-deny"));
    assert_eq!(denied.result["is_error"], true);
    assert_eq!(denied.result["result"], "Permission denied");
}

#[test]
#[ignore = "requires real Claude CLI auth and spends real Claude calls"]
fn live_permission_modes_smoke_common_modes() {
    if std::env::var("CCTTY_LIVE_PERMISSION_MODES").ok().as_deref() != Some("1") {
        eprintln!("set CCTTY_LIVE_PERMISSION_MODES=1 to run live permission-mode smoke tests");
        return;
    }

    for (mode, token, prompt) in [
        (
            "plan",
            "CCTTY_PLAN_MODE_OK",
            "In plan mode, write a short implementation plan and include CCTTY_PLAN_MODE_OK. Do not use tools.",
        ),
        (
            "auto",
            "CCTTY_AUTO_MODE_OK",
            "Reply exactly CCTTY_AUTO_MODE_OK and use no tools.",
        ),
        (
            "dontAsk",
            "CCTTY_DONTASK_MODE_OK",
            "Reply exactly CCTTY_DONTASK_MODE_OK and use no tools.",
        ),
        (
            "acceptEdits",
            "CCTTY_ACCEPT_EDITS_MODE_OK",
            "Reply exactly CCTTY_ACCEPT_EDITS_MODE_OK and use no tools.",
        ),
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let output = run_jsonl(
            env!("CARGO_BIN_EXE_cctty"),
            workspace.path(),
            &[
                "--print",
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "text",
                "--permission-mode",
                mode,
                "--max-turns",
                "1",
                "--no-chrome",
                prompt,
            ],
        );
        assert!(has_type(&output, "assistant"), "{mode} output: {output:?}");
        assert!(has_type(&output, "result"), "{mode} output: {output:?}");
        assert!(
            assistant_text(&output).contains(token),
            "{mode} output: {output:?}"
        );
    }
}

#[test]
#[ignore = "requires real Claude CLI auth and spends real Claude calls"]
fn live_accept_edits_writes_file_without_sdk_permission_callback() {
    if std::env::var("CCTTY_LIVE_PERMISSION_MODES").ok().as_deref() != Some("1") {
        eprintln!("set CCTTY_LIVE_PERMISSION_MODES=1 to run live permission-mode smoke tests");
        return;
    }

    let workspace = tempfile::tempdir().unwrap();
    let output = run_jsonl(
        env!("CARGO_BIN_EXE_cctty"),
        workspace.path(),
        &[
            "--print",
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "text",
            "--permission-mode",
            "acceptEdits",
            "--max-turns",
            "4",
            "--no-chrome",
            "Create a file named cctty_accept_edits.txt containing exactly CCTTY_ACCEPT_EDITS_FILE_OK. Do not create any other files.",
        ],
    );
    assert!(has_type(&output, "result"), "output: {output:?}");
    let written = std::fs::read_to_string(workspace.path().join("cctty_accept_edits.txt"))
        .unwrap_or_else(|error| panic!("missing acceptEdits output file: {error}\n{output:?}"));
    assert!(written.contains("CCTTY_ACCEPT_EDITS_FILE_OK"));
}

struct LivePermissionOutcome {
    request: Value,
    result: Value,
    tool_result: String,
    assistant_text: String,
}

fn run_live_permission_case(token: &str, behavior: &str) -> LivePermissionOutcome {
    let workspace = tempfile::tempdir().unwrap();
    let claude_dir = workspace.path().join(".claude");
    std::fs::create_dir(&claude_dir).unwrap();
    std::fs::write(
        claude_dir.join("settings.local.json"),
        serde_json::json!({
            "permissions": {
                "ask": ["Bash(printf:*)"],
                "defaultMode": "default",
                "disableAutoMode": "disable"
            },
            "disableAllHooks": true
        })
        .to_string(),
    )
    .unwrap();

    let session_id = uuid::Uuid::new_v4().to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cctty"))
        .current_dir(workspace.path())
        .env("CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK", "1")
        .args([
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--permission-prompt-tool",
            "stdio",
            "--session-id",
            &session_id,
            "--permission-mode",
            "default",
            "--no-chrome",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to run cctty: {error}"));
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": format!(
                    "Use Bash to run exactly `printf {token}`, then respond Done. Do not modify files."
                )
            }
        })
    )
    .unwrap();
    stdin.flush().unwrap();

    let deadline = Instant::now() + Duration::from_secs(180);
    let mut request = None;
    let mut result = None;
    let mut tool_result = String::new();
    let mut assistant_text = String::new();
    let mut raw_stdout = String::new();

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = remaining.min(Duration::from_millis(500));
        match rx.recv_timeout(timeout) {
            Ok(line) => {
                raw_stdout.push_str(&line);
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match value.get("type").and_then(Value::as_str) {
                    Some("control_request") => {
                        assert!(request.is_none(), "duplicate control_request\n{raw_stdout}");
                        let request_id = value["request_id"].as_str().unwrap();
                        let mut response = serde_json::json!({ "behavior": behavior });
                        if behavior == "deny" {
                            response["message"] =
                                Value::String("Do not run this command".to_owned());
                        }
                        writeln!(
                            stdin,
                            "{}",
                            serde_json::json!({
                                "type": "control_response",
                                "response": {
                                    "subtype": "success",
                                    "request_id": request_id,
                                    "response": response
                                }
                            })
                        )
                        .unwrap();
                        stdin.flush().unwrap();
                        request = Some(value);
                    }
                    Some("assistant") => {
                        assistant_text.push_str(&assistant_text_from_value(&value));
                    }
                    Some("user") => {
                        tool_result.push_str(&tool_result_text_from_value(&value));
                    }
                    Some("result") => {
                        result = Some(value);
                        break;
                    }
                    _ => {}
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    drop(stdin);
    let status = child
        .wait_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cctty did not exit\nstdout:\n{raw_stdout}");
        });

    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr).unwrap();
    }
    assert!(status.success(), "stderr:\n{stderr}\nstdout:\n{raw_stdout}");

    LivePermissionOutcome {
        request: request
            .unwrap_or_else(|| panic!("missing control_request\nstdout:\n{raw_stdout}")),
        result: result.unwrap_or_else(|| panic!("missing result\nstdout:\n{raw_stdout}")),
        tool_result,
        assistant_text,
    }
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

fn assistant_text_from_value(value: &Value) -> String {
    let mut out = String::new();
    if let Some(items) = value
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
    out
}

fn tool_result_text_from_value(value: &Value) -> String {
    let mut out = String::new();
    if let Some(items) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("tool_result") {
                match item.get("content") {
                    Some(Value::String(text)) => out.push_str(text),
                    Some(other) => out.push_str(&other.to_string()),
                    None => {}
                }
            }
        }
    }
    out
}
