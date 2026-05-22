# cctty

**Use Claude Code through a real terminal while keeping the official Python and
TypeScript Claude Agent SDKs.**

`cctty` is a drop-in `claude` executable replacement for Claude Agent SDK apps.
It launches the interactive Claude Code CLI inside a real PTY, drives the
terminal the way a person would, and translates the session back into
SDK-compatible `stream-json` messages.

If the non-interactive Claude Code or Claude Agent SDK execution path becomes
separately billed, unavailable, restricted, too expensive, or behaviorally
different from the terminal experience, `cctty` gives you a practical fallback:
keep the official Python or TypeScript SDK, but run the actual Claude Code work
through the interactive terminal.

In other words: **Claude Code SDK compatibility, powered by the interactive
Claude Code terminal.**

## Why cctty?

Claude Code has two very different surfaces:

- the terminal UI that humans use interactively;
- the non-interactive SDK/`stream-json` path that agent applications launch.

`cctty` bridges those surfaces. It starts interactive Claude Code in a TTY,
submits prompts with bracketed paste, watches Claude's transcript, handles
keyboard-driven permission forms, and emits the messages that the official SDKs
expect.

Use `cctty` when you want:

- **A practical answer to separate Claude Agent SDK billing.** Keep the SDK
  integration surface, but execute the work through interactive Claude Code in a
  terminal session.
- **A Claude Code SDK alternative without leaving the official SDKs.** Keep
  using `claude-agent-sdk` for Python or `@anthropic-ai/claude-agent-sdk` for
  TypeScript. `cctty` replaces only the executable path.
- **Terminal-native Claude Code behavior.** The work is done by the interactive
  `claude` CLI inside a PTY, not by reimplementing Claude Code.
- **Permission callbacks that still work.** SDK `can_use_tool` approvals are
  bridged to Claude's keyboard-driven TTY permission forms for Bash and file
  writes.
- **A testable compatibility contract.** Every captured `claude --help` flag is
  listed below with support status, known gaps, and test coverage.
- **A real SDK smoke test path.** The live suite asks both Python and
  TypeScript SDKs to build a browser mini-game under `permissionMode: "default"`
  and verifies the files are created through SDK approvals.

## Drop-in SDK Replacement

For most SDK apps, only one thing changes: the Claude executable path.

One executable path:

```diff
- cli_path="/path/to/claude"
+ cli_path="/path/to/cctty"
```

or, in TypeScript:

```diff
- pathToClaudeCodeExecutable: "/path/to/claude"
+ pathToClaudeCodeExecutable: "/path/to/cctty"
```

Everything else stays on the official SDK: streaming messages, permission
callbacks, `permissionMode`, `maxTurns`, `model`, `settingSources`, and the rest
of the SDK surface continue to flow through the SDK you already use.

`cctty` is not affiliated with Anthropic. It still requires a locally installed
and authenticated Claude Code CLI.

## Install

### Homebrew

Install from the official tap:

```sh
brew install Pyiner/cctty/cctty
```

or tap once and install normally:

```sh
brew tap Pyiner/cctty
brew install cctty
```

The tap lives at
[`Pyiner/homebrew-cctty`](https://github.com/Pyiner/homebrew-cctty).

### Release Binary

Download the archive for your platform from GitHub Releases:

```sh
curl -L -o cctty.tar.gz \
  https://github.com/Pyiner/cctty/releases/download/v0.1.0/cctty-0.1.0-aarch64-apple-darwin.tar.gz
tar -xzf cctty.tar.gz
sudo install -m 0755 cctty /usr/local/bin/cctty
```

Published release targets:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-gnu`

### From Source

```sh
cargo install --git https://github.com/Pyiner/cctty
```

## Quick Start

First confirm the underlying Claude CLI is installed and authenticated:

```sh
claude --version
cctty --version
```

Run a direct CLI smoke test:

```sh
cctty --print --output-format stream-json "Reply exactly CCTTY_OK"
```

By default `cctty` finds `claude` on `PATH`. To point at a specific underlying
Claude binary:

```sh
CCTTY_CLAUDE_PATH=/path/to/claude cctty -p "Reply OK"
```

## Diagnostics

`cctty` writes a small local diagnostic log by default. It records process
startup, resolved Claude path, SDK control requests, permission decisions,
synthetic result/idle events, and timeout summaries. It does not log full prompt
text by default.

Default paths:

- macOS: `~/Library/Logs/cctty/cctty.log`
- Linux: `${XDG_STATE_HOME:-~/.local/state}/cctty/cctty.log`

Controls:

```sh
CCTTY_LOG_FILE=/tmp/cctty.log cctty --version
CCTTY_LOG=0 cctty --version
CCTTY_LOG_TTY=1 CCTTY_LOG_FILE=/tmp/cctty.log cctty -p "Reply OK"
```

`CCTTY_LOG_TTY=1` also records recent visible TTY text around waits/timeouts.
Use it only for local debugging because that text can include prompts and model
output.

## Replacement Boundary

`cctty` replaces the `claude` executable used by the Claude Agent SDK and
`claude -p` style non-interactive flows. It does **not** replace higher-level
wrappers such as ACP servers, editor adapters, or project-specific agent
runtimes.

That boundary is intentional:

- If an app exposes `cli_path`, `pathToClaudeCodeExecutable`, or
  `CLAUDE_CODE_EXECUTABLE`, point that setting at `cctty`.
- If an app is an ACP adapter, keep the ACP adapter in place and configure its
  internal Claude executable path to `cctty` when it offers one.
- If a wrapper hard-codes `claude`, use a PATH shim or ask the wrapper to expose
  the executable-path option.
- `cctty --acp --stdio` is not a supported interface. `--acp` is not a current
  Claude Code CLI option; it belongs to third-party wrappers that sit above
  Claude Code.

## TypeScript SDK

Install the official SDK:

```sh
npm install @anthropic-ai/claude-agent-sdk
```

Use `cctty` as the SDK executable:

```ts
import { query } from "@anthropic-ai/claude-agent-sdk";

for await (const message of query({
  prompt: "Create a tiny README for this project.",
  options: {
    pathToClaudeCodeExecutable: "cctty",
    permissionMode: "default",
    settingSources: ["project", "local"],
  },
})) {
  console.log(message);
}
```

With a permission callback:

```ts
import { query } from "@anthropic-ai/claude-agent-sdk";

const canUseTool = async (toolName: string, input: Record<string, unknown>) => {
  if (["Read", "Write", "Edit", "MultiEdit"].includes(toolName)) {
    return { behavior: "allow" as const };
  }

  if (toolName === "Bash") {
    const command = String(input.command ?? "");
    if (command === "pwd" || command.startsWith("ls ")) {
      return { behavior: "allow" as const };
    }
  }

  return {
    behavior: "deny" as const,
    message: `${toolName} is not allowed by this app`,
  };
};

async function* prompt() {
  yield {
    type: "user" as const,
    message: {
      role: "user" as const,
      content: "Write index.html for a tiny canvas game.",
    },
  };
}

for await (const message of query({
  prompt: prompt(),
  options: {
    pathToClaudeCodeExecutable: "cctty",
    permissionMode: "default",
    canUseTool,
    settingSources: ["project", "local"],
  },
})) {
  console.log(message);
}
```

## Python SDK

Install the official SDK:

```sh
pip install claude-agent-sdk
```

Use `cctty` as the SDK executable:

```py
import asyncio
from pathlib import Path

from claude_agent_sdk import ClaudeAgentOptions, query


async def main():
    options = ClaudeAgentOptions(
        cli_path="cctty",
        cwd=Path.cwd(),
        permission_mode="default",
        setting_sources=["project", "local"],
    )

    async for message in query(
        prompt="Create a tiny README for this project.",
        options=options,
    ):
        print(message)


asyncio.run(main())
```

With a permission callback:

```py
import asyncio
from pathlib import Path

from claude_agent_sdk import (
    ClaudeAgentOptions,
    PermissionResultAllow,
    PermissionResultDeny,
    query,
)


async def can_use_tool(tool_name, input, context):
    if tool_name in {"Read", "Write", "Edit", "MultiEdit"}:
        return PermissionResultAllow()

    if tool_name == "Bash":
        command = str(input.get("command", ""))
        if command == "pwd" or command.startswith("ls "):
            return PermissionResultAllow()

    return PermissionResultDeny(message=f"{tool_name} is not allowed by this app")


async def prompt():
    yield {
        "type": "user",
        "message": {
            "role": "user",
            "content": "Write index.html for a tiny canvas game.",
        },
    }


async def main():
    options = ClaudeAgentOptions(
        cli_path="cctty",
        cwd=Path.cwd(),
        permission_mode="default",
        can_use_tool=can_use_tool,
        setting_sources=["project", "local"],
    )

    async for message in query(prompt=prompt(), options=options):
        print(message)


asyncio.run(main())
```

## Tested Open-Source Integrations

Surveyed and tested on 2026-05-21. The support target is always the same:
`cctty` replaces the underlying Claude executable/core, while the host project
keeps owning its own SDK, ACP, editor, or orchestration protocol.

| Project / package | Integration style | Status | Configure cctty | Tested coverage | Known gaps |
| --- | --- | --- | --- | --- | --- |
| `@anthropic-ai/claude-agent-sdk` | Official TypeScript Claude Agent SDK | Supported | `pathToClaudeCodeExecutable: "cctty"` | Deterministic SDK test with fake Claude; live mini-game write test under `permissionMode: "default"` with SDK `canUseTool` approvals; live direct SDK smoke through cctty. | Result metadata is synthesized when interactive Claude does not write a `result` frame. `--include-partial-messages` emits SDK-compatible text events after transcript persistence, not token-by-token partials. |
| `claude-agent-sdk` | Official Python Claude Agent SDK | Supported | `ClaudeAgentOptions(cli_path="cctty")` | Deterministic SDK test with fake Claude; live mini-game write test under `permission_mode="default"` with SDK `can_use_tool` approvals. | Same result metadata and synthetic partial-message limits as TypeScript. |
| Hermes Agent | OpenAI-like agent runtime with a Claude SDK provider | Supported on the cctty branch | Select provider `claude-sdk` or alias `cctty`; set `HERMES_CLAUDE_SDK_COMMAND=/path/to/cctty` or `CCTTY_CLI_PATH=/path/to/cctty`. | Hermes unit coverage for provider routing and a local `ClaudeSDKClient` smoke through cctty. `copilot-acp` remains separate. | Upstream release has to include the `claude-sdk` provider path; cctty does not implement Hermes' ACP adapter. |
| NanoClaw / NanoBot (`qwibitai/nanoclaw`) | Containerized TypeScript Agent SDK runner | Core supported; project integration needs a path override | Its runner passes `pathToClaudeCodeExecutable` to the SDK, but currently hard-codes `/pnpm/claude`. Patch NanoClaw to expose that setting, or install/symlink cctty at `/pnpm/claude` inside the agent image and set `CCTTY_CLAUDE_PATH` to the real Claude binary. | Source inspected at commit `0683c6e` / `nanoclaw@2.0.64`; NanoClaw-pinned `@anthropic-ai/claude-agent-sdk@0.2.138` smoke passed with cctty. | Full container/session runtime not run. Upstream should expose the executable path as config. |
| `@agentclientprotocol/claude-agent-acp` | ACP server powered by the TypeScript Claude Agent SDK | Supported as a core executable override | Set `CLAUDE_CODE_EXECUTABLE=/path/to/cctty` in the ACP adapter environment. | `0.36.1` smoke passed through `acpx`, including SDK initialization metadata, `includePartialMessages`, and idle session-state handling. | ACP behavior belongs to the adapter. cctty only supplies the Claude executable behind it. Dynamic `set_model` / `set_permission_mode` control requests are compatibility-acknowledged; the underlying TTY run still comes from the CLI args/settings used at launch. |
| `@zed-industries/claude-code-acp` | Deprecated ACP server powered by the TypeScript Claude Agent SDK | Supported as a core executable override | Set `CLAUDE_CODE_EXECUTABLE=/path/to/cctty` in the ACP adapter environment. | `0.16.2` smoke passed through `acpx`. npm marks this package deprecated in favor of `@agentclientprotocol/claude-agent-acp`. | Same boundary as above: keep the ACP adapter, replace only its Claude executable. |
| `acp-claude-code` | ACP bridge for Claude Code | Conditional | Set `ACP_PATH_TO_CLAUDE_CODE_EXECUTABLE=/path/to/cctty`. | `0.8.0` path override works. A smoke passed with the older SDK-exporting `@anthropic-ai/claude-code@1.0.128` dependency and fake Claude under cctty. | A fresh install currently resolves `@anthropic-ai/claude-code: latest` to a CLI-only package, so the wrapper can fail before cctty starts. This is a wrapper dependency issue, not an ACP feature cctty should implement. |
| `claude-code-acp` / `cc-acp` | Alternate ACP bridge for Claude Code | Unsupported without wrapper changes | No cctty setting found. | `0.1.1` source/package inspected. | The wrapper hard-resolves its bundled `@anthropic-ai/claude-code/cli.js` and ignores `CLAUDE_CODE_EXECUTABLE`; it needs an upstream executable-path option. |

Other surveyed SDK hosts generally fit the same rule: if the host exposes
`cli_path`, `pathToClaudeCodeExecutable`, `CLAUDE_CODE_EXECUTABLE`, or a similar
Claude executable-path option, point it at `cctty`. If it imports an SDK module
path, bundles a specific `cli.js`, or manages an interactive terminal itself,
the host needs a small upstream option before cctty can be selected cleanly.

## How It Works

Normal interactive commands, `--help`, and `--version` are proxied to the real
`claude` binary. The `--print` / `--input-format stream-json` path is handled by
starting interactive Claude in a PTY, submitting prompts with bracketed paste,
tailing Claude's JSONL transcript, and emitting `text`, `json`, or
`stream-json` output.

For SDK permission callbacks, `cctty` consumes the hidden
`--permission-prompt-tool stdio` flag, emits SDK-style `can_use_tool`
`control_request` messages, waits for SDK `control_response` decisions, and then
drives Claude's interactive permission UI with keyboard input.

Claude's interactive `AskUserQuestion` forms are bridged through the same SDK
control channel. When Claude asks a structured question, `cctty` forwards the
original `AskUserQuestion` tool input to the SDK caller when it is already in
the transcript. If Claude shows the terminal form before persisting that tool
call, `cctty` falls back to parsing the visible TTY form and still sends an
SDK-shaped `AskUserQuestion` request. If the caller returns
`updatedInput.answers`, `answers`, `content`, or similar structured form data,
`cctty` renders those answers as text and feeds them back into Claude's TTY. If
Claude opens a follow-up prompt, cctty writes there; if Claude returns to the
main prompt after cancelling the form, cctty submits the answers as the next
user message. SDK hosts can answer forms without pretending to click the
terminal UI.

## Tests

Fast deterministic tests use a fake interactive Claude binary:

```sh
cargo test
```

SDK integration tests download the official SDK packages and run them against
`cctty` with the fake interactive Claude underneath:

```sh
CCTTY_SDK_INTEGRATION=1 cargo test --test sdk_integration -- --ignored --nocapture
```

Live differential tests compare `claude --print` and `cctty --print` against the
real Claude CLI. These require local Claude auth and spend real Claude calls:

```sh
CCTTY_LIVE_CLAUDE_DIFF=1 cargo test --test claude_differential -- --ignored --nocapture
```

The focused live permission test forces a per-project Bash approval prompt with
a temporary `.claude/settings.local.json`, then verifies both SDK allow and deny
responses against the real interactive Claude TTY:

```sh
CCTTY_LIVE_PERMISSION=1 cargo test --test claude_differential live_permission_prompt_stdio_honors_project_ask_rules -- --ignored --nocapture
```

Live permission-mode smoke tests cover `plan`, `auto`, `dontAsk`, and
`acceptEdits`, including the first-run auto-mode consent form and an
`acceptEdits` file-write run:

```sh
CCTTY_LIVE_PERMISSION_MODES=1 cargo test --test claude_differential live_permission_modes_smoke_common_modes -- --ignored --nocapture
CCTTY_LIVE_PERMISSION_MODES=1 cargo test --test claude_differential live_accept_edits_writes_file_without_sdk_permission_callback -- --ignored --nocapture
```

Live SDK game tests install the official Python and TypeScript SDKs, run them
against real `cctty`/Claude, keep `permissionMode` at `default`, approve file
edits through SDK `can_use_tool` callbacks, and verify that a small browser game
is actually written:

```sh
CCTTY_LIVE_SDK_GAME=1 cargo test --test sdk_integration live_python_sdk_builds_game_with_cctty_permissions -- --ignored --nocapture
CCTTY_LIVE_SDK_GAME=1 cargo test --test sdk_integration live_typescript_sdk_builds_game_with_cctty_permissions -- --ignored --nocapture
```

## Release Flow

The repository includes GitHub Actions for CI and tagged releases.

```sh
git tag v0.1.0
git push origin v0.1.0
```

The release workflow builds macOS and Linux archives and publishes SHA-256
sums. If the repository secret `HOMEBREW_TAP_TOKEN` is configured with write
access to `Pyiner/homebrew-cctty`, the same workflow also updates the Homebrew
tap. The formula is not published as a release asset; users should install
through the tap.

## Compatibility Matrix

Captured from `claude --help` on Claude Code `2.1.148`.

Status legend:

- Supported: implemented by `cctty`, or proxied to real Claude with a passing
  test for the relevant path.
- Pass-through: forwarded to the underlying interactive Claude TTY. Parser
  coverage exists, but behavior parity still belongs to Claude and may need a
  live differential before users rely on it for SDK replacement.
- Partial: accepted, but output or behavior is known to differ from
  `claude --print`.
- Unsupported: accepted only as a no-op or not bridged yet. Do not rely on it.

| Option(s) | Status | Current handling in `cctty --print` | Test coverage / known difference |
| --- | --- | --- | --- |
| `--add-dir` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--agent`, `--agents` | Pass-through | Forwarded to interactive Claude. | Parser coverage plus fake-PTY argv capture. No live agent behavior differential yet. |
| `--allow-dangerously-skip-permissions` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--allowedTools`, `--allowed-tools` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. Permission behavior not live-tested. |
| `--append-system-prompt` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--bare` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. Bare auth/config behavior not differential-tested. |
| `--betas` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--brief` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--chrome` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `-c`, `--continue` | Partial | Forwarded. Transcript tail falls back to newest project transcript because no session id is known up front. | Parser coverage only. Needs live resume/continue differential. |
| `--dangerously-skip-permissions` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. Dangerous behavior is intentionally not exercised in default tests. |
| `-d`, `--debug` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--debug-file` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--disable-slash-commands` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--disallowedTools`, `--disallowed-tools` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. Tool-denial behavior not live-tested yet. |
| `--effort` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--exclude-dynamic-system-prompt-sections` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--fallback-model` | Partial | Forwarded, but Claude documents this as print-only. Since `cctty` runs underlying Claude interactively, parity is not proven. | Parser coverage only. Needs live overload/fallback strategy test. |
| `--file` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--fork-session` | Partial | Forwarded. `cctty` tails whichever transcript Claude writes. | Parser coverage only. Needs live resume/fork differential. |
| `--from-pr` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `-h`, `--help` | Supported | Entire command is proxied to real Claude. | Fake proxy test covers `--version`; README coverage test covers both aliases. |
| `--ide` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--include-hook-events` | Partial | Forwarded. Hook events appear only if interactive transcript writes them; stream semantics are not guaranteed to match `--print`. | Parser coverage only. |
| `--include-partial-messages` | Partial | Consumed by `cctty`. When transcript text arrives, `cctty` emits SDK-compatible synthetic `stream_event` text deltas before the persisted `assistant` frame so SDK wrappers that render partial messages can work. | Parser coverage plus fake-PTY stream-event test and live `@agentclientprotocol/claude-agent-acp` / `@zed-industries/claude-code-acp` smokes. It is not token-by-token live streaming; chunks arrive after transcript persistence. |
| `--input-format` | Supported | `text` prompts are read from argv/stdin. `stream-json` SDK input is read from stdin. | Fake-PTY test, Python SDK test, TypeScript SDK test, plus live Python/TypeScript SDK game tests. |
| `--json-schema` | Partial | Forwarded, but `cctty` synthesizes result frames when interactive transcript lacks one; `structured_output` parity is not proven. | Parser coverage only. Needs structured-output differential. |
| `--max-budget-usd` | Partial | Forwarded, but Claude documents this as print-only. Underlying interactive behavior is not proven equivalent. | Parser coverage only. |
| `--mcp-config` | Supported | Normal Claude-compatible stdio/SSE MCP configs are forwarded to interactive Claude. SDK in-process MCP servers are bridged: TypeScript `initialize.sdkMcpServers` and Python `{ "type": "sdk" }` entries are rewritten into a temporary stdio MCP proxy, and `mcp_message` control requests are round-tripped back to the SDK. | Parser coverage, fake-PTY argv passthrough for normal MCP configs, and TS/Python SDK-MCP round-trip tests covering `initialize`, `tools/list`, and `tools/call`. |
| `--mcp-debug` | Pass-through | Forwarded to interactive Claude. | Parser coverage plus fake-PTY argv passthrough test. |
| `--model` | Pass-through | Forwarded to interactive Claude. | Parser coverage and live differential with default configured model path. Specific model aliases not exhaustively tested. |
| `-n`, `--name` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--no-chrome` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--no-session-persistence` | Supported | Consumed by `cctty`. The underlying interactive run uses the normal Claude config/auth, then `cctty` removes the generated transcript and empty project directories after the run. | Parser coverage plus fake-PTY persistence cleanup test. This preserves auth better than replacing `CLAUDE_CONFIG_DIR`. |
| `--output-format` | Partial | `text`, `json`, and `stream-json` are emitted by `cctty`. `stream-json` includes transcript frames plus a synthetic `result` frame if interactive Claude did not write one. | Fake-PTY and live stream-json differential pass. Result metadata is partial. |
| `--permission-mode` | Supported | Forwarded to interactive Claude for all documented modes: `acceptEdits`, `auto`, `bypassPermissions`, `default`, `dontAsk`, `plan`. The first-run auto-mode consent form is handled by keyboard and chooses "enable auto mode" for the current run, not "make default". SDK permission callbacks are bridged when the caller also supplies hidden `--permission-prompt-tool stdio`. | Parser coverage for all modes plus fake-PTY argv capture for all modes. Live tests cover `plan`, `auto`, `dontAsk`, `acceptEdits`, `bypassPermissions`, and `default` with project-local `permissions.ask` rules. Live SDK game tests exercise file-write approvals through Python and TypeScript SDK callbacks. |
| `--plugin-dir` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--plugin-url` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `-p`, `--print` | Supported | Consumed by `cctty`; underlying Claude is intentionally launched interactively through a PTY. | Fake-PTY and live differential pass for basic query. |
| `--remote-control` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. Remote Control warnings may appear in transcript as system messages. |
| `--remote-control-session-name-prefix` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--replay-user-messages` | Partial | Consumed by `cctty`. User transcript frames may still be emitted, but exact replay semantics are not implemented separately. | Parser coverage marks it consumed. |
| `-r`, `--resume` | Partial | Forwarded. `cctty` tails the requested session transcript when a session id is supplied. | Parser coverage only. Needs live resume differential. |
| `--session-id` | Supported | Forwarded and used to locate the transcript. If omitted in print mode, `cctty` creates one. | Fake-PTY and live differential cover session-id based transcript tailing. |
| `--setting-sources` | Pass-through | Forwarded to interactive Claude. | Parser coverage, including `--setting-sources=project`. |
| `--settings` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--strict-mcp-config` | Pass-through | Forwarded to interactive Claude. | Parser coverage plus fake-PTY argv passthrough test. |
| `--system-prompt` | Pass-through | Non-empty values are forwarded to interactive Claude. Empty or whitespace-only SDK values are consumed so they do not erase Claude Code's built-in interactive system prompt. | Parser coverage for non-empty pass-through and empty-value consumption. |
| `--tmux` | Pass-through | Forwarded. `--tmux=classic` is preserved as an equals-form flag; plain `--tmux` does not swallow the prompt. | Parser regression test. |
| `--tools` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--verbose` | Pass-through | Forwarded. `cctty` itself does not require it for stream-json, but real Claude does, so SDK callers usually include it. | Parser and live differential coverage. |
| `-v`, `--version` | Supported | Entire command is proxied to real Claude. | Fake proxy test covers `--version`. |
| `-w`, `--worktree` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |

### SDK / Hidden Flag Compatibility

Some SDKs pass flags that are not listed in current `claude --help`.

| Option(s) | Status | Current handling | Notes |
| --- | --- | --- | --- |
| `--permission-prompt-tool`, `--permission-prompt-tool stdio` | Partial | `stdio` is consumed by `cctty`, not forwarded to interactive Claude. In `stream-json` mode, `cctty` watches transcript `assistant.tool_use` entries and also recognizes real TTY permission forms when Claude has not persisted the transcript yet. It emits SDK-style `control_request` / `can_use_tool`, waits for the matching `control_response`, then drives the interactive permission UI by keyboard. Bash allow confirms the selected row; Bash deny selects menu item `2` and pastes the SDK denial message into Claude's follow-up form when present. File create/write/edit/update forms are mapped to `Write`/`Edit` with `file_path`; allow confirms the selected row, while deny selects the file prompt's `3. No` row to avoid accidentally choosing "allow all edits during this session". `AskUserQuestion` form tool uses are forwarded with their original structured `questions` input when available; if only the TTY form is visible, cctty parses that form and sends an SDK-shaped fallback request. SDK-returned structured answers are textified and sent through Claude's follow-up prompt, or submitted as the next user message if Claude returns to the main prompt after cancelling the form. If interactive Claude returns to the prompt after a rejected tool without writing a final result, `cctty` emits a synthetic error result with `result: "Permission denied"`. | Fake-PTY tests cover transcript-first allow/deny, Bash TTY-form-before-transcript, file Write TTY-form-before-transcript, transcript-vs-TTY description precedence, `AskUserQuestion` structured form answer round-trip, and TTY-form-before-transcript `AskUserQuestion` fallback. Live Claude Code `2.1.148` coverage forces `Bash(printf:*)` approval with project-local settings and verifies both allow and deny; live `AskUserQuestion` smoke verifies TTY fallback, answer textification, main-prompt answer injection, and final `result`. Live Python and TypeScript SDK game tests exercise `Write` approval through real file-creation TTY forms. Still partial: broader `Edit`/`MultiEdit` TTY variants, exact `permission_suggestions`, and exact `blocked_path` parity are not complete. |
| `--permission-prompt-tool <name>` | Pass-through | Non-`stdio` values are forwarded to interactive Claude. `cctty` does not emulate custom permission prompt tools itself. | Parser coverage only. |
| `--max-turns`, `--task-budget`, `--max-thinking-tokens`, `--thinking`, `--thinking-display` | Pass-through | Forwarded to interactive Claude. These are emitted by official SDK thinking, budget, and turn-limit options. | Parser coverage only. Print-mode parity still belongs to real Claude. |
| `--system-prompt-file`, `--append-system-prompt-file`, `--managed-settings`, `--resume-session-at` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. These are SDK/newer-CLI compatibility entries, not from the captured public help output above. |
| `--session-mirror`, `--sdk-url` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. These are hidden SDK transport/session flags; cctty does not implement the remote SDK transport itself. |
| `--advisor`, `--channels`, `--dangerously-load-development-channels`, `--plan-mode-instructions`, `--plan-mode-required` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. Hidden/native Claude behavior is not differential-tested. |
| `--agent-id`, `--agent-name`, `--team-name`, `--agent-color`, `--agent-type`, `--parent-session-id`, `--teammate-mode` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. These hidden teammate/team flags are treated as native Claude-owned semantics. |
| `--remote`, `--rc`, `--teleport`, `--prefill`, `--prefill-b64`, `--rewind-files`, `--deep-link-cwd-b64`, `--deep-link-last-fetch`, `--deep-link-origin`, `--deep-link-repo`, `--enable-auth-status`, `--enable-auto-mode`, `--init`, `--init-only`, `--maintenance`, `--workload`, `--cowork`, `--xaa` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. These are hidden/native flags found in the installed Claude binary; users should prefer documented flags unless an SDK or host app emits them. |

### Current High-Risk Gaps

- Permission callbacks now have fake-PTY allow/deny coverage, a live
  `Bash(printf:*)` approval differential for both allow and deny, and live
  Python/TypeScript SDK coverage for file creation approvals. Permission modes
  `plan`, `auto`, `dontAsk`, `acceptEdits`, `bypassPermissions`, and `default`
  have focused live coverage. Remaining risk is breadth: more
  `Edit`/`MultiEdit` TTY variants, `allowedTools`/`disallowedTools`
  interactions, and exact SDK metadata fields still need live coverage.
- `--include-partial-messages` is compatibility-oriented, not true token-level
  streaming. cctty emits synthetic text deltas once transcript text is
  persisted; this unblocks SDK wrappers, but does not perfectly match Claude
  Code's live partial timing.
- `--json-schema`, `--max-budget-usd`, and `--fallback-model` need focused live
  differentials before being marked supported.
- SDK control requests used by popular wrappers are compatibility-acked:
  `initialize`, `interrupt`, `set_model`, `set_permission_mode`,
  `set_max_thinking_tokens`, `apply_flag_settings`, and `mcp_status`. Dynamic
  model/mode changes after a TTY session is already running are still partial;
  the actual Claude process behavior comes from its launch args and local
  settings.
