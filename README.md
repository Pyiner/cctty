# cctty

`cctty` is a Claude CLI replacement for non-interactive SDK usage.

The goal is drop-in compatibility, but this repository does not claim full
Claude CLI parity yet. The table below is the compatibility contract for the
current implementation.

Normal interactive commands, `--help`, and `--version` are proxied to the real
`claude` binary. The `--print` / `--input-format stream-json` path is handled by
starting interactive Claude in a PTY, submitting prompts with bracketed paste,
tailing Claude's JSONL transcript, and emitting `text`, `json`, or
`stream-json` output.

This keeps native Claude Agent SDK callers on their normal subprocess protocol
while avoiding direct use of Claude's non-interactive execution path.

## Usage

```sh
cctty --print --output-format stream-json "Reply OK"
```

By default `cctty` finds `claude` on `PATH`. To point at a specific underlying
Claude binary:

```sh
CCTTY_CLAUDE_PATH=/path/to/claude cctty -p "Reply OK"
```

TypeScript SDK:

```ts
import { query } from "@anthropic-ai/claude-agent-sdk";

for await (const message of query({
  prompt: "Reply OK",
  options: { pathToClaudeCodeExecutable: "/path/to/cctty" },
})) {
  console.log(message);
}
```

Python SDK:

```py
from claude_agent_sdk import ClaudeAgentOptions, query

options = ClaudeAgentOptions(cli_path="/path/to/cctty")
async for message in query(prompt="Reply OK", options=options):
    print(message)
```

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

## Compatibility Matrix

Captured from `claude --help` on Claude Code `2.1.144`.

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
| `--include-partial-messages` | Unsupported | Consumed by `cctty`; no partial assistant chunks are emitted because transcript tailing only sees persisted messages. | Parser coverage marks it consumed. |
| `--input-format` | Supported | `text` prompts are read from argv/stdin. `stream-json` SDK input is read from stdin. | Fake-PTY test, Python SDK test, TypeScript SDK test. |
| `--json-schema` | Partial | Forwarded, but `cctty` synthesizes result frames when interactive transcript lacks one; `structured_output` parity is not proven. | Parser coverage only. Needs structured-output differential. |
| `--max-budget-usd` | Partial | Forwarded, but Claude documents this as print-only. Underlying interactive behavior is not proven equivalent. | Parser coverage only. |
| `--mcp-config` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. SDK MCP control messages are not bridged yet. |
| `--mcp-debug` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--model` | Pass-through | Forwarded to interactive Claude. | Parser coverage and live differential with default configured model path. Specific model aliases not exhaustively tested. |
| `-n`, `--name` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--no-chrome` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--no-session-persistence` | Supported | Consumed by `cctty`. The underlying interactive run uses the normal Claude config/auth, then `cctty` removes the generated transcript and empty project directories after the run. | Parser coverage plus fake-PTY persistence cleanup test. This preserves auth better than replacing `CLAUDE_CONFIG_DIR`. |
| `--output-format` | Partial | `text`, `json`, and `stream-json` are emitted by `cctty`. `stream-json` includes transcript frames plus a synthetic `result` frame if interactive Claude did not write one. | Fake-PTY and live stream-json differential pass. Result metadata is partial. |
| `--permission-mode` | Partial | Forwarded to interactive Claude for all documented modes: `acceptEdits`, `auto`, `bypassPermissions`, `default`, `dontAsk`, `plan`. SDK permission callbacks are bridged when the caller also supplies hidden `--permission-prompt-tool stdio`. | Parser coverage for all modes plus fake-PTY argv capture for all modes. Live differential covers `bypassPermissions`; live permission coverage also exercises `default` with a project-local `permissions.ask` rule. Other modes still need focused live tests. |
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
| `--strict-mcp-config` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--system-prompt` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--tmux` | Pass-through | Forwarded. `--tmux=classic` is preserved as an equals-form flag; plain `--tmux` does not swallow the prompt. | Parser regression test. |
| `--tools` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |
| `--verbose` | Pass-through | Forwarded. `cctty` itself does not require it for stream-json, but real Claude does, so SDK callers usually include it. | Parser and live differential coverage. |
| `-v`, `--version` | Supported | Entire command is proxied to real Claude. | Fake proxy test covers `--version`. |
| `-w`, `--worktree` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. |

### SDK / Hidden Flag Compatibility

Some SDKs pass flags that are not listed in current `claude --help`.

| Option(s) | Status | Current handling | Notes |
| --- | --- | --- | --- |
| `--permission-prompt-tool stdio` | Partial | Consumed by `cctty`, not forwarded to interactive Claude. In `stream-json` mode, `cctty` watches transcript `assistant.tool_use` entries and also recognizes real TTY permission forms when Claude has not persisted the transcript yet. It emits SDK-style `control_request` / `can_use_tool`, waits for the matching `control_response`, then drives the interactive permission UI by keyboard: allow confirms the selected approval row; deny selects menu item `2` and pastes the SDK denial message into Claude's follow-up form when present. If interactive Claude returns to the prompt after a rejected tool without writing a final result, `cctty` emits a synthetic error result with `result: "Permission denied"`. | Fake-PTY tests cover transcript-first allow/deny, TTY-form-before-transcript, and transcript-vs-TTY description precedence. Live Claude Code `2.1.144` coverage forces `Bash(printf:*)` approval with project-local settings and verifies both allow and deny. Still partial: non-Bash TTY forms, exact `permission_suggestions`, and exact `blocked_path` parity are not complete. |
| `--permission-prompt-tool <name>` | Pass-through | Non-`stdio` values are forwarded to interactive Claude. `cctty` does not emulate custom permission prompt tools itself. | Parser coverage only. |
| `--system-prompt-file` | Pass-through | Forwarded to interactive Claude. | Parser coverage because SDKs/older CLIs may emit it. |
| `--task-budget`, `--max-thinking-tokens`, `--thinking`, `--thinking-display`, `--managed-settings`, `--resume-session-at` | Pass-through | Forwarded to interactive Claude. | Parser coverage only. These are SDK/newer-CLI compatibility entries, not from the captured help output above. |

### Current High-Risk Gaps

- Permission callbacks now have fake-PTY allow/deny coverage and a live
  `Bash(printf:*)` approval differential for both allow and deny. Remaining
  risk is breadth: non-Bash permission forms, `allowedTools`/`disallowedTools`
  interactions, and exact SDK metadata fields still need live coverage.
- `--include-partial-messages` is not implemented because transcript tailing does
  not expose partial assistant chunks.
- `--json-schema`, `--max-budget-usd`, and `--fallback-model` need focused live
  differentials before being marked supported.
- SDK control requests beyond `initialize` and `interrupt` are currently
  reported as unsupported rather than silently faked.
