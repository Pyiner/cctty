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
            if arg == "--include-partial-messages"
                || arg == "--replay-user-messages"
                || arg == "--no-session-persistence"
            {
                index += 1;
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
        "--agent"
        | "--agents"
        | "--append-system-prompt"
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
        | "--permission-mode"
        | "--permission-prompt-tool"
        | "--plugin-dir"
        | "--plugin-url"
        | "--resume-session-at"
        | "--remote-control-session-name-prefix"
        | "--setting-sources"
        | "--settings"
        | "--system-prompt"
        | "--system-prompt-file"
        | "--task-budget"
        | "--thinking"
        | "--thinking-display" => FlagArity::One,
        "--debug" | "-d" | "--from-pr" | "--remote-control" | "--tmux" | "--worktree" | "-w" => {
            FlagArity::Optional
        }
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
}
