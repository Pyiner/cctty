use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

mod support;
use support::FakeClaude;
use wait_timeout::ChildExt;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

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

#[test]
#[ignore = "downloads Python Claude Agent SDK and spends real Claude calls"]
fn live_python_sdk_builds_game_with_cctty_permissions() {
    if std::env::var("CCTTY_LIVE_SDK_GAME").ok().as_deref() != Some("1") {
        eprintln!("set CCTTY_LIVE_SDK_GAME=1 to run live SDK game tests");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let venv = temp.path().join("venv");
    run(Command::new("python3").args(["-m", "venv"]).arg(&venv));
    run(Command::new(venv_bin(&venv, "python")).args([
        "-m",
        "pip",
        "install",
        "claude-agent-sdk==0.2.82",
    ]));

    let workspace = tempfile::tempdir().unwrap();
    write_live_game_settings(workspace.path());
    let script = temp.path().join("python_live_game.py");
    fs::write(
        &script,
        r#"
import asyncio
import json
import os
from pathlib import Path

from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    PermissionResultAllow,
    PermissionResultDeny,
    TextBlock,
    query,
)

WORKSPACE = Path(os.environ["WORKSPACE"])
approval_events = []

async def can_use_tool(tool_name, input, context):
    approval_events.append({
        "tool": tool_name,
        "input": input,
        "tool_use_id": context.tool_use_id,
    })
    if tool_name in {"Read", "Write", "Edit", "MultiEdit"}:
        return PermissionResultAllow()
    if tool_name == "Bash":
        command = str(input.get("command", "")).strip()
        safe_prefixes = ("pwd", "ls", "cat ", "test ", "find .")
        if command.startswith(safe_prefixes) and all(token not in command for token in [">", ">>", "|", ";", "&&"]):
            return PermissionResultAllow()
    return PermissionResultDeny(message=f"{tool_name} is not allowed in this live SDK probe")

async def main():
    options = ClaudeAgentOptions(
        cli_path=os.environ["CCTTY_BIN"],
        cwd=WORKSPACE,
        permission_mode="default",
        model="sonnet",
        effort="low",
        max_turns=6,
        setting_sources=["project", "local"],
        can_use_tool=can_use_tool,
        hooks={"Stop": []},
        extra_args={"no-chrome": None},
    )
    prompt = """
Create a tiny browser mini-game in this empty directory.
Write exactly these two files:
- index.html: a complete standalone HTML/CSS/JavaScript canvas game where the player moves with arrow keys, collects coins, avoids one enemy, sees score, and can restart.
- README.md: concise local run instructions.
Constraints: no external assets, no package manager, no shell commands, no extra files.
After writing the files, reply exactly GAME_READY.
"""
    text = ""
    async def input_stream():
        yield {
            "type": "user",
            "message": {
                "role": "user",
                "content": prompt,
            },
        }

    async for message in query(prompt=input_stream(), options=options):
        if isinstance(message, AssistantMessage):
            for block in message.content:
                if isinstance(block, TextBlock):
                    text += block.text

    index_path = WORKSPACE / "index.html"
    readme_path = WORKSPACE / "README.md"
    index = index_path.read_text(encoding="utf-8")
    readme = readme_path.read_text(encoding="utf-8")
    if "<canvas" not in index or "addEventListener" not in index or "score" not in index.lower():
        raise SystemExit(f"index.html does not look like a playable canvas game: {index[:300]!r}")
    if "index.html" not in readme:
        raise SystemExit(f"README.md missing run instructions: {readme[:300]!r}")
    if not any(event["tool"] in {"Write", "Edit", "MultiEdit"} for event in approval_events):
        raise SystemExit(f"expected write/edit permission callback, got {approval_events!r}")
    if any(event["tool"] not in {"Read", "Write", "Edit", "MultiEdit", "Bash"} for event in approval_events):
        raise SystemExit(f"unexpected tool approval events: {approval_events!r}")
    print(json.dumps({
        "sdk": "python",
        "approval_count": len(approval_events),
        "approval_tools": sorted({event["tool"] for event in approval_events}),
        "assistant_text": text[-120:],
    }))

asyncio.run(main())
"#,
    )
    .unwrap();

    let stdout = run_with_timeout(
        Command::new(venv_bin(&venv, "python"))
            .arg(script)
            .env("CCTTY_BIN", env!("CARGO_BIN_EXE_cctty"))
            .env("CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK", "1")
            .env("WORKSPACE", workspace.path())
            .env_remove("CCTTY_CLAUDE_PATH"),
        Duration::from_secs(240),
    );
    assert!(stdout.contains("\"sdk\": \"python\""), "stdout: {stdout}");
    assert_game_workspace(workspace.path());
}

#[test]
#[ignore = "downloads TypeScript Claude Agent SDK and spends real Claude calls"]
fn live_typescript_sdk_builds_game_with_cctty_permissions() {
    if std::env::var("CCTTY_LIVE_SDK_GAME").ok().as_deref() != Some("1") {
        eprintln!("set CCTTY_LIVE_SDK_GAME=1 to run live SDK game tests");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("ts-live-project");
    fs::create_dir_all(&project).unwrap();
    run(Command::new("npm")
        .arg("init")
        .arg("-y")
        .current_dir(&project));
    run(Command::new("npm")
        .args(["install", "@anthropic-ai/claude-agent-sdk@0.3.144"])
        .current_dir(&project));

    let workspace = tempfile::tempdir().unwrap();
    write_live_game_settings(workspace.path());
    let script = project.join("ts_live_game.mjs");
    fs::write(
        &script,
        r#"
import fs from 'node:fs';
import path from 'node:path';
import { query } from '@anthropic-ai/claude-agent-sdk';

const workspace = process.env.WORKSPACE;
const approvalEvents = [];

const canUseTool = async (toolName, input, options) => {
  approvalEvents.push({ tool: toolName, input, toolUseID: options.toolUseID });
  if (['Read', 'Write', 'Edit', 'MultiEdit'].includes(toolName)) {
    return { behavior: 'allow' };
  }
  if (toolName === 'Bash') {
    const command = String(input.command ?? '').trim();
    const safePrefixes = ['pwd', 'ls', 'cat ', 'test ', 'find .'];
    if (safePrefixes.some((prefix) => command.startsWith(prefix)) &&
        !['>', '>>', '|', ';', '&&'].some((token) => command.includes(token))) {
      return { behavior: 'allow' };
    }
  }
  return { behavior: 'deny', message: `${toolName} is not allowed in this live SDK probe` };
};

const prompt = `
Create a tiny browser mini-game in this empty directory.
Write exactly these two files:
- index.html: a complete standalone HTML/CSS/JavaScript canvas game where the player moves with arrow keys, collects coins, avoids one enemy, sees score, and can restart.
- README.md: concise local run instructions.
Constraints: no external assets, no package manager, no shell commands, no extra files.
After writing the files, reply exactly GAME_READY.
`;

async function* inputStream() {
  yield {
    type: 'user',
    message: {
      role: 'user',
      content: prompt,
    },
  };
}

let text = '';
for await (const message of query({
  prompt: inputStream(),
  options: {
    pathToClaudeCodeExecutable: process.env.CCTTY_BIN,
    cwd: workspace,
    permissionMode: 'default',
    model: 'sonnet',
    effort: 'low',
    maxTurns: 6,
    settingSources: ['project', 'local'],
    canUseTool,
    hooks: { Stop: [] },
    extraArgs: { 'no-chrome': null },
  },
})) {
  if (message.type === 'assistant') {
    for (const block of message.message.content) {
      if (block.type === 'text') text += block.text;
    }
  }
}

const index = fs.readFileSync(path.join(workspace, 'index.html'), 'utf8');
const readme = fs.readFileSync(path.join(workspace, 'README.md'), 'utf8');
if (!index.includes('<canvas') || !index.includes('addEventListener') || !index.toLowerCase().includes('score')) {
  throw new Error(`index.html does not look like a playable canvas game: ${index.slice(0, 300)}`);
}
if (!readme.includes('index.html')) {
  throw new Error(`README.md missing run instructions: ${readme.slice(0, 300)}`);
}
if (!approvalEvents.some((event) => ['Write', 'Edit', 'MultiEdit'].includes(event.tool))) {
  throw new Error(`expected write/edit permission callback, got ${JSON.stringify(approvalEvents)}`);
}
if (approvalEvents.some((event) => !['Read', 'Write', 'Edit', 'MultiEdit', 'Bash'].includes(event.tool))) {
  throw new Error(`unexpected tool approval events: ${JSON.stringify(approvalEvents)}`);
}
console.log(JSON.stringify({
  sdk: 'typescript',
  approval_count: approvalEvents.length,
  approval_tools: [...new Set(approvalEvents.map((event) => event.tool))].sort(),
  assistant_text: text.slice(-120),
}));
"#,
    )
    .unwrap();

    let stdout = run_with_timeout(
        Command::new("node")
            .arg(script)
            .current_dir(&project)
            .env("CCTTY_BIN", env!("CARGO_BIN_EXE_cctty"))
            .env("CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK", "1")
            .env("WORKSPACE", workspace.path())
            .env_remove("CCTTY_CLAUDE_PATH"),
        Duration::from_secs(240),
    );
    assert!(
        stdout.contains("\"sdk\":\"typescript\""),
        "stdout: {stdout}"
    );
    assert_game_workspace(workspace.path());
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

fn run_with_timeout(command: &mut Command, timeout: Duration) -> String {
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let child_id = child.id();
    let status = child.wait_timeout(timeout).unwrap_or_else(|error| {
        terminate_process_group(child_id);
        panic!("failed waiting for command: {error}");
    });
    terminate_process_group(child_id);
    let _ = child.wait();

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout).unwrap();
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr).unwrap();
    }

    let status = status.unwrap_or_else(|| {
        panic!("command timed out after {timeout:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
    });
    assert!(
        status.success(),
        "command failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
}

#[cfg(unix)]
fn terminate_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_pid: u32) {}

fn write_live_game_settings(workspace: &Path) {
    let claude_dir = workspace.join(".claude");
    fs::create_dir(&claude_dir).unwrap();
    fs::write(
        claude_dir.join("settings.local.json"),
        serde_json::json!({
            "permissions": {
                "ask": ["Write", "Edit", "MultiEdit"],
                "defaultMode": "default",
                "disableAutoMode": "disable"
            },
            "disableAllHooks": true
        })
        .to_string(),
    )
    .unwrap();
}

fn assert_game_workspace(workspace: &Path) {
    let index = fs::read_to_string(workspace.join("index.html")).unwrap();
    let readme = fs::read_to_string(workspace.join("README.md")).unwrap();
    assert!(index.contains("<canvas"), "index.html missing canvas");
    assert!(
        index.contains("addEventListener"),
        "index.html missing keyboard event listener"
    );
    assert!(
        index.to_ascii_lowercase().contains("score"),
        "index.html missing score"
    );
    assert!(
        readme.contains("index.html"),
        "README.md missing run command"
    );
}

fn venv_bin(venv: &Path, name: &str) -> std::path::PathBuf {
    venv.join("bin").join(name)
}
