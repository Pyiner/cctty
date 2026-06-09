<p align="center">
  <img src="assets/cctty-banner.svg" alt="cctty: 通过真实终端运行 Claude Agent SDK" width="100%">
</p>

<h1 align="center">cctty</h1>

<p align="center">
  <strong>让 Claude Agent SDK 通过人类正在使用的那个 Claude Code 交互式终端工作。</strong>
</p>

<p align="center">
  <a href="README.md">English</a>
  ·
  <a href="#快速开始">快速开始</a>
  ·
  <a href="#sdk-接入">SDK 接入</a>
  ·
  <a href="#开源生态实测">开源生态实测</a>
  ·
  <a href="#兼容状态">兼容状态</a>
</p>

<p align="center">
  <a href="https://github.com/Pyiner/cctty/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/Pyiner/cctty/ci.yml?branch=master&label=ci&style=for-the-badge"></a>
  <a href="https://github.com/Pyiner/cctty/releases"><img alt="Release" src="https://img.shields.io/github/v/release/Pyiner/cctty?style=for-the-badge"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/license-MIT-57b500?style=for-the-badge"></a>
  <a href="https://github.com/Pyiner/homebrew-cctty"><img alt="Homebrew" src="https://img.shields.io/badge/homebrew-Pyiner%2Fcctty-fbb040?style=for-the-badge"></a>
</p>

`cctty` 是一个 `claude` 可执行文件替身，面向官方 Claude Agent SDK
使用者。你的应用继续使用官方 Python SDK 或 TypeScript SDK，只把 SDK
启动的 Claude Code 可执行文件路径换成 `cctty`。

当 Claude Code 的非交互式执行路径、Claude Agent SDK 路径被单独计费、
受限、不可用，或者和真实终端行为不一致时，`cctty` 提供一个务实的替代：

> **保留官方 Claude Agent SDK API；实际执行走交互式 Claude Code 终端。**

`cctty` 会启动本机已经登录的 Claude Code CLI，放进真实 PTY 里，像人一样
粘贴 prompt、观察终端输出、处理权限/表单，然后再把结果翻译成 SDK 需要的
`stream-json` 消息。

`cctty` 与 Anthropic 无关联。它不是 Claude Code 重写，也不是 SDK 分叉；它仍然
需要你本机已经安装并登录 Claude Code CLI。

## 为什么需要它

Claude Code 至少有两个重要入口：

- 人类直接使用的交互式终端；
- SDK、`claude -p`、`stream-json` 等非交互式路径。

很多 Agent 应用已经接入了官方 Claude Agent SDK。如果非交互式路径被单独收费、
受限或表现不稳定，完全重写 SDK 接入成本很高。`cctty` 的定位是只替换底层
`claude` 二进制，让上层仍然认为自己在和官方 SDK 协议交互。

适合 `cctty` 的场景：

| 场景 | cctty 怎么帮你 |
| --- | --- |
| 想继续使用官方 Python / TypeScript Agent SDK | 只改 `cli_path` 或 `pathToClaudeCodeExecutable`。 |
| SDK 非交互式路径开始单独计费或被限制 | 让实际工作通过交互式 Claude Code 终端完成。 |
| 需要权限审批 | 把 SDK `can_use_tool` 回调桥接到 Claude 终端里的键盘权限表单。 |
| 需要 Plan 模式 | 支持 `permissionMode: "plan"` / `--permission-mode plan`，并做过真实 Conductor 类场景验证。 |
| 需要 MCP | 原生 `--mcp-config` 透传给 Claude；SDK in-process MCP server 会通过临时 stdio MCP 代理桥接。 |
| 需要表单 / `AskUserQuestion` | SDK 结构化问题会转发；终端可见表单也会兜底解析。SDK 返回的答案会文本化后送回 Claude。 |
| 需要流式输入 | 支持 `--input-format stream-json` 和 SDK 多轮 stdin 流。 |

## 安装

### Homebrew

```sh
brew install Pyiner/cctty/cctty
```

或者先 tap：

```sh
brew tap Pyiner/cctty
brew install cctty
```

### 下载二进制

```sh
curl -L -o cctty.tar.gz \
  https://github.com/Pyiner/cctty/releases/download/v0.2.3/cctty-0.2.3-aarch64-apple-darwin.tar.gz
tar -xzf cctty.tar.gz
sudo install -m 0755 cctty /usr/local/bin/cctty
```

发布包包含：

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-musl`（Linux 静态构建，不要求目标机有较新的 glibc）

### 从源码安装

```sh
cargo install --git https://github.com/Pyiner/cctty
```

## 快速开始

先确认本机真实 Claude Code CLI 可以运行，并且已经登录：

```sh
claude --version
cctty --version
```

跑一个最小 smoke：

```sh
cctty --print --output-format stream-json "Reply exactly CCTTY_OK"
```

默认情况下，`cctty` 会从 `PATH` 找真实的 `claude`。如果你需要指定真实 Claude
路径：

```sh
CCTTY_CLAUDE_PATH=/path/to/claude cctty -p "Reply OK"
```

## SDK 接入

绝大多数 SDK 应用只需要改一个路径。

Python：

```diff
- cli_path="/path/to/claude"
+ cli_path="/path/to/cctty"
```

TypeScript：

```diff
- pathToClaudeCodeExecutable: "/path/to/claude"
+ pathToClaudeCodeExecutable: "/path/to/cctty"
```

流式消息、权限回调、`permissionMode`、`maxTurns`、`model`、`settingSources`、
MCP 配置等仍然走官方 SDK。

### TypeScript 示例

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

### Python 示例

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

## 替换边界

`cctty` 替换的是 SDK 底层启动的 `claude` 可执行文件，适用于 Claude Agent SDK
应用和 `claude -p` 类非交互式调用。

它不替代更上层的 ACP server、编辑器插件或项目自己的 Agent 编排协议。

- 如果应用暴露 `cli_path`、`pathToClaudeCodeExecutable`、
  `CLAUDE_CODE_EXECUTABLE`，把这个路径指向 `cctty`。
- 如果应用本身是 ACP adapter，保留 ACP adapter，只把它内部的 Claude
  可执行文件路径改成 `cctty`。
- 如果某个 wrapper 硬编码 `claude`，可以用 PATH shim，或者给上游提一个
  executable-path 配置。
- `cctty --acp --stdio` 不是推荐接口。ACP 是 Claude Code 上层 wrapper 的协议，
  不是 `cctty` 这个底层 Claude 可执行文件替身要实现的核心接口。

## 兼容状态

完整参数矩阵见英文 README 的
[Compatibility Matrix](README.md#compatibility-matrix)。下面是中文摘要：

| 能力 | 当前状态 |
| --- | --- |
| 官方 TypeScript Agent SDK | 支持，通过 `pathToClaudeCodeExecutable: "cctty"`。 |
| 官方 Python Agent SDK | 支持，通过 `ClaudeAgentOptions(cli_path="cctty")`。 |
| 权限模式 | 支持 `default`、`plan`、`auto`、`dontAsk`、`acceptEdits`、`bypassPermissions` 等常见模式。 |
| SDK 权限回调 | 支持 `--permission-prompt-tool stdio`，把 SDK `can_use_tool` 映射到终端权限表单。 |
| Plan 模式 | 支持并做过真实场景验证；长 prompt 被 Claude 折叠成 paste/form 时也会处理。 |
| MCP | 支持原生 `--mcp-config` 透传；支持 SDK in-process MCP 通过临时 stdio 代理桥接。 |
| 表单 / `AskUserQuestion` | 支持结构化转发和终端表单兜底解析；SDK 返回值会送回 Claude。 |
| 流式输入 | 支持 `--input-format stream-json` 和多轮 SDK stdin。 |
| 模型选择 | `--model` 透传；SDK `set_model` 会更新下一轮底层 Claude 启动参数。 |
| 快速模式 | 快速模式目前视为 Claude 终端自身行为；`cctty` 暂不实现单独的 `set_fast_mode` 控制。 |
| `--include-partial-messages` | 兼容导向的部分支持：会在 transcript 持久化后发合成 text delta，不是逐 token 实时流。 |
| `--json-schema` / `--fallback-model` / `--max-budget-usd` | 已接受并转发，但还需要更多真实差分测试才能标成完全支持。 |

## 开源生态实测

`cctty` 的目标是替换 Claude executable/core，而不是接管上层框架协议。

| 项目 / 包 | 状态 | 接入方式 |
| --- | --- | --- |
| `@anthropic-ai/claude-agent-sdk` | 支持 | `pathToClaudeCodeExecutable: "cctty"` |
| `claude-agent-sdk` | 支持 | `ClaudeAgentOptions(cli_path="cctty")` |
| Hermes Agent | 支持 cctty 分支 | 选择 `claude-sdk` provider，设置 `HERMES_CLAUDE_SDK_COMMAND=/path/to/cctty` 或 `CCTTY_CLI_PATH=/path/to/cctty`。 |
| NanoClaw / NanoBot | Core 支持，项目侧需要路径配置 | 其 runner 使用 SDK，但当前硬编码 `/pnpm/claude`；需要上游暴露配置，或在镜像内替换/symlink。 |
| `@agentclientprotocol/claude-agent-acp` | 支持作为底层 executable override | 设置 `CLAUDE_CODE_EXECUTABLE=/path/to/cctty`。 |
| `@zed-industries/claude-code-acp` | 支持作为底层 executable override | 设置 `CLAUDE_CODE_EXECUTABLE=/path/to/cctty`；该包已被 npm 标记为 deprecated。 |
| `acp-claude-code` | 条件支持 | 设置 `ACP_PATH_TO_CLAUDE_CODE_EXECUTABLE=/path/to/cctty`；部分版本依赖旧 SDK 导出。 |
| `claude-code-acp` / `cc-acp` | 需要 wrapper 改动 | 当前硬解析内置 `@anthropic-ai/claude-code/cli.js`，没有可替换路径。 |

## 诊断日志

`cctty` 默认写少量本地诊断日志，记录进程启动、真实 Claude 路径、SDK 控制请求、
权限决策、合成 result/idle 事件和超时摘要。默认不记录完整 prompt 文本。

默认路径：

- macOS：`~/Library/Logs/cctty/cctty.log`
- Linux：`${XDG_STATE_HOME:-~/.local/state}/cctty/cctty.log`

常用开关：

```sh
CCTTY_LOG_FILE=/tmp/cctty.log cctty --version
CCTTY_LOG=0 cctty --version
CCTTY_LOG_TTY=1 CCTTY_LOG_FILE=/tmp/cctty.log cctty -p "Reply OK"
```

`CCTTY_LOG_TTY=1` 会记录等待/超时时最近可见的终端文本，可能包含 prompt 和模型输出，
只建议本地排障时使用。

## 测试

快速 deterministic 测试使用假的交互式 Claude：

```sh
cargo test
```

SDK 集成测试会下载官方 SDK 包，并让它们通过 `cctty` 调用假的 Claude：

```sh
CCTTY_SDK_INTEGRATION=1 cargo test --test sdk_integration -- --ignored --nocapture
```

真实 Claude 差分测试需要本机 Claude 登录，会消耗真实调用：

```sh
CCTTY_LIVE_CLAUDE_DIFF=1 cargo test --test claude_differential -- --ignored --nocapture
```

真实 SDK 游戏测试会让 Python / TypeScript SDK 通过 `cctty` 写一个小浏览器游戏，
并通过 SDK 权限回调审批文件写入：

```sh
CCTTY_LIVE_SDK_GAME=1 cargo test --test sdk_integration live_python_sdk_builds_game_with_cctty_permissions -- --ignored --nocapture
CCTTY_LIVE_SDK_GAME=1 cargo test --test sdk_integration live_typescript_sdk_builds_game_with_cctty_permissions -- --ignored --nocapture
```

## 发布

仓库包含 GitHub Actions CI 和 tag release。发布新版本：

```sh
git tag v0.2.3
git push origin v0.2.3
```

Release workflow 会构建 macOS / Linux archive、生成 SHA-256，并在配置了
`HOMEBREW_TAP_TOKEN` 时更新 `Pyiner/homebrew-cctty`。

## License

MIT
