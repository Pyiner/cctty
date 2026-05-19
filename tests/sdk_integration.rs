use std::fs;
use std::path::Path;
use std::process::Command;

mod support;
use support::FakeClaude;

#[test]
#[ignore = "downloads Python Claude Agent SDK and runs it against cctty"]
fn python_sdk_query_works_with_cctty() {
    if std::env::var("CCTTY_SDK_INTEGRATION").ok().as_deref() != Some("1") {
        eprintln!("set CCTTY_SDK_INTEGRATION=1 to run SDK integration tests");
        return;
    }

    let fake_claude = FakeClaude::new();
    let temp = tempfile::tempdir().unwrap();
    let venv = temp.path().join("venv");
    run(Command::new("python3").args(["-m", "venv"]).arg(&venv));
    run(Command::new(venv_bin(&venv, "python")).args([
        "-m",
        "pip",
        "install",
        "claude-agent-sdk==0.2.82",
    ]));

    let script = temp.path().join("python_sdk_probe.py");
    fs::write(
        &script,
        r#"
import asyncio
import os
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ClaudeAgentOptions, TextBlock, query

async def main():
    options = ClaudeAgentOptions(
        cli_path=os.environ["CCTTY_BIN"],
        cwd=Path(os.environ["WORKSPACE"]),
        permission_mode="bypassPermissions",
        max_turns=1,
        setting_sources=["project"],
    )
    text = ""
    async for message in query(prompt="Say SDK_OK", options=options):
        if isinstance(message, AssistantMessage):
            for block in message.content:
                if isinstance(block, TextBlock):
                    text += block.text
    if "FAKE_RESPONSE: Say SDK_OK" not in text:
        raise SystemExit(f"missing fake response: {text!r}")

asyncio.run(main())
"#,
    )
    .unwrap();

    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    run(Command::new(venv_bin(&venv, "python"))
        .arg(script)
        .env("CCTTY_BIN", env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fake_claude.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK", "1")
        .env("WORKSPACE", workspace.path()));
}

#[test]
#[ignore = "downloads TypeScript Claude Agent SDK and runs it against cctty"]
fn typescript_sdk_query_works_with_cctty() {
    if std::env::var("CCTTY_SDK_INTEGRATION").ok().as_deref() != Some("1") {
        eprintln!("set CCTTY_SDK_INTEGRATION=1 to run SDK integration tests");
        return;
    }

    let fake_claude = FakeClaude::new();
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("ts-project");
    fs::create_dir_all(&project).unwrap();
    run(Command::new("npm")
        .arg("init")
        .arg("-y")
        .current_dir(&project));
    run(Command::new("npm")
        .args(["install", "@anthropic-ai/claude-agent-sdk@0.3.144"])
        .current_dir(&project));

    let script = project.join("ts_sdk_probe.mjs");
    fs::write(
        &script,
        r#"
import { query } from '@anthropic-ai/claude-agent-sdk';

let text = '';
for await (const message of query({
  prompt: 'Say SDK_OK',
  options: {
    pathToClaudeCodeExecutable: process.env.CCTTY_BIN,
    cwd: process.env.WORKSPACE,
    permissionMode: 'bypassPermissions',
    maxTurns: 1,
    settingSources: ['project'],
  },
})) {
  if (message.type === 'assistant') {
    for (const block of message.message.content) {
      if (block.type === 'text') text += block.text;
    }
  }
}
if (!text.includes('FAKE_RESPONSE: Say SDK_OK')) {
  throw new Error(`missing fake response: ${text}`);
}
"#,
    )
    .unwrap();

    let workspace = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    run(Command::new("node")
        .arg(script)
        .current_dir(&project)
        .env("CCTTY_BIN", env!("CARGO_BIN_EXE_cctty"))
        .env("CCTTY_CLAUDE_PATH", fake_claude.path())
        .env("CLAUDE_CONFIG_DIR", config_dir.path())
        .env("CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK", "1")
        .env("WORKSPACE", workspace.path()));
}

fn run(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn venv_bin(venv: &Path, name: &str) -> std::path::PathBuf {
    venv.join("bin").join(name)
}
