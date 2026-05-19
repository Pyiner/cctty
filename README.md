# cctty

`cctty` is a Claude CLI replacement for non-interactive SDK usage.

The binary accepts Claude-style arguments. Normal interactive commands, `--help`,
and `--version` are proxied to the real `claude` binary. The `--print` /
`--input-format stream-json` path is handled by starting interactive Claude in a
PTY, submitting prompts with bracketed paste, tailing Claude's JSONL transcript,
and emitting `text`, `json`, or `stream-json` output.

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

## Compatibility Notes

The target contract is CLI replacement parity: every Claude argument should be
accepted, and print-mode behavior should be checked through differential tests.
The current implementation covers the core SDK query path, stream JSON input,
text/json/stream-json output, session IDs, resume transcript lookup, and
pass-through of non-print commands.

Advanced SDK control requests beyond `initialize` and `interrupt` are currently
reported as unsupported rather than silently faked. Those should be added with
focused differential tests as the next compatibility slices.
