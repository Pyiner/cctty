use assert_cmd::Command;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};
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
fn writes_diagnostic_log_to_configured_file() {
    let fixture = FakeClaude::new();
    let log = tempfile::NamedTempFile::new().unwrap();

    Command::cargo_bin("cctty")
        .unwrap()
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CCTTY_LOG_FILE", log.path())
        .arg("--version")
        .assert()
        .success();

    let text = std::fs::read_to_string(log.path()).unwrap();
    assert!(text.contains("start mode=Passthrough"), "{text}");
    assert!(text.contains("passthrough_spawn"), "{text}");
    assert!(text.contains("passthrough_exit exit_code=0"), "{text}");
    assert!(text.contains("finish exit_code=0"), "{text}");
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
fn stream_json_accepts_non_uuid_host_session_id() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let argv_path = tempfile::NamedTempFile::new().unwrap();
    let external_session_id = "conductor-session-1";

    let output = Command::cargo_bin("cctty")
        .unwrap()
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("FAKE_CLAUDE_ARGV_PATH", argv_path.path())
        .current_dir(workspace.path())
        .args([
            "--print",
            "--output-format",
            "stream-json",
            "--input-format",
            "text",
            "--session-id",
            external_session_id,
            "Say OK",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(argv_path.path()).unwrap()).unwrap();
    let claude_session_id = argv
        .windows(2)
        .find(|pair| pair[0] == "--session-id")
        .map(|pair| pair[1].clone())
        .expect("fake Claude should receive --session-id");
    assert_ne!(claude_session_id, external_session_id);
    uuid::Uuid::parse_str(&claude_session_id).unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines = stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lines[0]["session_id"], external_session_id);
    assert_eq!(lines[3]["session_id"], external_session_id);
}

#[test]
fn interactive_claude_gets_terminal_env_not_sdk_transport_env() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let env_path = tempfile::NamedTempFile::new().unwrap();

    Command::cargo_bin("cctty")
        .unwrap()
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("FAKE_CLAUDE_ENV_PATH", env_path.path())
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("CLAUDE_CODE_ENTRYPOINT", "sdk-py")
        .env("CLAUDE_AGENT_SDK_VERSION", "0.0.0")
        .env("CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK", "1")
        .current_dir(workspace.path())
        .args(["--print", "--output-format", "stream-json", "Say OK"])
        .assert()
        .success();

    let env: Value =
        serde_json::from_str(&std::fs::read_to_string(env_path.path()).unwrap()).unwrap();
    assert_eq!(env["TERM"], "xterm-256color");
    assert_eq!(env["COLORTERM"], "truecolor");
    assert_eq!(env["NO_COLOR"], Value::Null);
    assert_eq!(env["CLAUDE_CODE_ENTRYPOINT"], Value::Null);
    assert_eq!(env["CLAUDE_AGENT_SDK_VERSION"], Value::Null);
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
    let session_lock_dir = tempfile::tempdir().unwrap();
    let argv_path = tempfile::NamedTempFile::new().unwrap();
    let agents =
        r#"{"reviewer":{"description":"Review synthetic code","prompt":"Review carefully"}}"#;

    Command::cargo_bin("cctty")
        .unwrap()
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("FAKE_CLAUDE_ARGV_PATH", argv_path.path())
        .env("FAKE_CLAUDE_SESSION_LOCK_DIR", session_lock_dir.path())
        .env("FAKE_CLAUDE_SESSION_LOCK_RELEASE_DELAY_MS", "250")
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
fn passes_mcp_flags_to_underlying_claude_tty() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let argv_path = tempfile::NamedTempFile::new().unwrap();
    let mcp_config =
        r#"{"mcpServers":{"docs":{"type":"stdio","command":"node","args":["server.js"]}}}"#;

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
            "--mcp-config",
            mcp_config,
            "--strict-mcp-config",
            "--mcp-debug",
            "Check MCP",
        ])
        .assert()
        .success();

    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(argv_path.path()).unwrap()).unwrap();
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["--mcp-config", mcp_config])
    );
    let mcp_config_arg = argv
        .windows(2)
        .find(|pair| pair[0] == "--mcp-config")
        .map(|pair| pair[1].clone())
        .expect("expected --mcp-config to be passed to fake Claude");
    assert_eq!(mcp_config_arg, mcp_config);
    let proxy_marker = ["__cctty", "mcp", "proxy"].join("-");
    assert!(!mcp_config_arg.contains(&proxy_marker));
    assert!(argv.iter().any(|arg| arg == "--strict-mcp-config"));
    assert!(argv.iter().any(|arg| arg == "--mcp-debug"));
}

#[test]
fn runs_underlying_claude_in_cctty_current_directory() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let cwd_path = tempfile::NamedTempFile::new().unwrap();

    Command::cargo_bin("cctty")
        .unwrap()
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("FAKE_CLAUDE_CWD_PATH", cwd_path.path())
        .current_dir(workspace.path())
        .args(["--print", "--output-format", "stream-json", "Check cwd"])
        .assert()
        .success();

    let cwd = std::fs::read_to_string(cwd_path.path()).unwrap();
    let expected = std::fs::canonicalize(workspace.path()).unwrap();
    assert_eq!(std::path::Path::new(cwd.trim()), expected.as_path());
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
fn stream_json_initialize_returns_sdk_metadata_for_wrappers() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .current_dir(workspace.path())
        .args([
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            serde_json::json!({
                "type": "control_request",
                "request_id": "init-1",
                "request": { "subtype": "initialize" },
            })
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let status = child.wait_timeout(Duration::from_secs(10)).unwrap();
    if status.is_none() {
        let _ = child.kill();
        panic!("cctty did not exit after stdin closed");
    }
    assert!(status.unwrap().success());

    let mut stdout = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
    let response = stdout
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .expect("expected JSON control response");

    assert_eq!(response["type"], "control_response");
    assert_eq!(response["response"]["request_id"], "init-1");
    assert_eq!(response["response"]["subtype"], "success");
    assert_eq!(
        response["response"]["response"]["models"][0]["value"],
        "default"
    );
    let models = response["response"]["response"]["models"]
        .as_array()
        .expect("models should be an array")
        .iter()
        .filter_map(|model| model["value"].as_str())
        .collect::<Vec<_>>();
    assert!(
        models.contains(&"opus"),
        "expected Conductor default model alias in {models:?}"
    );
    assert_eq!(
        response["response"]["response"]["available_output_styles"][0],
        "default"
    );
    assert!(response["response"]["response"]["commands"].is_array());
    assert!(response["response"]["response"]["agents"].is_array());
}

#[test]
fn stream_json_accepts_wrapper_control_requests_after_initialize() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .current_dir(workspace.path())
        .args([
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    {
        let stdin = child.stdin.as_mut().unwrap();
        for value in [
            serde_json::json!({
                "type": "control_request",
                "request_id": "model-1",
                "request": { "subtype": "set_model", "model": "default" },
            }),
            serde_json::json!({
                "type": "control_request",
                "request_id": "mode-1",
                "request": { "subtype": "set_permission_mode", "mode": "default" },
            }),
            serde_json::json!({
                "type": "control_request",
                "request_id": "settings-1",
                "request": { "subtype": "apply_flag_settings", "settings": {} },
            }),
            serde_json::json!({
                "type": "control_request",
                "request_id": "mcp-1",
                "request": { "subtype": "mcp_status" },
            }),
        ] {
            writeln!(stdin, "{value}").unwrap();
        }
    }
    drop(child.stdin.take());

    let status = child.wait_timeout(Duration::from_secs(10)).unwrap();
    if status.is_none() {
        let _ = child.kill();
        panic!("cctty did not exit after stdin closed");
    }
    assert!(status.unwrap().success());

    let mut stdout = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
    let responses = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();

    assert_eq!(responses.len(), 4, "stdout:\n{stdout}");
    assert!(responses.iter().all(|value| {
        value["type"] == "control_response" && value["response"]["subtype"] == "success"
    }));
    assert_eq!(
        responses[3]["response"]["response"]["mcpServers"],
        Value::Array(vec![])
    );
}

#[test]
fn stream_json_control_updates_restart_tty_with_new_model_and_permission_mode() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let argv_path = tempfile::NamedTempFile::new().unwrap();
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
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    {
        let stdin = child.stdin.as_mut().unwrap();
        for value in [
            serde_json::json!({
                "type": "control_request",
                "request_id": "model-1",
                "request": { "subtype": "set_model", "model": "opus" },
            }),
            serde_json::json!({
                "type": "control_request",
                "request_id": "mode-1",
                "request": { "subtype": "set_permission_mode", "mode": "plan" },
            }),
            serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": "Reply OK" },
            }),
        ] {
            writeln!(stdin, "{value}").unwrap();
        }
    }
    drop(child.stdin.take());

    let status = child
        .wait_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("cctty did not exit after stdin closed");
        });
    assert!(status.success());

    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(argv_path.path()).unwrap()).unwrap();
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "--model" && pair[1] == "opus"),
        "argv={argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "--permission-mode" && pair[1] == "plan"),
        "argv={argv:?}"
    );
}

#[test]
fn stream_json_metadata_control_requests_do_not_wait_for_tty_prompt() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("FAKE_CLAUDE_STARTUP_DELAY_MS", "1500")
        .current_dir(workspace.path())
        .args([
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    let started = Instant::now();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "control_request",
            "request_id": "metadata-init",
            "request": { "subtype": "initialize" },
        })
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(800),
        "initialize waited for TTY startup: {line}"
    );
    let response: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(response["response"]["request_id"], "metadata-init");
    line.clear();

    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "control_request",
            "request_id": "metadata-mcp",
            "request": { "subtype": "mcp_status" },
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    reader.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(response["response"]["request_id"], "metadata-mcp");
    assert_eq!(
        response["response"]["response"]["mcpServers"],
        Value::Array(vec![])
    );
    drop(stdin);

    let status = child
        .wait_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("cctty did not exit after stdin closed");
        });
    assert!(status.success());
}

#[test]
fn stream_json_ts_sdk_mcp_server_round_trips_tool_calls() {
    assert_sdk_mcp_server_round_trips_tool_calls(
        Vec::new(),
        serde_json::json!({ "subtype": "initialize", "sdkMcpServers": ["conductor"], "systemPrompt": [""] }),
    );
}

#[test]
fn stream_json_python_sdk_mcp_config_round_trips_tool_calls() {
    assert_sdk_mcp_server_round_trips_tool_calls(
        vec![
            "--mcp-config".to_owned(),
            serde_json::json!({ "mcpServers": { "conductor": { "type": "sdk", "name": "conductor" } } }).to_string(),
            "--strict-mcp-config".to_owned(),
        ],
        serde_json::json!({ "subtype": "initialize", "hooks": null }),
    );
}

#[test]
fn stream_json_sdk_mcp_survives_control_restart_between_turns() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let session_lock_dir = tempfile::tempdir().unwrap();
    let argv_path = tempfile::NamedTempFile::new().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000015";
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("FAKE_CLAUDE_ARGV_PATH", argv_path.path())
        .env("FAKE_CLAUDE_SESSION_LOCK_DIR", session_lock_dir.path())
        .env("FAKE_CLAUDE_SESSION_LOCK_STALE_MS", "650")
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
    let mut line = String::new();

    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "control_request",
            "request_id": "init-mcp-restart",
            "request": { "subtype": "initialize", "sdkMcpServers": ["conductor"], "systemPrompt": [""] },
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let init_response = read_json_line(&mut reader, &mut line);
    assert_eq!(init_response["type"], "control_response");
    assert_eq!(init_response["response"]["request_id"], "init-mcp-restart");

    let (first_methods, first_result) = drive_fake_sdk_mcp_prompt(
        &mut stdin,
        &mut reader,
        &mut line,
        "USE_FAKE_SDK_MCP_ASK first",
    );
    assert_eq!(first_methods, ["initialize", "tools/list", "tools/call"]);
    assert!(first_result.contains("Technical design"), "{first_result}");

    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "control_request",
            "request_id": "set-model-after-mcp",
            "request": { "subtype": "set_model", "model": "opus" },
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let model_response = read_json_line(&mut reader, &mut line);
    assert_eq!(model_response["type"], "control_response");
    assert_eq!(
        model_response["response"]["request_id"],
        "set-model-after-mcp"
    );

    let (second_methods, second_result) = drive_fake_sdk_mcp_prompt(
        &mut stdin,
        &mut reader,
        &mut line,
        "USE_FAKE_SDK_MCP_ASK second",
    );
    assert_eq!(second_methods, ["initialize", "tools/list", "tools/call"]);
    assert!(second_result.contains("SDK users"), "{second_result}");
    drop(stdin);

    let status = child
        .wait_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("cctty did not exit after stdin closed");
        });
    assert!(status.success());

    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(argv_path.path()).unwrap()).unwrap();
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "--model" && pair[1] == "opus"),
        "argv={argv:?}"
    );
    let mcp_config_arg = argv
        .windows(2)
        .find(|pair| pair[0] == "--mcp-config")
        .map(|pair| pair[1].clone())
        .expect("expected rewritten --mcp-config to be passed after restart");
    assert!(
        mcp_config_arg.contains("__cctty-mcp-proxy"),
        "{mcp_config_arg}"
    );
}

fn read_json_line<R: BufRead>(reader: &mut R, line: &mut String) -> Value {
    line.clear();
    let count = reader.read_line(line).unwrap();
    assert!(count > 0, "expected JSON line from cctty");
    serde_json::from_str(line.trim()).unwrap()
}

fn drive_fake_sdk_mcp_prompt<W: Write, R: BufRead>(
    stdin: &mut W,
    reader: &mut R,
    line: &mut String,
    prompt: &str,
) -> (Vec<String>, String) {
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": prompt },
        })
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut methods = Vec::new();
    loop {
        let value = read_json_line(reader, line);
        match value.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                assert_eq!(value["request"]["subtype"], "mcp_message");
                assert_eq!(value["request"]["server_name"], "conductor");
                let request_id = value["request_id"].as_str().unwrap();
                let message = &value["request"]["message"];
                let method = message["method"].as_str().unwrap_or_default();
                let id = message["id"].clone();
                if method != "notifications/initialized" {
                    methods.push(method.to_owned());
                }
                let mcp_response = fake_sdk_mcp_response(method, id, message);
                writeln!(
                    stdin,
                    "{}",
                    serde_json::json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "success",
                            "request_id": request_id,
                            "response": { "mcp_response": mcp_response },
                        },
                    })
                )
                .unwrap();
                stdin.flush().unwrap();
            }
            Some("result") => {
                return (
                    methods,
                    value["result"].as_str().unwrap_or_default().to_owned(),
                );
            }
            _ => {}
        }
    }
}

fn fake_sdk_mcp_response(method: &str, id: Value, message: &Value) -> Value {
    match method {
        "initialize" => {
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "protocolVersion": "2024-11-05", "capabilities": { "tools": { "listChanged": true } }, "serverInfo": { "name": "conductor", "version": "test" } } })
        }
        "notifications/initialized" => {
            serde_json::json!({ "jsonrpc": "2.0", "id": 0, "result": {} })
        }
        "tools/list" => {
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [{ "name": "AskUserQuestion", "description": "Ask the user a structured question", "inputSchema": { "type": "object", "properties": { "questions": { "type": "array" } } } }] } })
        }
        "tools/call" => {
            assert_eq!(message["params"]["name"], "AskUserQuestion");
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [{ "type": "text", "text": "User responses:\n- Document type: Technical design\n- Audience: SDK users" }] } })
        }
        other => panic!("unexpected MCP method {other}: {message}"),
    }
}

fn assert_sdk_mcp_server_round_trips_tool_calls(extra_args: Vec<String>, initialize: Value) {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let argv_path = tempfile::NamedTempFile::new().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000014";
    let mut args = vec![
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--permission-prompt-tool".to_owned(),
        "stdio".to_owned(),
        "--session-id".to_owned(),
        session_id.to_owned(),
    ];
    args.extend(extra_args);

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("FAKE_CLAUDE_ARGV_PATH", argv_path.path())
        .current_dir(workspace.path())
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    writeln!(stdin, "{}", serde_json::json!({ "type": "control_request", "request_id": "init-mcp-1", "request": initialize })).unwrap();
    stdin.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let init_response: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(init_response["type"], "control_response");
    assert_eq!(init_response["response"]["request_id"], "init-mcp-1");
    line.clear();

    writeln!(
        stdin,
        r#"{{"type":"user","message":{{"role":"user","content":"USE_FAKE_SDK_MCP_ASK"}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut methods = Vec::new();
    let mut final_result = String::new();
    while reader.read_line(&mut line).unwrap() > 0 {
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        match value.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                assert_eq!(value["request"]["subtype"], "mcp_message");
                assert_eq!(value["request"]["server_name"], "conductor");
                let request_id = value["request_id"].as_str().unwrap();
                let message = &value["request"]["message"];
                let method = message["method"].as_str().unwrap_or_default();
                let id = message["id"].clone();
                if method != "notifications/initialized" {
                    methods.push(method.to_owned());
                }
                let mcp_response = match method {
                    "initialize" => {
                        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "protocolVersion": "2024-11-05", "capabilities": { "tools": { "listChanged": true } }, "serverInfo": { "name": "conductor", "version": "test" } } })
                    }
                    "notifications/initialized" => {
                        serde_json::json!({ "jsonrpc": "2.0", "id": 0, "result": {} })
                    }
                    "tools/list" => {
                        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [{ "name": "AskUserQuestion", "description": "Ask the user a structured question", "inputSchema": { "type": "object", "properties": { "questions": { "type": "array" } } } }] } })
                    }
                    "tools/call" => {
                        assert_eq!(message["params"]["name"], "AskUserQuestion");
                        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [{ "type": "text", "text": "User responses:\n- Document type: Technical design\n- Audience: SDK users" }] } })
                    }
                    other => panic!("unexpected MCP method {other}: {value}"),
                };
                writeln!(stdin, "{}", serde_json::json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request_id, "response": { "mcp_response": mcp_response } } })).unwrap();
                stdin.flush().unwrap();
            }
            Some("result") => {
                final_result = value["result"].as_str().unwrap_or_default().to_owned();
                break;
            }
            _ => {}
        }
        line.clear();
    }
    drop(stdin);

    assert_eq!(
        methods,
        ["initialize", "tools/list", "tools/call"],
        "final_result={final_result}"
    );
    assert!(
        final_result.contains("User responses:"),
        "final_result={final_result}"
    );
    assert!(
        final_result.contains("Technical design"),
        "final_result={final_result}"
    );
    assert!(
        final_result.contains("SDK users"),
        "final_result={final_result}"
    );

    let status = child
        .wait_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("cctty did not exit after stdin closed");
        });
    assert!(status.success());

    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(argv_path.path()).unwrap()).unwrap();
    let mcp_config_arg = argv
        .windows(2)
        .find(|pair| pair[0] == "--mcp-config")
        .map(|pair| pair[1].clone())
        .expect("expected rewritten --mcp-config to be passed to fake Claude");
    assert!(
        mcp_config_arg.contains("__cctty-mcp-proxy"),
        "{mcp_config_arg}"
    );
    assert!(
        !mcp_config_arg.contains(r#""type":"sdk""#),
        "{mcp_config_arg}"
    );
}

#[test]
fn stream_json_include_partial_messages_emits_text_stream_events() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000009";
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .current_dir(workspace.path())
        .args([
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--include-partial-messages",
            "--session-id",
            session_id,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"type":"user","message":{{"role":"user","content":"Partial please"}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let status = child.wait_timeout(Duration::from_secs(10)).unwrap();
    if status.is_none() {
        let _ = child.kill();
        panic!("cctty did not exit after stdin closed");
    }
    assert!(status.unwrap().success());

    let mut stdout = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
    let values = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();

    assert!(
        values.iter().any(|value| {
            value["type"] == "stream_event"
                && value["event"]["type"] == "content_block_delta"
                && value["event"]["delta"]["type"] == "text_delta"
                && value["event"]["delta"]["text"] == "FAKE_RESPONSE: Partial please"
        }),
        "stdout:\n{stdout}"
    );
    assert!(values.iter().any(|value| value["type"] == "assistant"));
    assert!(values.iter().any(|value| value["type"] == "result"));
}

#[test]
fn stream_json_recovers_from_claude_missing_resume_startup_error() {
    assert_stream_json_recovers_from_bad_resume_startup("no_conversation");
}

#[test]
fn stream_json_handles_multiple_user_messages_in_one_sdk_process() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .current_dir(workspace.path())
        .args([
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--session-id",
            "00000000-0000-0000-0000-000000000013",
            "--permission-mode",
            "default",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    {
        let stdin = child.stdin.as_mut().unwrap();
        for prompt in ["First stream prompt", "Second stream prompt"] {
            writeln!(
                stdin,
                "{}",
                serde_json::json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": prompt,
                    },
                    "session_id": "conductor-session-1",
                })
            )
            .unwrap();
        }
    }
    drop(child.stdin.take());

    let status = child.wait_timeout(Duration::from_secs(10)).unwrap();
    if status.is_none() {
        let _ = child.kill();
        panic!("cctty did not exit after stdin closed");
    }
    assert!(status.unwrap().success());

    let mut stdout = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
    assert!(
        stdout.contains("FAKE_RESPONSE: First stream prompt"),
        "{stdout}"
    );
    assert!(
        stdout.contains("FAKE_RESPONSE: Second stream prompt"),
        "{stdout}"
    );
    let results = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["type"] == "result")
        .count();
    assert_eq!(results, 2, "{stdout}");
}

fn assert_stream_json_recovers_from_bad_resume_startup(fake_failure: &str) {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let argv_path = tempfile::NamedTempFile::new().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("FAKE_CLAUDE_FAIL_SESSION_ARGS", fake_failure)
        .env("FAKE_CLAUDE_ARGV_PATH", argv_path.path())
        .current_dir(workspace.path())
        .args([
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--resume-session-at",
            "message-1",
            "--model",
            "sonnet",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"type":"user","message":{{"role":"user","content":"Recover please"}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let status = child.wait_timeout(Duration::from_secs(10)).unwrap();
    if status.is_none() {
        let _ = child.kill();
        panic!("cctty did not exit after stdin closed");
    }
    assert!(status.unwrap().success());

    let mut stdout = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
    assert!(stdout.contains("FAKE_RESPONSE: Recover please"), "{stdout}");

    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(argv_path.path()).unwrap()).unwrap();
    assert!(
        !argv.iter().any(|arg| arg == "--resume-session-at"),
        "retry should strip resume/session args that make interactive Claude report a session lock: {argv:?}"
    );
    assert!(argv.windows(2).any(|pair| pair == ["--model", "sonnet"]));
}

#[test]
fn stream_json_emits_idle_session_state_when_sdk_requests_it() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000010";
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fixture.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1")
        .current_dir(workspace.path())
        .args([
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--session-id",
            session_id,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"type":"user","message":{{"role":"user","content":"Idle please"}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());

    let status = child.wait_timeout(Duration::from_secs(10)).unwrap();
    if status.is_none() {
        let _ = child.kill();
        panic!("cctty did not exit after stdin closed");
    }
    assert!(status.unwrap().success());

    let mut stdout = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
    let values = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();

    assert!(
        values.iter().any(|value| {
            value["type"] == "system"
                && value["subtype"] == "session_state_changed"
                && value["state"] == "idle"
                && value["session_id"] == session_id
        }),
        "stdout:\n{stdout}"
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

#[test]
fn stream_json_permission_prompt_stdio_allows_generic_tty_permission_form() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000016";
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
        r#"{{"type":"user","message":{{"role":"user","content":"USE_TTY_GENERIC_TOOL_PERMISSION"}}}}"#
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
                    value["request"]["tool_name"],
                    Value::String("ToolSearch".to_owned())
                );
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
                                "behavior": "allow"
                            }
                        }
                    })
                )
                .unwrap();
                stdin.flush().unwrap();
            }
            Some("result") => {
                saw_allowed_result = value["result"] == "FAKE_GENERIC_TOOL_ALLOWED";
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
    assert!(saw_allowed_result, "expected allowed generic tool result");
    let status = child
        .wait_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("cctty did not exit after stdin closed");
        });
    assert!(status.success());
}

#[test]
fn stream_json_permission_prompt_stdio_round_trips_ask_user_question_form() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000011";
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
        r#"{{"type":"user","message":{{"role":"user","content":"USE_FAKE_ASK_USER_QUESTION"}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut question_request_count = 0;
    let mut result = String::new();
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap() > 0 {
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        match value.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                question_request_count += 1;
                assert_eq!(
                    value["request"]["subtype"],
                    Value::String("can_use_tool".to_owned())
                );
                assert_eq!(
                    value["request"]["tool_name"],
                    Value::String("AskUserQuestion".to_owned())
                );
                assert_eq!(
                    value["request"]["input"]["questions"]
                        .as_array()
                        .unwrap()
                        .len(),
                    2
                );
                let request_id = value["request_id"].as_str().unwrap();
                assert!(
                    request_id.starts_with("cctty_permission_"),
                    "AskUserQuestion should use transcript tool input, not TTY fallback: {request_id}"
                );
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
                                "updatedInput": {
                                    "answers": {
                                        "What kind of document do you want?": [
                                            "Technical design",
                                            "API examples"
                                        ],
                                        "Who is the audience?": {
                                            "role": "Developers",
                                            "experience": "SDK users"
                                        },
                                        "Include examples": true
                                    }
                                }
                            }
                        }
                    })
                )
                .unwrap();
                stdin.flush().unwrap();
            }
            Some("result") => {
                result = value["result"].as_str().unwrap_or_default().to_owned();
                break;
            }
            _ => {}
        }
        line.clear();
    }
    drop(stdin);

    assert_eq!(
        question_request_count, 1,
        "expected exactly one AskUserQuestion can_use_tool request"
    );
    assert!(
        result.contains("FAKE_ASK_USER_FEEDBACK: 用户表单回答："),
        "result:\n{result}"
    );
    assert!(
        result.contains("- What kind of document do you want?: Technical design, API examples"),
        "result:\n{result}"
    );
    assert!(
        result.contains("- Include examples: true"),
        "result:\n{result}"
    );
    assert!(result.contains("Developers"), "result:\n{result}");
    assert!(result.contains("SDK users"), "result:\n{result}");

    let status = child
        .wait_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("cctty did not exit after stdin closed");
        });
    assert!(status.success());
}

#[test]
fn stream_json_permission_prompt_stdio_falls_back_to_tty_ask_user_question_form() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000012";
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
        r#"{{"type":"user","message":{{"role":"user","content":"USE_TTY_FIRST_FAKE_ASK_USER_QUESTION"}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut question_request_count = 0;
    let mut result = String::new();
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap() > 0 {
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        match value.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                question_request_count += 1;
                assert_eq!(
                    value["request"]["tool_name"],
                    Value::String("AskUserQuestion".to_owned())
                );
                assert_eq!(
                    value["request"]["input"]["questions"][0]["question"],
                    Value::String("What kind of document do you want?".to_owned())
                );
                let request_id = value["request_id"].as_str().unwrap();
                assert!(
                    request_id.starts_with("cctty_question_"),
                    "expected TTY fallback request id, got {request_id}"
                );
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
                                "updatedInput": {
                                    "answers": {
                                        "What kind of document do you want?": "Technical design"
                                    }
                                }
                            }
                        }
                    })
                )
                .unwrap();
                stdin.flush().unwrap();
            }
            Some("result") => {
                result = value["result"].as_str().unwrap_or_default().to_owned();
                break;
            }
            _ => {}
        }
        line.clear();
    }
    drop(stdin);

    assert_eq!(
        question_request_count, 1,
        "expected exactly one AskUserQuestion request even after late transcript"
    );
    assert!(
        result.contains("FAKE_TTY_FIRST_ASK_FEEDBACK: 用户表单回答："),
        "result:\n{result}"
    );
    assert!(
        result.contains("- What kind of document do you want?: Technical design"),
        "result:\n{result}"
    );

    let status = child
        .wait_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("cctty did not exit after stdin closed");
        });
    assert!(status.success());
}

#[test]
fn stream_json_permission_prompt_stdio_reads_tty_form_before_transcript_tool_use() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000005";
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
        r#"{{"type":"user","message":{{"role":"user","content":"USE_TTY_ONLY_FAKE_TOOL"}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut permission_request_count = 0;
    let mut saw_allowed_result = false;
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap() > 0 {
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        match value.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                permission_request_count += 1;
                assert_eq!(
                    value["request"]["subtype"],
                    Value::String("can_use_tool".to_owned())
                );
                assert_eq!(
                    value["request"]["tool_name"],
                    Value::String("Bash".to_owned())
                );
                assert_eq!(
                    value["request"]["input"]["command"],
                    Value::String("echo tty fake".to_owned())
                );
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
                                "behavior": "allow"
                            }
                        }
                    })
                )
                .unwrap();
                stdin.flush().unwrap();
            }
            Some("result") => {
                saw_allowed_result = value["result"] == "FAKE_TTY_TOOL_ALLOWED";
                break;
            }
            _ => {}
        }
        line.clear();
    }
    drop(stdin);

    assert_eq!(
        permission_request_count, 1,
        "expected exactly one can_use_tool control_request"
    );
    assert!(saw_allowed_result, "expected allowed fake TTY tool result");
    let status = child
        .wait_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("cctty did not exit after stdin closed");
        });
    assert!(status.success());
}

#[test]
fn stream_json_permission_prompt_stdio_reads_tty_write_form_before_transcript_tool_use() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000007";
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
        r#"{{"type":"user","message":{{"role":"user","content":"USE_TTY_WRITE_FAKE_TOOL"}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut permission_request_count = 0;
    let mut saw_denied_result = false;
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap() > 0 {
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        match value.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                permission_request_count += 1;
                assert_eq!(
                    value["request"]["subtype"],
                    Value::String("can_use_tool".to_owned())
                );
                assert_eq!(
                    value["request"]["tool_name"],
                    Value::String("Write".to_owned())
                );
                assert_eq!(
                    value["request"]["input"]["file_path"],
                    Value::String("index.html".to_owned())
                );
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
                                "behavior": "deny"
                            }
                        }
                    })
                )
                .unwrap();
                stdin.flush().unwrap();
            }
            Some("result") => {
                saw_denied_result = value["result"] == "FAKE_TTY_WRITE_DENIED";
                break;
            }
            _ => {}
        }
        line.clear();
    }
    drop(stdin);

    assert_eq!(
        permission_request_count, 1,
        "expected exactly one can_use_tool control_request"
    );
    assert!(
        saw_denied_result,
        "expected cctty to select the file prompt's third No choice"
    );
    let status = child
        .wait_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("cctty did not exit after stdin closed");
        });
    assert!(status.success());
}

#[test]
fn stream_json_permission_prompt_stdio_prefers_transcript_tool_use_over_tty_description() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000006";
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
        r#"{{"type":"user","message":{{"role":"user","content":"USE_FAKE_TOOL_WITH_TTY_DESCRIPTION"}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut permission_request_count = 0;
    let mut saw_allowed_result = false;
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap() > 0 {
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        match value.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                permission_request_count += 1;
                assert_eq!(
                    value["request"]["input"]["command"],
                    Value::String("printf fake-token".to_owned())
                );
                assert_eq!(
                    value["request"]["input"]["description"],
                    Value::String("Print test string".to_owned())
                );
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
                                "behavior": "allow"
                            }
                        }
                    })
                )
                .unwrap();
                stdin.flush().unwrap();
            }
            Some("result") => {
                saw_allowed_result = value["result"] == "FAKE_TOOL_WITH_DESCRIPTION_ALLOWED";
                break;
            }
            _ => {}
        }
        line.clear();
    }
    drop(stdin);

    assert_eq!(
        permission_request_count, 1,
        "expected exactly one can_use_tool control_request"
    );
    assert!(
        saw_allowed_result,
        "expected allowed fake tool-with-description result"
    );
    let status = child
        .wait_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("cctty did not exit after stdin closed");
        });
    assert!(status.success());
}

#[test]
fn stream_json_permission_prompt_stdio_handles_exit_plan_mode_menu() {
    let fixture = FakeClaude::new();
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let session_id = "00000000-0000-0000-0000-000000000020";
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
            "--permission-mode",
            "plan",
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
        r#"{{"type":"user","message":{{"role":"user","content":"USE_FAKE_PLAN_MODE"}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut permission_request_count = 0;
    let mut saw_plan_result = false;
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap() > 0 {
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        match value.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                permission_request_count += 1;
                assert_eq!(
                    value["request"]["subtype"],
                    Value::String("can_use_tool".to_owned())
                );
                assert_eq!(
                    value["request"]["tool_name"],
                    Value::String("ExitPlanMode".to_owned())
                );
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
                                "updated_input": { "_targetMode": "default" }
                            }
                        }
                    })
                )
                .unwrap();
                stdin.flush().unwrap();
            }
            Some("result") => {
                saw_plan_result = value["result"] == "FAKE_PLAN_MANUAL_ALLOWED";
                break;
            }
            _ => {}
        }
        line.clear();
    }
    drop(stdin);

    assert_eq!(
        permission_request_count, 1,
        "expected only ExitPlanMode can_use_tool request; internal plan Write should be skipped"
    );
    assert!(
        saw_plan_result,
        "expected cctty to select manual plan approval"
    );
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
