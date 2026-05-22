use uuid::Uuid;

use crate::error::{CcttyError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Text,
    StreamJson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandMode {
    Passthrough,
    Print,
}

#[derive(Debug, Clone)]
pub struct Invocation {
    pub mode: CommandMode,
    pub passthrough_args: Vec<String>,
    pub prompt: Option<String>,
    pub input_format: InputFormat,
    pub output_format: OutputFormat,
    pub session_id: Option<String>,
    pub resume: Option<String>,
    pub continue_conversation: bool,
    pub no_session_persistence: bool,
    pub permission_prompt_tool_stdio: bool,
    pub include_partial_messages: bool,
}

impl Invocation {
    pub fn parse(argv: Vec<String>) -> Result<Self> {
        let mut args = argv.into_iter().skip(1).collect::<Vec<_>>();
        if args.iter().any(|arg| arg == "--help" || arg == "-h") {
            return Ok(Self::passthrough(args));
        }
        if args.iter().any(|arg| arg == "--version" || arg == "-v") {
            return Ok(Self::passthrough(args));
        }

        let mut passthrough_args = Vec::new();
        let mut prompt = None;
        let mut input_format = InputFormat::Text;
        let mut output_format = OutputFormat::Text;
        let mut print = false;
        let mut session_id = None;
        let mut resume = None;
        let mut continue_conversation = false;
        let mut no_session_persistence = false;
        let mut permission_prompt_tool_stdio = false;
        let mut include_partial_messages = false;

        let mut index = 0;
        while index < args.len() {
            let arg = args[index].clone();
            if arg == "--" {
                if index + 1 < args.len() {
                    prompt = Some(args[index + 1..].join(" "));
                }
                break;
            }

            if arg == "--print" || arg == "-p" {
                print = true;
                index += 1;
                continue;
            }
            if let Some(value) = long_equals_value(&arg, "--input-format") {
                input_format = parse_input_format(value)?;
                index += 1;
                continue;
            }
            if arg == "--input-format" {
                let value = take_value(&args, index, "--input-format")?;
                input_format = parse_input_format(value)?;
                index += 2;
                continue;
            }
            if let Some(value) = long_equals_value(&arg, "--output-format") {
                output_format = parse_output_format(value)?;
                index += 1;
                continue;
            }
            if arg == "--output-format" {
                let value = take_value(&args, index, "--output-format")?;
                output_format = parse_output_format(value)?;
                index += 2;
                continue;
            }
            if arg == "--include-partial-messages" {
                include_partial_messages = true;
                index += 1;
                continue;
            }
            if arg == "--replay-user-messages" {
                index += 1;
                continue;
            }
            if arg == "--no-session-persistence" {
                no_session_persistence = true;
                index += 1;
                continue;
            }
            if let Some(value) = long_equals_value(&arg, "--permission-prompt-tool") {
                permission_prompt_tool_stdio = value == "stdio";
                if !permission_prompt_tool_stdio {
                    passthrough_args.push(arg);
                }
                index += 1;
                continue;
            }
            if arg == "--permission-prompt-tool" {
                let value = take_value(&args, index, "--permission-prompt-tool")?.to_owned();
                permission_prompt_tool_stdio = value == "stdio";
                if !permission_prompt_tool_stdio {
                    passthrough_args.push(arg);
                    passthrough_args.push(value);
                }
                index += 2;
                continue;
            }
            if let Some(value) = long_equals_value(&arg, "--system-prompt") {
                if !value.trim().is_empty() {
                    passthrough_args.push(arg);
                }
                index += 1;
                continue;
            }
            if arg == "--system-prompt" {
                let value = take_value(&args, index, "--system-prompt")?.to_owned();
                if !value.trim().is_empty() {
                    passthrough_args.push(arg);
                    passthrough_args.push(value);
                }
                index += 2;
                continue;
            }

            if let Some(value) = long_equals_value(&arg, "--session-id") {
                session_id = Some(value.to_owned());
                passthrough_args.push(arg);
                index += 1;
                continue;
            }
            if arg == "--session-id" {
                let value = take_value(&args, index, "--session-id")?.to_owned();
                session_id = Some(value.clone());
                passthrough_args.push(arg);
                passthrough_args.push(value);
                index += 2;
                continue;
            }
            if let Some(value) = long_equals_value(&arg, "--resume") {
                resume = Some(value.to_owned());
                passthrough_args.push(arg);
                index += 1;
                continue;
            }
            if arg == "--resume" || arg == "-r" {
                passthrough_args.push(arg);
                if index + 1 < args.len() && !args[index + 1].starts_with('-') {
                    let value = args[index + 1].clone();
                    resume = Some(value.clone());
                    passthrough_args.push(value);
                    index += 2;
                } else {
                    index += 1;
                }
                continue;
            }
            if arg == "--continue" || arg == "-c" {
                continue_conversation = true;
                passthrough_args.push(arg);
                index += 1;
                continue;
            }

            if arg.starts_with('-') {
                passthrough_args.push(arg.clone());
                index += 1;
                if arg.contains('=') {
                    continue;
                }
                let arity = flag_arity(&arg);
                match arity {
                    FlagArity::None => {}
                    FlagArity::One => {
                        if index < args.len() {
                            passthrough_args.push(args[index].clone());
                            index += 1;
                        }
                    }
                    FlagArity::Optional => {
                        if index < args.len() && !args[index].starts_with('-') {
                            passthrough_args.push(args[index].clone());
                            index += 1;
                        }
                    }
                    FlagArity::Many => {
                        while index < args.len() && !args[index].starts_with('-') {
                            passthrough_args.push(args[index].clone());
                            index += 1;
                        }
                    }
                }
                continue;
            }

            prompt = Some(args[index..].join(" "));
            break;
        }

        if print && session_id.is_none() && resume.is_none() && !continue_conversation {
            let generated = Uuid::new_v4().to_string();
            passthrough_args.push("--session-id".to_owned());
            passthrough_args.push(generated.clone());
            session_id = Some(generated.clone());
        }

        let mode = if print || input_format == InputFormat::StreamJson {
            CommandMode::Print
        } else {
            CommandMode::Passthrough
        };

        args.clear();
        Ok(Self {
            mode,
            passthrough_args,
            prompt,
            input_format,
            output_format,
            session_id,
            resume,
            continue_conversation,
            no_session_persistence,
            permission_prompt_tool_stdio,
            include_partial_messages,
        })
    }

    fn passthrough(args: Vec<String>) -> Self {
        Self {
            mode: CommandMode::Passthrough,
            passthrough_args: args,
            prompt: None,
            input_format: InputFormat::Text,
            output_format: OutputFormat::Text,
            session_id: None,
            resume: None,
            continue_conversation: false,
            no_session_persistence: false,
            permission_prompt_tool_stdio: false,
            include_partial_messages: false,
        }
    }
}

fn take_value<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str> {
    args.get(index + 1)
        .map(String::as_str)
        .ok_or_else(|| CcttyError::Usage(format!("{flag} requires a value")))
}

fn long_equals_value<'a>(arg: &'a str, flag: &str) -> Option<&'a str> {
    arg.strip_prefix(flag)
        .and_then(|rest| rest.strip_prefix('='))
}

fn parse_input_format(value: &str) -> Result<InputFormat> {
    match value {
        "text" => Ok(InputFormat::Text),
        "stream-json" => Ok(InputFormat::StreamJson),
        other => Err(CcttyError::Usage(format!(
            "unsupported --input-format {other:?}; expected text or stream-json"
        ))),
    }
}

fn parse_output_format(value: &str) -> Result<OutputFormat> {
    match value {
        "text" => Ok(OutputFormat::Text),
        "json" => Ok(OutputFormat::Json),
        "stream-json" => Ok(OutputFormat::StreamJson),
        other => Err(CcttyError::Usage(format!(
            "unsupported --output-format {other:?}; expected text, json, or stream-json"
        ))),
    }
}

#[derive(Debug, Clone, Copy)]
enum FlagArity {
    None,
    One,
    Optional,
    Many,
}

fn flag_arity(arg: &str) -> FlagArity {
    let flag = arg.split_once('=').map(|(flag, _)| flag).unwrap_or(arg);
    match flag {
        "--add-dir" | "--allowedTools" | "--allowed-tools" | "--betas" | "--disallowedTools"
        | "--disallowed-tools" | "--file" | "--mcp-config" | "--tools" => FlagArity::Many,
        "--channels" | "--dangerously-load-development-channels" => FlagArity::Many,
        "--agent"
        | "--advisor"
        | "--agent-color"
        | "--agent-id"
        | "--agent-name"
        | "--agent-type"
        | "--agents"
        | "--append-system-prompt"
        | "--append-system-prompt-file"
        | "--deep-link-cwd-b64"
        | "--deep-link-last-fetch"
        | "--deep-link-repo"
        | "--debug-file"
        | "--effort"
        | "--fallback-model"
        | "--json-schema"
        | "--managed-settings"
        | "--max-budget-usd"
        | "--max-thinking-tokens"
        | "--max-turns"
        | "--model"
        | "--name"
        | "-n"
        | "--parent-session-id"
        | "--permission-mode"
        | "--permission-prompt-tool"
        | "--plan-mode-instructions"
        | "--plugin-dir"
        | "--plugin-url"
        | "--prefill"
        | "--prefill-b64"
        | "--rewind-files"
        | "--resume-session-at"
        | "--remote-control-session-name-prefix"
        | "--sdk-url"
        | "--setting-sources"
        | "--settings"
        | "--system-prompt"
        | "--system-prompt-file"
        | "--task-budget"
        | "--team-name"
        | "--teammate-mode"
        | "--thinking"
        | "--thinking-display"
        | "--workload" => FlagArity::One,
        "--debug" | "-d" | "--from-pr" | "--rc" | "--remote" | "--remote-control"
        | "--teleport" | "--worktree" | "-w" => FlagArity::Optional,
        _ => FlagArity::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strips_print_flags_and_keeps_claude_flags() {
        let invocation = Invocation::parse(vec![
            "cctty".to_owned(),
            "--print".to_owned(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--input-format=text".to_owned(),
            "--model".to_owned(),
            "sonnet".to_owned(),
            "hello".to_owned(),
        ])
        .unwrap();

        assert_eq!(invocation.mode, CommandMode::Print);
        assert_eq!(invocation.output_format, OutputFormat::StreamJson);
        assert_eq!(invocation.input_format, InputFormat::Text);
        assert_eq!(invocation.prompt.as_deref(), Some("hello"));
        assert!(
            invocation
                .passthrough_args
                .windows(2)
                .any(|w| w == ["--model", "sonnet"])
        );
        assert!(
            !invocation
                .passthrough_args
                .iter()
                .any(|arg| arg == "--print")
        );
        assert!(invocation.session_id.is_some());
    }

    #[test]
    fn parse_does_not_let_equals_flags_swallow_later_cctty_flags() {
        let invocation = Invocation::parse(vec![
            "cctty".to_owned(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--setting-sources=project".to_owned(),
            "--input-format".to_owned(),
            "stream-json".to_owned(),
        ])
        .unwrap();

        assert_eq!(invocation.mode, CommandMode::Print);
        assert_eq!(invocation.input_format, InputFormat::StreamJson);
        assert_eq!(
            invocation.passthrough_args,
            vec!["--setting-sources=project".to_owned()]
        );
    }

    #[test]
    fn parse_tmux_does_not_swallow_prompt() {
        let invocation = Invocation::parse(vec![
            "cctty".to_owned(),
            "--print".to_owned(),
            "--tmux".to_owned(),
            "hello".to_owned(),
        ])
        .unwrap();

        assert_eq!(invocation.prompt.as_deref(), Some("hello"));
        assert!(
            invocation
                .passthrough_args
                .iter()
                .any(|arg| arg == "--tmux")
        );
    }

    #[test]
    fn parse_passes_every_current_claude_help_option_shape() {
        for case in current_claude_option_cases() {
            let mut argv = vec![
                "cctty".to_owned(),
                "--output-format".to_owned(),
                "stream-json".to_owned(),
            ];
            argv.extend(case.argv.iter().map(|value| value.to_string()));
            argv.push("--input-format".to_owned());
            argv.push("stream-json".to_owned());

            let invocation = Invocation::parse(argv)
                .unwrap_or_else(|error| panic!("{} failed to parse: {error}", case.name));
            if matches!(case.name, "-h" | "--help" | "-v" | "--version") {
                assert_eq!(invocation.mode, CommandMode::Passthrough);
                for expected in case.expected_passthrough {
                    assert!(
                        invocation
                            .passthrough_args
                            .iter()
                            .any(|arg| arg == expected),
                        "{} did not pass through {expected:?}; got {:?}",
                        case.name,
                        invocation.passthrough_args
                    );
                }
                continue;
            }
            assert_eq!(
                invocation.input_format,
                InputFormat::StreamJson,
                "{} swallowed --input-format",
                case.name
            );
            for expected in case.expected_passthrough {
                assert!(
                    invocation
                        .passthrough_args
                        .iter()
                        .any(|arg| arg == expected),
                    "{} did not pass through {expected:?}; got {:?}",
                    case.name,
                    invocation.passthrough_args
                );
            }
        }
    }

    #[test]
    fn parse_passes_all_permission_modes() {
        for mode in [
            "acceptEdits",
            "auto",
            "bypassPermissions",
            "default",
            "dontAsk",
            "plan",
        ] {
            let invocation = Invocation::parse(vec![
                "cctty".to_owned(),
                "--print".to_owned(),
                "--permission-mode".to_owned(),
                mode.to_owned(),
                "hello".to_owned(),
            ])
            .unwrap();

            assert!(
                invocation
                    .passthrough_args
                    .windows(2)
                    .any(|pair| pair == ["--permission-mode", mode]),
                "permission mode {mode} was not passed through"
            );
        }
    }

    #[test]
    fn parse_passes_hidden_sdk_and_native_option_shapes() {
        for case in hidden_sdk_and_native_option_cases() {
            let mut argv = vec![
                "cctty".to_owned(),
                "--output-format".to_owned(),
                "stream-json".to_owned(),
            ];
            argv.extend(case.argv.iter().map(|value| value.to_string()));
            argv.push("--input-format".to_owned());
            argv.push("stream-json".to_owned());

            let invocation = Invocation::parse(argv)
                .unwrap_or_else(|error| panic!("{} failed to parse: {error}", case.name));
            assert_eq!(
                invocation.input_format,
                InputFormat::StreamJson,
                "{} swallowed --input-format",
                case.name
            );
            for expected in case.expected_passthrough {
                assert!(
                    invocation
                        .passthrough_args
                        .iter()
                        .any(|arg| arg == expected),
                    "{} did not pass through {expected:?}; got {:?}",
                    case.name,
                    invocation.passthrough_args
                );
            }
        }
    }

    #[test]
    fn parse_tracks_permission_prompt_stdio() {
        let invocation = Invocation::parse(vec![
            "cctty".to_owned(),
            "--print".to_owned(),
            "--permission-prompt-tool".to_owned(),
            "stdio".to_owned(),
            "hello".to_owned(),
        ])
        .unwrap();

        assert!(invocation.permission_prompt_tool_stdio);
        assert!(
            invocation
                .passthrough_args
                .iter()
                .all(|arg| arg != "--permission-prompt-tool" && arg != "stdio")
        );
    }

    #[test]
    fn parse_forwards_non_stdio_permission_prompt_tool() {
        let invocation = Invocation::parse(vec![
            "cctty".to_owned(),
            "--print".to_owned(),
            "--permission-prompt-tool".to_owned(),
            "custom-permission-tool".to_owned(),
            "hello".to_owned(),
        ])
        .unwrap();

        assert!(!invocation.permission_prompt_tool_stdio);
        assert!(
            invocation
                .passthrough_args
                .windows(2)
                .any(|arg| arg == ["--permission-prompt-tool", "custom-permission-tool"])
        );
    }

    #[test]
    fn parse_drops_empty_sdk_system_prompt_to_preserve_claude_defaults() {
        let invocation = Invocation::parse(vec![
            "cctty".to_owned(),
            "--input-format".to_owned(),
            "stream-json".to_owned(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--system-prompt".to_owned(),
            "".to_owned(),
        ])
        .unwrap();

        assert!(
            !invocation
                .passthrough_args
                .iter()
                .any(|arg| arg == "--system-prompt")
        );

        let invocation = Invocation::parse(vec![
            "cctty".to_owned(),
            "--input-format".to_owned(),
            "stream-json".to_owned(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--system-prompt=   ".to_owned(),
        ])
        .unwrap();

        assert!(
            !invocation
                .passthrough_args
                .iter()
                .any(|arg| arg.starts_with("--system-prompt"))
        );
    }

    #[test]
    fn parse_tracks_no_session_persistence_without_forwarding_it() {
        let invocation = Invocation::parse(vec![
            "cctty".to_owned(),
            "--print".to_owned(),
            "--no-session-persistence".to_owned(),
            "hello".to_owned(),
        ])
        .unwrap();

        assert!(invocation.no_session_persistence);
        assert!(
            !invocation
                .passthrough_args
                .iter()
                .any(|arg| arg == "--no-session-persistence")
        );
    }

    #[test]
    fn parse_tracks_include_partial_messages_without_forwarding_it() {
        let invocation = Invocation::parse(vec![
            "cctty".to_owned(),
            "--input-format".to_owned(),
            "stream-json".to_owned(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--include-partial-messages".to_owned(),
        ])
        .unwrap();

        assert!(invocation.include_partial_messages);
        assert!(
            !invocation
                .passthrough_args
                .iter()
                .any(|arg| arg == "--include-partial-messages")
        );
    }

    struct OptionCase {
        name: &'static str,
        argv: &'static [&'static str],
        expected_passthrough: &'static [&'static str],
    }

    fn current_claude_option_cases() -> Vec<OptionCase> {
        vec![
            pass("--add-dir", &["--add-dir", "dir-a", "dir-b"]),
            pass("--agent", &["--agent", "reviewer"]),
            pass(
                "--agents",
                &[
                    "--agents",
                    r#"{"reviewer":{"description":"Review","prompt":"Review"}}"#,
                ],
            ),
            pass(
                "--allow-dangerously-skip-permissions",
                &["--allow-dangerously-skip-permissions"],
            ),
            pass("--allowedTools", &["--allowedTools", "Bash,Read"]),
            pass("--allowed-tools", &["--allowed-tools", "Bash", "Read"]),
            pass(
                "--append-system-prompt",
                &["--append-system-prompt", "extra"],
            ),
            pass("--bare", &["--bare"]),
            pass("--betas", &["--betas", "beta-a", "beta-b"]),
            pass("--brief", &["--brief"]),
            pass("--chrome", &["--chrome"]),
            pass("-c", &["-c"]),
            pass("--continue", &["--continue"]),
            pass(
                "--dangerously-skip-permissions",
                &["--dangerously-skip-permissions"],
            ),
            pass("-d", &["-d", "api"]),
            pass("--debug", &["--debug", "api"]),
            pass("--debug-file", &["--debug-file", "debug.log"]),
            pass("--disable-slash-commands", &["--disable-slash-commands"]),
            pass("--disallowedTools", &["--disallowedTools", "Write,Edit"]),
            pass(
                "--disallowed-tools",
                &["--disallowed-tools", "Write", "Edit"],
            ),
            pass("--effort", &["--effort", "low"]),
            pass(
                "--exclude-dynamic-system-prompt-sections",
                &["--exclude-dynamic-system-prompt-sections"],
            ),
            pass("--fallback-model", &["--fallback-model", "sonnet"]),
            pass(
                "--file",
                &["--file", "file_abc:doc.txt", "file_def:img.png"],
            ),
            pass("--fork-session", &["--fork-session"]),
            pass("--from-pr", &["--from-pr", "123"]),
            pass("-h", &["-h"]),
            pass("--help", &["--help"]),
            pass("--ide", &["--ide"]),
            pass("--include-hook-events", &["--include-hook-events"]),
            consumed(
                "--include-partial-messages",
                &["--include-partial-messages"],
            ),
            consumed("--input-format", &["--input-format", "text"]),
            pass("--json-schema", &["--json-schema", r#"{"type":"object"}"#]),
            pass("--max-budget-usd", &["--max-budget-usd", "0.01"]),
            pass("--mcp-config", &["--mcp-config", r#"{"mcpServers":{}}"#]),
            pass("--mcp-debug", &["--mcp-debug"]),
            pass("--model", &["--model", "sonnet"]),
            pass("-n", &["-n", "Synthetic Session"]),
            pass("--name", &["--name", "Synthetic Session"]),
            pass("--no-chrome", &["--no-chrome"]),
            consumed("--no-session-persistence", &["--no-session-persistence"]),
            consumed("--output-format", &["--output-format", "json"]),
            pass("--permission-mode", &["--permission-mode", "plan"]),
            pass("--plugin-dir", &["--plugin-dir", "plugin-dir"]),
            pass(
                "--plugin-url",
                &["--plugin-url", "https://example.invalid/plugin.zip"],
            ),
            consumed("-p", &["-p"]),
            consumed("--print", &["--print"]),
            pass("--remote-control", &["--remote-control", "synthetic"]),
            pass(
                "--remote-control-session-name-prefix",
                &["--remote-control-session-name-prefix", "synthetic"],
            ),
            consumed("--replay-user-messages", &["--replay-user-messages"]),
            pass("-r", &["-r", "00000000-0000-0000-0000-000000000001"]),
            pass(
                "--resume",
                &["--resume", "00000000-0000-0000-0000-000000000001"],
            ),
            pass(
                "--session-id",
                &["--session-id", "00000000-0000-0000-0000-000000000002"],
            ),
            pass("--setting-sources", &["--setting-sources", "project"]),
            pass("--settings", &["--settings", "{}"]),
            pass("--strict-mcp-config", &["--strict-mcp-config"]),
            pass("--system-prompt", &["--system-prompt", "system"]),
            pass("--tmux", &["--tmux"]),
            pass("--tools", &["--tools", "Bash,Read"]),
            pass("--verbose", &["--verbose"]),
            pass("-v", &["-v"]),
            pass("--version", &["--version"]),
            pass("-w", &["-w", "synthetic-worktree"]),
            pass("--worktree", &["--worktree", "synthetic-worktree"]),
        ]
    }

    fn hidden_sdk_and_native_option_cases() -> Vec<OptionCase> {
        vec![
            pass("--advisor", &["--advisor", "sonnet"]),
            pass("--agent-color", &["--agent-color", "blue"]),
            pass("--agent-id", &["--agent-id", "agent_0000000000000000"]),
            pass("--agent-name", &["--agent-name", "Test Agent"]),
            pass("--agent-type", &["--agent-type", "reviewer"]),
            pass(
                "--append-system-prompt-file",
                &["--append-system-prompt-file", "append.md"],
            ),
            pass("--channels", &["--channels", "server-a", "server-b"]),
            pass("--cowork", &["--cowork"]),
            pass(
                "--dangerously-load-development-channels",
                &[
                    "--dangerously-load-development-channels",
                    "server-a",
                    "server-b",
                ],
            ),
            pass("--deep-link-cwd-b64", &["--deep-link-cwd-b64", "L3RtcA=="]),
            pass("--deep-link-last-fetch", &["--deep-link-last-fetch", "0"]),
            pass("--deep-link-origin", &["--deep-link-origin"]),
            pass("--deep-link-repo", &["--deep-link-repo", "owner/repo"]),
            pass("--enable-auth-status", &["--enable-auth-status"]),
            pass("--enable-auto-mode", &["--enable-auto-mode"]),
            pass("--init", &["--init"]),
            pass("--init-only", &["--init-only"]),
            pass("--maintenance", &["--maintenance"]),
            pass("--managed-settings", &["--managed-settings", "{}"]),
            pass("--max-thinking-tokens", &["--max-thinking-tokens", "1024"]),
            pass("--max-turns", &["--max-turns", "1"]),
            pass("--parent-session-id", &["--parent-session-id", "parent-1"]),
            pass(
                "--plan-mode-instructions",
                &["--plan-mode-instructions", "Write a short plan first"],
            ),
            pass("--plan-mode-required", &["--plan-mode-required"]),
            pass("--prefill", &["--prefill", "Draft text"]),
            pass("--prefill-b64", &["--prefill-b64", "RHJhZnQ="]),
            pass("--rc", &["--rc", "mobile"]),
            pass("--remote", &["--remote", "session"]),
            pass("--resume-session-at", &["--resume-session-at", "message-1"]),
            pass("--rewind-files", &["--rewind-files", "message-1"]),
            pass("--sdk-url", &["--sdk-url", "ws://127.0.0.1:1234"]),
            pass("--session-mirror", &["--session-mirror"]),
            pass(
                "--system-prompt-file",
                &["--system-prompt-file", "system.md"],
            ),
            pass("--task-budget", &["--task-budget", "2048"]),
            pass("--team-name", &["--team-name", "Test Team"]),
            pass("--teammate-mode", &["--teammate-mode", "auto"]),
            pass("--teleport", &["--teleport", "session-1"]),
            pass("--thinking", &["--thinking", "adaptive"]),
            pass("--thinking-display", &["--thinking-display", "full"]),
            pass("--workload", &["--workload", "test-workload"]),
            pass("--xaa", &["--xaa"]),
        ]
    }

    fn pass(name: &'static str, argv: &'static [&'static str]) -> OptionCase {
        OptionCase {
            name,
            argv,
            expected_passthrough: argv,
        }
    }

    fn consumed(name: &'static str, argv: &'static [&'static str]) -> OptionCase {
        OptionCase {
            name,
            argv,
            expected_passthrough: &[],
        }
    }
}
