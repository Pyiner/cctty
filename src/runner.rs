use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader as StdBufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::args::{CommandMode, InputFormat, Invocation, OutputFormat};
use crate::error::{CcttyError, Result};
use crate::logging;
use crate::pty::{PtyProcess, PtySpawnSpec};
use crate::transcript::{TranscriptState, claude_config_dir, read_complete_lines, transcript_path};

mod claude_path;
mod sdk_control;
mod sdk_mcp;
mod session_id;
mod tty_text;
use claude_path::resolve_claude_path;
use sdk_control::handle_control_request;
pub(crate) use sdk_mcp::run_mcp_proxy;
use sdk_mcp::{
    SdkMcpRuntime, SdkStreamState, args_with_mcp_runtime, create_sdk_mcp_runtime,
    mcp_tool_names_for_log, rewrite_mcp_tool_call_for_sdk, rewrite_mcp_tools_for_claude,
    sdk_mcp_runtime_servers, sdk_mcp_runtime_socket_path, sdk_mcp_servers_from_initialize,
};
use session_id::SessionIdAlias;
use tty_text::{
    compact_tty_output, plain_tty_output, recent_tty_log_text, single_line_log_text,
    visible_tty_lines, visible_tty_lines_preserving_spacing,
};

const COMPLETION_IDLE: Duration = Duration::from_millis(1_500);
const TRANSCRIPT_POLL: Duration = Duration::from_millis(80);
const TRUST_PROMPT_SETTLE: Duration = Duration::from_millis(800);
const TTY_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const TTY_READY_SETTLE: Duration = Duration::from_millis(250);
const PERMISSION_PROMPT_TIMEOUT: Duration = Duration::from_secs(8);
const TTY_PERMISSION_TRANSCRIPT_GRACE: Duration = Duration::from_millis(1_500);
const TTY_QUESTION_FORM_SETTLE: Duration = Duration::from_millis(250);
const MCP_PROXY_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const MCP_PROXY_REQUEST_LINE_TIMEOUT: Duration = Duration::from_millis(250);
const PTY_TERMINATE_TIMEOUT: Duration = Duration::from_secs(5);
const TTY_SESSION_LOCK_RETRY_TIMEOUT: Duration = Duration::from_secs(8);
const TTY_SESSION_LOCK_RETRY_DELAY: Duration = Duration::from_millis(300);
const TTY_VISIBLE_COMPLETION_IDLE: Duration = Duration::from_secs(3);
const RUN_TIMEOUT: Duration = Duration::from_secs(3600);

pub async fn run(invocation: Invocation) -> Result<i32> {
    match invocation.mode {
        CommandMode::Passthrough => run_passthrough(&invocation).await,
        CommandMode::Print => run_print(invocation).await,
    }
}

async fn run_passthrough(invocation: &Invocation) -> Result<i32> {
    let claude = resolve_claude_path()?;
    logging::event(format!(
        "passthrough_spawn claude={} args={}",
        claude,
        invocation.passthrough_args.len()
    ));
    let mut child = tokio::process::Command::new(claude)
        .args(&invocation.passthrough_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let status = child.wait().await?;
    let code = status.code().unwrap_or(1);
    logging::event(format!("passthrough_exit exit_code={code}"));
    Ok(code)
}

async fn run_print(invocation: Invocation) -> Result<i32> {
    let claude = resolve_claude_path()?;
    let cwd = std::env::current_dir()?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let config_dir = claude_config_dir()?;
    let session_id = invocation
        .session_id
        .clone()
        .or_else(|| invocation.resume.clone());
    let session_alias = SessionIdAlias::new(session_id.clone());
    let claude_session_id = session_alias.claude_session_id();
    let transcript = if invocation.continue_conversation {
        None
    } else {
        claude_session_id
            .as_ref()
            .map(|session_id| transcript_path(&config_dir, &cwd, session_id))
    };
    let env = interactive_claude_env();
    let mut passthrough_args = session_alias.rewrite_args_for_claude(&invocation.passthrough_args);
    if invocation.permission_prompt_tool_stdio {
        passthrough_args = allow_bridged_ask_user_question_tool(&passthrough_args);
    }
    let mut current_session_id = claude_session_id.clone();
    let mut attach_existing = invocation.continue_conversation;
    let mut transcript = transcript;
    let mut process = spawn_tty_process(
        &claude,
        &cwd,
        &env,
        &passthrough_args,
        &current_session_id,
        &invocation,
    )?;
    if invocation.input_format == InputFormat::Text {
        if let Err(error) = prepare_tty_for_prompt_retrying_session_lock(
            &mut process,
            &claude,
            &cwd,
            &env,
            &passthrough_args,
            &current_session_id,
            &invocation,
        )
        .await
        {
            if is_bad_resume_startup_error(&error) {
                let stripped_args = strip_session_resume_args(&passthrough_args);
                if stripped_args != passthrough_args {
                    logging::event("tty_restart reason=bad_resume action=strip_resume_args");
                    process.terminate(PTY_TERMINATE_TIMEOUT);
                    passthrough_args = stripped_args;
                    current_session_id = None;
                    attach_existing = false;
                    transcript = None;
                    process = spawn_tty_process(
                        &claude,
                        &cwd,
                        &env,
                        &passthrough_args,
                        &current_session_id,
                        &invocation,
                    )?;
                    prepare_tty_for_prompt_retrying_session_lock(
                        &mut process,
                        &claude,
                        &cwd,
                        &env,
                        &passthrough_args,
                        &current_session_id,
                        &invocation,
                    )
                    .await?;
                } else {
                    return Err(error);
                }
            } else {
                return Err(error);
            }
        }
    }

    let mut tail = TailCursor::new(transcript, &config_dir, attach_existing, session_alias)?;

    match invocation.input_format {
        InputFormat::Text => {
            let prompt = prompt_from_invocation(&invocation)?;
            let outcome = submit_prompt_and_tail(
                &mut process,
                &mut tail,
                &prompt,
                invocation.output_format,
                invocation.include_partial_messages,
            )
            .await?;
            write_final_output(&outcome, invocation.output_format)?;
        }
        InputFormat::StreamJson => {
            let mut stream_spawn = StreamSpawnContext {
                claude: &claude,
                cwd: &cwd,
                env: &env,
                args: passthrough_args.clone(),
                session_id: current_session_id.clone(),
                invocation: &invocation,
                allow_session_strip_on_next_prepare: false,
            };
            run_stream_json(
                &mut process,
                &mut tail,
                &mut stream_spawn,
                invocation.output_format,
                invocation.permission_prompt_tool_stdio,
                invocation.include_partial_messages,
            )
            .await?;
        }
    }

    process.terminate(PTY_TERMINATE_TIMEOUT);
    if invocation.no_session_persistence {
        tail.remove_current_transcript()?;
    }
    Ok(0)
}

fn spawn_tty_process(
    claude: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    args: &[String],
    session_id: &Option<String>,
    invocation: &Invocation,
) -> Result<PtyProcess> {
    logging::event(format!(
        "tty_spawn claude={} cwd={} session_id={} input={:?} output={:?} permission_prompt_stdio={} include_partial_messages={} args={}",
        claude,
        cwd.display(),
        session_id.as_deref().unwrap_or(""),
        invocation.input_format,
        invocation.output_format,
        invocation.permission_prompt_tool_stdio,
        invocation.include_partial_messages,
        sanitized_arg_shape(args)
    ));
    PtyProcess::spawn(&PtySpawnSpec {
        command: claude.to_owned(),
        args: args.to_vec(),
        cwd: cwd.to_path_buf(),
        env: env.clone(),
        unset_env: interactive_claude_unset_env(args),
    })
}

fn interactive_claude_env() -> HashMap<String, String> {
    HashMap::from([
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("COLORTERM".to_owned(), "truecolor".to_owned()),
    ])
}

fn interactive_claude_unset_env(args: &[String]) -> Vec<String> {
    let mut keys = [
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_AGENT_SDK_VERSION",
        "NO_COLOR",
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect::<Vec<_>>();
    if enables_remote_control(args) {
        if std::env::var_os("CLAUDE_CODE_OAUTH_TOKEN").is_some() {
            logging::event(
                "env_unset reason=remote_control_full_scope name=CLAUDE_CODE_OAUTH_TOKEN",
            );
        }
        keys.push("CLAUDE_CODE_OAUTH_TOKEN".to_owned());
    }
    keys
}

fn enables_remote_control(args: &[String]) -> bool {
    let mut enabled = false;
    for arg in args {
        if arg == "--no-chrome" {
            enabled = false;
        } else if matches!(
            arg.as_str(),
            "--chrome" | "--remote-control" | "--remote" | "--rc"
        ) || arg.starts_with("--remote-control=")
            || arg.starts_with("--remote=")
            || arg.starts_with("--rc=")
        {
            enabled = true;
        }
    }
    enabled
}

fn is_bad_resume_startup_error(error: &CcttyError) -> bool {
    matches!(error, CcttyError::Tty(message) if tty_output_has_missing_resume_startup_error(message))
}

fn is_session_lock_startup_error(error: &CcttyError) -> bool {
    matches!(error, CcttyError::Tty(message) if tty_output_has_session_lock_startup_error(message))
}

fn strip_session_resume_args(args: &[String]) -> Vec<String> {
    let mut stripped = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if matches!(
            arg.as_str(),
            "--session-id" | "--resume" | "-r" | "--resume-session-at"
        ) {
            index += 1;
            if index < args.len() && !args[index].starts_with('-') {
                index += 1;
            }
            continue;
        }
        if matches!(arg.as_str(), "--continue" | "-c" | "--fork-session")
            || arg.starts_with("--session-id=")
            || arg.starts_with("--resume=")
            || arg.starts_with("--resume-session-at=")
        {
            index += 1;
            continue;
        }
        stripped.push(arg.clone());
        index += 1;
    }
    stripped
}

fn allow_bridged_ask_user_question_tool(args: &[String]) -> Vec<String> {
    let rewritten = strip_disallowed_tool(args, "AskUserQuestion");
    if rewritten != args {
        logging::event("args_rewrite reason=allow_bridged_ask_user_question");
    }
    rewritten
}

fn strip_disallowed_tool(args: &[String], tool_name: &str) -> Vec<String> {
    let mut stripped = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if is_disallowed_tools_flag(arg) {
            if let Some(value) = args.get(index + 1) {
                if let Some(filtered) = filter_disallowed_tool_value(value, tool_name) {
                    stripped.push(arg.clone());
                    stripped.push(filtered);
                }
                index += 2;
            } else {
                stripped.push(arg.clone());
                index += 1;
            }
            continue;
        }
        if let Some((flag, value)) = arg.split_once('=')
            && is_disallowed_tools_flag(flag)
        {
            if let Some(filtered) = filter_disallowed_tool_value(value, tool_name) {
                stripped.push(format!("{flag}={filtered}"));
            }
            index += 1;
            continue;
        }
        stripped.push(arg.clone());
        index += 1;
    }
    stripped
}

fn is_disallowed_tools_flag(flag: &str) -> bool {
    matches!(flag, "--disallowedTools" | "--disallowed-tools")
}

fn filter_disallowed_tool_value(value: &str, tool_name: &str) -> Option<String> {
    let kept = value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty() && !part.eq_ignore_ascii_case(tool_name))
        .collect::<Vec<_>>();
    if kept.is_empty() {
        None
    } else {
        Some(kept.join(","))
    }
}

fn sanitized_arg_shape(args: &[String]) -> String {
    if args.is_empty() {
        return "-".to_owned();
    }
    args.iter()
        .map(|arg| {
            if arg.starts_with('-') {
                arg.split_once('=')
                    .map(|(flag, _)| format!("{flag}=<value>"))
                    .unwrap_or_else(|| arg.clone())
            } else {
                "<value>".to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

struct StreamSpawnContext<'a> {
    claude: &'a str,
    cwd: &'a Path,
    env: &'a HashMap<String, String>,
    args: Vec<String>,
    session_id: Option<String>,
    invocation: &'a Invocation,
    allow_session_strip_on_next_prepare: bool,
}

async fn ensure_sdk_mcp_runtime(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    input: &mut mpsc::Receiver<Value>,
    sdk_state: &mut SdkStreamState,
    spawn: &mut StreamSpawnContext<'_>,
) -> Result<bool> {
    if sdk_state.mcp_runtime.is_some() {
        return Ok(false);
    }
    let servers = sdk_state.sdk_mcp_server_names();
    if servers.is_empty() {
        return Ok(false);
    }

    let runtime = create_sdk_mcp_runtime(servers)?;
    let rewritten_args = args_with_mcp_runtime(&spawn.args, &runtime)?;
    logging::event(format!(
        "mcp_runtime_start socket={} servers={}",
        sdk_mcp_runtime_socket_path(&runtime),
        sdk_mcp_runtime_servers(&runtime).join(",")
    ));
    logging::event(format!(
        "tty_restart reason=sdk_mcp servers={} args={}",
        sdk_mcp_runtime_servers(&runtime).join(","),
        sanitized_arg_shape(&rewritten_args)
    ));
    process.terminate(PTY_TERMINATE_TIMEOUT);
    *process = spawn_tty_process(
        spawn.claude,
        spawn.cwd,
        spawn.env,
        &rewritten_args,
        &spawn.session_id,
        spawn.invocation,
    )?;
    match prepare_tty_for_prompt_with_mcp_retrying_session_lock(
        process,
        input,
        &runtime,
        sdk_state,
        spawn,
        &rewritten_args,
        &spawn.session_id,
    )
    .await
    {
        Ok(()) => {
            spawn.allow_session_strip_on_next_prepare = false;
        }
        Err(error)
            if is_session_lock_startup_error(&error)
                && spawn.allow_session_strip_on_next_prepare =>
        {
            let stripped_args = strip_session_resume_args(&spawn.args);
            if stripped_args == spawn.args {
                return Err(error);
            }
            let rewritten_stripped_args = args_with_mcp_runtime(&stripped_args, &runtime)?;
            logging::event(
                "tty_restart reason=control_update_session_lock action=strip_session_args",
            );
            process.terminate(PTY_TERMINATE_TIMEOUT);
            *process = spawn_tty_process(
                spawn.claude,
                spawn.cwd,
                spawn.env,
                &rewritten_stripped_args,
                &None,
                spawn.invocation,
            )?;
            prepare_tty_for_prompt_with_mcp_retrying_session_lock(
                process,
                input,
                &runtime,
                sdk_state,
                spawn,
                &rewritten_stripped_args,
                &None,
            )
            .await?;
            spawn.args = stripped_args;
            spawn.session_id = None;
            spawn.allow_session_strip_on_next_prepare = false;
            tail.reset_for_new_session();
        }
        Err(error) => return Err(error),
    }
    sdk_state.mcp_runtime = Some(runtime);
    Ok(true)
}

async fn run_stream_json(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    spawn: &mut StreamSpawnContext<'_>,
    output_format: OutputFormat,
    permission_prompt_tool_stdio: bool,
    include_partial_messages: bool,
) -> Result<()> {
    if output_format != OutputFormat::StreamJson {
        return Err(CcttyError::Usage(
            "--input-format stream-json currently requires --output-format stream-json".to_owned(),
        ));
    }
    let mut input = spawn_stdin_json_reader();
    let mut sdk_state = SdkStreamState::new(&spawn.args);
    let mut tty_prepared = false;
    loop {
        let value = if let Some(value) = sdk_state.deferred_input.pop_front() {
            value
        } else {
            match input.recv().await {
                Some(value) => value,
                None => break,
            }
        };
        match value.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                handle_control_request(
                    process,
                    &mut input,
                    &mut sdk_state,
                    spawn,
                    &mut tty_prepared,
                    &value,
                )
                .await?
            }
            Some("control_response") => {}
            Some("control_cancel_request") => {}
            Some("user") => {
                let prompt = user_prompt_from_sdk_message(&value)?;
                logging::event(format!(
                    "stream_user received content_chars={}",
                    prompt.chars().count()
                ));
                maybe_log_prompt_diagnostic(&prompt);
                if ensure_sdk_mcp_runtime(process, tail, &mut input, &mut sdk_state, spawn).await? {
                    tty_prepared = true;
                }
                if !tty_prepared {
                    prepare_stream_tty_for_prompt_with_sdk_state(
                        process,
                        tail,
                        &mut input,
                        &mut sdk_state,
                        spawn,
                    )
                    .await?;
                    tty_prepared = true;
                }
                let _ = submit_prompt_and_tail_stream(
                    process,
                    tail,
                    &mut input,
                    &mut sdk_state,
                    &prompt,
                    output_format,
                    permission_prompt_tool_stdio,
                    include_partial_messages,
                )
                .await?;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn prepare_stream_tty_for_prompt_with_sdk_state(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    input: &mut mpsc::Receiver<Value>,
    sdk_state: &mut SdkStreamState,
    spawn: &mut StreamSpawnContext<'_>,
) -> Result<()> {
    let Some(runtime) = sdk_state.mcp_runtime.take() else {
        return prepare_stream_tty_for_prompt(process, tail, spawn).await;
    };
    let result = prepare_stream_tty_for_prompt_with_mcp_runtime(
        process, tail, input, &runtime, sdk_state, spawn,
    )
    .await;
    sdk_state.mcp_runtime = Some(runtime);
    result
}

async fn prepare_stream_tty_for_prompt(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    spawn: &mut StreamSpawnContext<'_>,
) -> Result<()> {
    match prepare_tty_for_prompt_retrying_session_lock(
        process,
        spawn.claude,
        spawn.cwd,
        spawn.env,
        &spawn.args,
        &spawn.session_id,
        spawn.invocation,
    )
    .await
    {
        Ok(()) => {
            spawn.allow_session_strip_on_next_prepare = false;
            Ok(())
        }
        Err(error)
            if is_session_lock_startup_error(&error)
                && spawn.allow_session_strip_on_next_prepare =>
        {
            let stripped_args = strip_session_resume_args(&spawn.args);
            if stripped_args == spawn.args {
                return Err(error);
            }
            logging::event(
                "tty_restart reason=control_update_session_lock action=strip_session_args",
            );
            process.terminate(PTY_TERMINATE_TIMEOUT);
            *process = spawn_tty_process(
                spawn.claude,
                spawn.cwd,
                spawn.env,
                &stripped_args,
                &None,
                spawn.invocation,
            )?;
            prepare_tty_for_prompt_retrying_session_lock(
                process,
                spawn.claude,
                spawn.cwd,
                spawn.env,
                &stripped_args,
                &None,
                spawn.invocation,
            )
            .await?;
            spawn.args = stripped_args;
            spawn.session_id = None;
            spawn.allow_session_strip_on_next_prepare = false;
            tail.reset_for_new_session();
            Ok(())
        }
        Err(error) if is_bad_resume_startup_error(&error) => {
            let stripped_args = strip_session_resume_args(&spawn.args);
            if stripped_args == spawn.args {
                return Err(error);
            }
            logging::event("tty_restart reason=bad_resume action=strip_resume_args");
            process.terminate(PTY_TERMINATE_TIMEOUT);
            *process = spawn_tty_process(
                spawn.claude,
                spawn.cwd,
                spawn.env,
                &stripped_args,
                &None,
                spawn.invocation,
            )?;
            prepare_tty_for_prompt_retrying_session_lock(
                process,
                spawn.claude,
                spawn.cwd,
                spawn.env,
                &stripped_args,
                &None,
                spawn.invocation,
            )
            .await?;
            spawn.args = stripped_args;
            spawn.session_id = None;
            spawn.allow_session_strip_on_next_prepare = false;
            tail.reset_for_new_session();
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn prepare_stream_tty_for_prompt_with_mcp_runtime(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    input: &mut mpsc::Receiver<Value>,
    runtime: &SdkMcpRuntime,
    sdk_state: &mut SdkStreamState,
    spawn: &mut StreamSpawnContext<'_>,
) -> Result<()> {
    let rewritten_spawn_args = args_with_mcp_runtime(&spawn.args, runtime)?;
    match prepare_tty_for_prompt_with_mcp_retrying_session_lock(
        process,
        input,
        runtime,
        sdk_state,
        spawn,
        &rewritten_spawn_args,
        &spawn.session_id,
    )
    .await
    {
        Ok(()) => {
            spawn.allow_session_strip_on_next_prepare = false;
            Ok(())
        }
        Err(error)
            if is_session_lock_startup_error(&error)
                && spawn.allow_session_strip_on_next_prepare =>
        {
            let stripped_args = strip_session_resume_args(&spawn.args);
            if stripped_args == spawn.args {
                return Err(error);
            }
            let rewritten_args = args_with_mcp_runtime(&stripped_args, runtime)?;
            logging::event(
                "tty_restart reason=control_update_session_lock action=strip_session_args",
            );
            process.terminate(PTY_TERMINATE_TIMEOUT);
            *process = spawn_tty_process(
                spawn.claude,
                spawn.cwd,
                spawn.env,
                &rewritten_args,
                &None,
                spawn.invocation,
            )?;
            prepare_tty_for_prompt_with_mcp_retrying_session_lock(
                process,
                input,
                runtime,
                sdk_state,
                spawn,
                &rewritten_args,
                &None,
            )
            .await?;
            spawn.args = stripped_args;
            spawn.session_id = None;
            spawn.allow_session_strip_on_next_prepare = false;
            tail.reset_for_new_session();
            Ok(())
        }
        Err(error) if is_bad_resume_startup_error(&error) => {
            let stripped_args = strip_session_resume_args(&spawn.args);
            if stripped_args == spawn.args {
                return Err(error);
            }
            let rewritten_args = args_with_mcp_runtime(&stripped_args, runtime)?;
            logging::event("tty_restart reason=bad_resume action=strip_resume_args");
            process.terminate(PTY_TERMINATE_TIMEOUT);
            *process = spawn_tty_process(
                spawn.claude,
                spawn.cwd,
                spawn.env,
                &rewritten_args,
                &None,
                spawn.invocation,
            )?;
            prepare_tty_for_prompt_with_mcp_retrying_session_lock(
                process,
                input,
                runtime,
                sdk_state,
                spawn,
                &rewritten_args,
                &None,
            )
            .await?;
            spawn.args = stripped_args;
            spawn.session_id = None;
            spawn.allow_session_strip_on_next_prepare = false;
            tail.reset_for_new_session();
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn prepare_tty_for_prompt_retrying_session_lock(
    process: &mut PtyProcess,
    claude: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    args: &[String],
    session_id: &Option<String>,
    invocation: &Invocation,
) -> Result<()> {
    let started = Instant::now();
    loop {
        match prepare_tty_for_prompt(process).await {
            Ok(()) => return Ok(()),
            Err(error)
                if is_session_lock_startup_error(&error)
                    && started.elapsed() < TTY_SESSION_LOCK_RETRY_TIMEOUT =>
            {
                logging::event("tty_restart reason=session_lock_retry");
                process.terminate(PTY_TERMINATE_TIMEOUT);
                tokio::time::sleep(TTY_SESSION_LOCK_RETRY_DELAY).await;
                *process = spawn_tty_process(claude, cwd, env, args, session_id, invocation)?;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn prepare_tty_for_prompt_with_mcp_retrying_session_lock(
    process: &mut PtyProcess,
    input: &mut mpsc::Receiver<Value>,
    runtime: &SdkMcpRuntime,
    sdk_state: &mut SdkStreamState,
    spawn: &StreamSpawnContext<'_>,
    args: &[String],
    session_id: &Option<String>,
) -> Result<()> {
    let started = Instant::now();
    loop {
        match prepare_tty_for_prompt_with_mcp(process, input, runtime, sdk_state).await {
            Ok(()) => return Ok(()),
            Err(error)
                if is_session_lock_startup_error(&error)
                    && started.elapsed() < TTY_SESSION_LOCK_RETRY_TIMEOUT =>
            {
                logging::event("tty_restart reason=session_lock_retry");
                process.terminate(PTY_TERMINATE_TIMEOUT);
                tokio::time::sleep(TTY_SESSION_LOCK_RETRY_DELAY).await;
                *process = spawn_tty_process(
                    spawn.claude,
                    spawn.cwd,
                    spawn.env,
                    args,
                    session_id,
                    spawn.invocation,
                )?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn spawn_stdin_json_reader() -> mpsc::Receiver<Value> {
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(trimmed) {
                        Ok(value) => {
                            if tx.send(value).await.is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = tx
                                .send(json!({
                                    "type": "cctty_stdin_error",
                                    "error": error.to_string(),
                                }))
                                .await;
                            break;
                        }
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = tx
                        .send(json!({
                            "type": "cctty_stdin_error",
                            "error": error.to_string(),
                        }))
                        .await;
                    break;
                }
            }
        }
    });
    rx
}

async fn service_pending_mcp_proxy_requests(
    input: &mut mpsc::Receiver<Value>,
    sdk_state: &mut SdkStreamState,
) -> Result<()> {
    let Some(runtime) = sdk_state.mcp_runtime.take() else {
        return Ok(());
    };
    let result = service_pending_mcp_proxy_requests_for_runtime(input, &runtime, sdk_state).await;
    sdk_state.mcp_runtime = Some(runtime);
    result
}

#[cfg(unix)]
async fn service_pending_mcp_proxy_requests_for_runtime(
    input: &mut mpsc::Receiver<Value>,
    runtime: &SdkMcpRuntime,
    sdk_state: &mut SdkStreamState,
) -> Result<()> {
    loop {
        let (mut stream, _) = match runtime.listener.accept() {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        stream.set_read_timeout(Some(MCP_PROXY_REQUEST_LINE_TIMEOUT))?;
        let mut line = String::new();
        {
            let mut reader = StdBufReader::new(stream.try_clone()?);
            match reader.read_line(&mut line) {
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    logging::event("mcp_proxy_skip reason=request_line_timeout");
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        let envelope: Value = serde_json::from_str(line.trim())?;
        let server_name = envelope
            .get("server_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let message = envelope.get("message").cloned().unwrap_or(Value::Null);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let sdk_message = rewrite_mcp_tool_call_for_sdk(message);
        let request_id = sdk_state.next_mcp_request_id();
        logging::event(format!(
            "mcp_proxy_request server={} method={}",
            server_name, method
        ));
        let control = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "mcp_message",
                "server_name": server_name,
                "message": sdk_message,
            },
        });
        println!("{}", serde_json::to_string(&control)?);
        std::io::stdout().flush()?;
        let mcp_response = rewrite_mcp_tools_for_claude(
            wait_for_mcp_control_response(input, sdk_state, &request_id).await?,
        );
        if method == "tools/list"
            && let Some(summary) = mcp_tool_names_for_log(&mcp_response)
        {
            logging::event(format!(
                "mcp_proxy_tools server={} tools={summary}",
                server_name
            ));
        }
        writeln!(
            stream,
            "{}",
            serde_json::to_string(&json!({ "mcp_response": mcp_response }))?
        )?;
        stream.flush()?;
    }
}

#[cfg(not(unix))]
async fn service_pending_mcp_proxy_requests_for_runtime(
    _input: &mut mpsc::Receiver<Value>,
    _runtime: &SdkMcpRuntime,
    _sdk_state: &mut SdkStreamState,
) -> Result<()> {
    Ok(())
}

async fn wait_for_mcp_control_response(
    input: &mut mpsc::Receiver<Value>,
    sdk_state: &mut SdkStreamState,
    request_id: &str,
) -> Result<Value> {
    loop {
        let next = tokio::time::timeout(MCP_PROXY_RESPONSE_TIMEOUT, input.recv())
            .await
            .map_err(|_| CcttyError::Timeout("timed out waiting for SDK MCP response".to_owned()))?
            .ok_or_else(|| {
                CcttyError::Usage("stdin closed while waiting for SDK MCP response".to_owned())
            })?;
        if next.get("type").and_then(Value::as_str) == Some("control_response") {
            let response = next.get("response").unwrap_or(&Value::Null);
            if response.get("request_id").and_then(Value::as_str) == Some(request_id) {
                if response.get("subtype").and_then(Value::as_str) == Some("error") {
                    let message = response
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("SDK MCP request failed");
                    return Err(CcttyError::Usage(message.to_owned()));
                }
                return Ok(response
                    .get("response")
                    .and_then(|body| body.get("mcp_response"))
                    .cloned()
                    .unwrap_or(Value::Null));
            }
        }
        sdk_state.deferred_input.push_back(next);
    }
}

async fn submit_prompt_and_tail(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    prompt: &str,
    output_format: OutputFormat,
    include_partial_messages: bool,
) -> Result<TranscriptState> {
    tail.prepare_offset()?;
    submit_prompt_to_tty(process, tail, prompt).await?;
    tail_until_complete(process, tail, output_format, include_partial_messages).await
}

async fn submit_prompt_and_tail_stream(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    input: &mut mpsc::Receiver<Value>,
    sdk_state: &mut SdkStreamState,
    prompt: &str,
    output_format: OutputFormat,
    permission_prompt_tool_stdio: bool,
    include_partial_messages: bool,
) -> Result<TranscriptState> {
    tail.prepare_offset()?;
    submit_prompt_to_tty(process, tail, prompt).await?;
    tail_until_complete_stream(
        process,
        tail,
        input,
        sdk_state,
        output_format,
        permission_prompt_tool_stdio,
        include_partial_messages,
    )
    .await
}

async fn submit_prompt_to_tty(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    prompt: &str,
) -> Result<()> {
    logging::event(format!(
        "prompt_submit start content_chars={}",
        prompt.chars().count()
    ));
    // Confirm the submission via the transcript (an ACK that Claude actually
    // accepted the message) instead of trusting the on-screen state. When Claude
    // is slow to become input-ready — connecting MCP servers, or right after the
    // workspace-trust dialog is dismissed — the paste lands on a transitional
    // screen, the "still editing" heuristic reads false, no Enter is sent, and
    // the message is silently dropped. If no transcript activity appears within
    // the window, clear the line and re-submit (always pressing Enter).
    const ACK_TIMEOUT: Duration = Duration::from_secs(5);
    const MAX_ATTEMPTS: usize = 4;
    for attempt in 0..MAX_ATTEMPTS {
        if attempt == 0 {
            process.write_all(&bracketed_paste_input(prompt))?;
            tokio::time::sleep(Duration::from_millis(120)).await;
            maybe_log_submit_tty_diagnostic(process, "after_paste");
            if tty_output_still_editing_prompt(&process.recent_output(), prompt) {
                process.write_all(b"\r")?;
                tokio::time::sleep(Duration::from_millis(120)).await;
                maybe_log_submit_tty_diagnostic(process, "after_enter");
            }
        } else {
            logging::event(format!(
                "prompt_submit_retry attempt={attempt} reason=no_transcript_ack"
            ));
            process.write_all(b"\x1b")?; // Esc: dismiss any stray menu
            tokio::time::sleep(Duration::from_millis(80)).await;
            process.write_all(b"\x15")?; // Ctrl-U: clear the input line
            tokio::time::sleep(Duration::from_millis(80)).await;
            process.write_all(&bracketed_paste_input(prompt))?;
            tokio::time::sleep(Duration::from_millis(120)).await;
            maybe_log_submit_tty_diagnostic(process, "after_repaste");
            process.write_all(b"\r")?;
            tokio::time::sleep(Duration::from_millis(120)).await;
            maybe_log_submit_tty_diagnostic(process, "after_retry_enter");
        }
        if wait_for_transcript_ack(tail, ACK_TIMEOUT).await? {
            logging::event(format!("prompt_submit done attempt={attempt} ack=true"));
            return Ok(());
        }
    }
    logging::event("prompt_submit done ack=false");
    Ok(())
}

/// Wait until the target transcript shows new bytes — Claude's acknowledgement
/// that it accepted the submitted prompt — or the timeout elapses.
async fn wait_for_transcript_ack(tail: &mut TailCursor, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(path) = tail.resolve_path()? {
            if std::fs::metadata(&path)
                .map(|meta| meta.len() > tail.offset)
                .unwrap_or(false)
            {
                return Ok(true);
            }
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn maybe_log_prompt_diagnostic(prompt: &str) {
    if std::env::var("CCTTY_LOG_PROMPT").ok().as_deref() != Some("1") {
        return;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    prompt.hash(&mut hasher);
    logging::event(format!(
        "prompt_debug chars={} hash={:016x} preview={}",
        prompt.chars().count(),
        hasher.finish(),
        single_line_log_text(&recent_tty_log_text(prompt, 1_000))
    ));
}

fn maybe_log_submit_tty_diagnostic(process: &PtyProcess, stage: &str) {
    if std::env::var("CCTTY_LOG_TTY").ok().as_deref() != Some("1") {
        return;
    }
    logging::event(format!(
        "prompt_submit_tty stage={} tty={} text={}",
        stage,
        tty_wait_class(&process.recent_output()),
        single_line_log_text(&recent_tty_log_text(&process.recent_output(), 1_500))
    ));
}

async fn tail_until_complete(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    output_format: OutputFormat,
    include_partial_messages: bool,
) -> Result<TranscriptState> {
    let started = Instant::now();
    let mut last_activity = Instant::now();
    let mut state = TranscriptState::default();
    let mut tty_debug = TtyDebugLogger::new("text");
    let mut tail_progress = TailProgressLogger::new("text");
    let mut questions = TtyQuestionBridge::new(false);
    let mut visible_progress = TtyVisibleProgress::new(process);

    loop {
        if started.elapsed() > RUN_TIMEOUT {
            logging::event("tail_timeout stream=false");
            return Err(CcttyError::Timeout(
                "timed out waiting for Claude transcript".to_owned(),
            ));
        }

        if let Some(path) = tail.resolve_path()? {
            match read_complete_lines(&path, tail.offset).await {
                Ok((lines, consumed)) if consumed > 0 => {
                    tail.offset += consumed;
                    for line in lines {
                        let mut value: Value = serde_json::from_str(&line)?;
                        tail.externalize_value(&mut value);
                        state.apply(&value);
                        emit_transcript_value(
                            &value,
                            &state,
                            output_format,
                            include_partial_messages,
                        )?;
                        last_activity = Instant::now();
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(CcttyError::Transcript(format!(
                        "failed to read {}: {error}",
                        path.display()
                    )));
                }
            }
        }

        if state.saw_result {
            logging::event("tail_result source=transcript");
            emit_idle_session_state_if_requested(&mut state, output_format)?;
            return Ok(state);
        }
        if questions
            .maybe_handle_tty_question(process, None, None)
            .await?
        {
            last_activity = Instant::now();
        }
        visible_progress.observe(process);
        if !state.assistant_text.is_empty() && last_activity.elapsed() >= COMPLETION_IDLE {
            if output_format == OutputFormat::StreamJson && !state.saw_result {
                let value = synthetic_result(&state, started.elapsed());
                logging::event("tail_result source=synthetic");
                println!("{}", serde_json::to_string(&value)?);
                std::io::stdout().flush()?;
                state.apply(&value);
            }
            emit_idle_session_state_if_requested(&mut state, output_format)?;
            return Ok(state);
        }
        if state.assistant_text.is_empty()
            && visible_progress.completed_without_transcript(process)
            && last_activity.elapsed() >= COMPLETION_IDLE
        {
            if output_format == OutputFormat::StreamJson && !state.saw_result {
                let value = synthetic_prompt_ready_result(&state, started.elapsed());
                logging::event("tail_result source=synthetic_prompt_ready");
                println!("{}", serde_json::to_string(&value)?);
                std::io::stdout().flush()?;
                state.apply(&value);
            }
            emit_idle_session_state_if_requested(&mut state, output_format)?;
            return Ok(state);
        }
        tail_progress.maybe_log(process, tail, &state, started.elapsed());
        tty_debug.maybe_log(process, started.elapsed());
        tokio::time::sleep(TRANSCRIPT_POLL).await;
    }
}

async fn tail_until_complete_stream(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    input: &mut mpsc::Receiver<Value>,
    sdk_state: &mut SdkStreamState,
    output_format: OutputFormat,
    permission_prompt_tool_stdio: bool,
    include_partial_messages: bool,
) -> Result<TranscriptState> {
    let started = Instant::now();
    let mut last_activity = Instant::now();
    let mut state = TranscriptState::default();
    let mut permission = PermissionBridge::new(permission_prompt_tool_stdio);
    let mut questions = TtyQuestionBridge::new(permission_prompt_tool_stdio);
    let mut tty_debug = TtyDebugLogger::new("stream");
    let mut tail_progress = TailProgressLogger::new("stream");
    let mut visible_progress = TtyVisibleProgress::new(process);

    loop {
        if started.elapsed() > RUN_TIMEOUT {
            logging::event("tail_timeout stream=true");
            return Err(CcttyError::Timeout(
                "timed out waiting for Claude transcript".to_owned(),
            ));
        }
        service_pending_mcp_proxy_requests(input, sdk_state).await?;
        if let Some(path) = tail.resolve_path()? {
            match read_complete_lines(&path, tail.offset).await {
                Ok((lines, consumed)) if consumed > 0 => {
                    tail.offset += consumed;
                    for line in lines {
                        let mut value: Value = serde_json::from_str(&line)?;
                        tail.externalize_value(&mut value);
                        permission
                            .maybe_request_tool_permission(process, input, sdk_state, &value)
                            .await?;
                        state.apply(&value);
                        emit_transcript_value(
                            &value,
                            &state,
                            output_format,
                            include_partial_messages,
                        )?;
                        last_activity = Instant::now();
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(CcttyError::Transcript(format!(
                        "failed to read {}: {error}",
                        path.display()
                    )));
                }
            }
        }

        if permission
            .maybe_request_tty_permission(process, input)
            .await?
        {
            last_activity = Instant::now();
        }
        if !permission.handled_ask_user_question()
            && questions
                .maybe_handle_tty_question(process, Some(input), Some(sdk_state))
                .await?
        {
            permission.mark_ask_user_question_handled();
            last_activity = Instant::now();
        }
        visible_progress.observe(process);

        if state.saw_result {
            logging::event("tail_result source=transcript stream=true");
            emit_idle_session_state_if_requested(&mut state, output_format)?;
            return Ok(state);
        }
        if permission.denied_current_turn() && last_activity.elapsed() >= COMPLETION_IDLE {
            if output_format == OutputFormat::StreamJson && !state.saw_result {
                let value = synthetic_permission_denied_result(&state, started.elapsed());
                logging::event("tail_result source=synthetic_permission_denied");
                println!("{}", serde_json::to_string(&value)?);
                std::io::stdout().flush()?;
                state.apply(&value);
            }
            emit_idle_session_state_if_requested(&mut state, output_format)?;
            return Ok(state);
        }
        if !state.assistant_text.is_empty() && last_activity.elapsed() >= COMPLETION_IDLE {
            if output_format == OutputFormat::StreamJson && !state.saw_result {
                let value = synthetic_result(&state, started.elapsed());
                logging::event("tail_result source=synthetic stream=true");
                println!("{}", serde_json::to_string(&value)?);
                std::io::stdout().flush()?;
                state.apply(&value);
            }
            emit_idle_session_state_if_requested(&mut state, output_format)?;
            return Ok(state);
        }
        if state.assistant_text.is_empty()
            && !permission.denied_current_turn()
            && visible_progress.completed_without_transcript(process)
            && last_activity.elapsed() >= COMPLETION_IDLE
        {
            if output_format == OutputFormat::StreamJson && !state.saw_result {
                let value = synthetic_prompt_ready_result(&state, started.elapsed());
                logging::event("tail_result source=synthetic_prompt_ready stream=true");
                println!("{}", serde_json::to_string(&value)?);
                std::io::stdout().flush()?;
                state.apply(&value);
            }
            emit_idle_session_state_if_requested(&mut state, output_format)?;
            return Ok(state);
        }
        tail_progress.maybe_log(process, tail, &state, started.elapsed());
        tty_debug.maybe_log(process, started.elapsed());
        tokio::time::sleep(TRANSCRIPT_POLL).await;
    }
}

fn emit_transcript_value(
    value: &Value,
    state: &TranscriptState,
    output_format: OutputFormat,
    include_partial_messages: bool,
) -> Result<()> {
    if output_format != OutputFormat::StreamJson {
        return Ok(());
    }
    if include_partial_messages {
        for event in synthetic_stream_events_for_assistant(value, state) {
            println!("{}", serde_json::to_string(&event)?);
        }
    }
    println!("{}", serde_json::to_string(value)?);
    std::io::stdout().flush()?;
    Ok(())
}

fn synthetic_stream_events_for_assistant(value: &Value, state: &TranscriptState) -> Vec<Value> {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    let Some(text) = assistant_text_from_value(value).filter(|text| !text.is_empty()) else {
        return Vec::new();
    };
    let message = value.get("message").unwrap_or(value);
    let message_id = message
        .get("id")
        .or_else(|| value.get("uuid"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let session_id = value
        .get("session_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| state.session_id.clone())
        .unwrap_or_default();
    let parent_tool_use_id = value
        .get("parent_tool_use_id")
        .cloned()
        .unwrap_or(Value::Null);
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("<synthetic>");
    let usage = message.get("usage").cloned().unwrap_or_else(zero_usage);
    let base = |event: Value| {
        json!({
            "type": "stream_event",
            "session_id": session_id,
            "uuid": Uuid::new_v4().to_string(),
            "parent_tool_use_id": parent_tool_use_id,
            "event": event,
        })
    };

    vec![
        base(json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": usage,
            }
        })),
        base(json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "text",
                "text": "",
            }
        })),
        base(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "text_delta",
                "text": text,
            }
        })),
        base(json!({
            "type": "content_block_stop",
            "index": 0,
        })),
        base(json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": "end_turn",
                "stop_sequence": null,
            },
            "usage": usage,
        })),
        base(json!({
            "type": "message_stop",
        })),
    ]
}

fn assistant_text_from_value(value: &Value) -> Option<String> {
    let message = value.get("message").unwrap_or(value);
    let content = message.get("content")?;
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|item| {
                    (item.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| item.get("text").and_then(Value::as_str))
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

fn emit_idle_session_state_if_requested(
    state: &mut TranscriptState,
    output_format: OutputFormat,
) -> Result<()> {
    if output_format != OutputFormat::StreamJson || state.saw_idle_session_state {
        return Ok(());
    }
    let value = json!({
        "type": "system",
        "subtype": "session_state_changed",
        "state": "idle",
        "session_id": state.session_id.clone().unwrap_or_default(),
    });
    logging::event("session_state state=idle source=synthetic");
    println!("{}", serde_json::to_string(&value)?);
    std::io::stdout().flush()?;
    state.apply(&value);
    Ok(())
}

struct TtyDebugLogger {
    enabled: bool,
    stage: &'static str,
    last_recent: String,
    next_log: Instant,
}

impl TtyDebugLogger {
    fn new(stage: &'static str) -> Self {
        Self {
            enabled: std::env::var("CCTTY_LOG_TTY").ok().as_deref() == Some("1"),
            stage,
            last_recent: String::new(),
            next_log: Instant::now() + Duration::from_secs(3),
        }
    }

    fn maybe_log(&mut self, process: &PtyProcess, elapsed: Duration) {
        if !self.enabled || Instant::now() < self.next_log {
            return;
        }
        self.next_log = Instant::now() + Duration::from_secs(3);
        let recent = recent_tty_log_text(&process.recent_output(), 1_500);
        if recent.is_empty() || recent == self.last_recent {
            return;
        }
        self.last_recent = recent.clone();
        logging::event(format!(
            "tty_recent stage={} elapsed_ms={} text={}",
            self.stage,
            elapsed.as_millis(),
            single_line_log_text(&recent)
        ));
    }
}

struct TailProgressLogger {
    stage: &'static str,
    next_log: Instant,
}

impl TailProgressLogger {
    fn new(stage: &'static str) -> Self {
        Self {
            stage,
            next_log: Instant::now() + Duration::from_secs(10),
        }
    }

    fn maybe_log(
        &mut self,
        process: &PtyProcess,
        tail: &TailCursor,
        state: &TranscriptState,
        elapsed: Duration,
    ) {
        if Instant::now() < self.next_log {
            return;
        }
        self.next_log = Instant::now() + Duration::from_secs(10);
        logging::event(format!(
            "tail_wait stage={} elapsed_ms={} transcript={} offset={} assistant_chars={} saw_result={} tty={}",
            self.stage,
            elapsed.as_millis(),
            tail.path_log_label(),
            tail.offset,
            state.assistant_text.chars().count(),
            state.saw_result,
            tty_wait_class(&process.recent_output()),
        ));
    }
}

struct TtyVisibleProgress {
    last_snapshot: String,
    last_change: Instant,
    saw_model_activity: bool,
}

impl TtyVisibleProgress {
    fn new(process: &PtyProcess) -> Self {
        Self {
            last_snapshot: tty_progress_snapshot(process),
            last_change: Instant::now(),
            saw_model_activity: false,
        }
    }

    fn observe(&mut self, process: &PtyProcess) {
        let snapshot = tty_progress_snapshot(process);
        if snapshot != self.last_snapshot {
            self.last_snapshot = snapshot;
            self.last_change = Instant::now();
        }
        if tty_output_has_visible_model_activity(&process.recent_output()) {
            self.saw_model_activity = true;
        }
    }

    fn completed_without_transcript(&self, process: &PtyProcess) -> bool {
        self.saw_model_activity
            && tty_wait_class(&process.recent_output()) == "prompt_ready"
            && self.last_change.elapsed() >= TTY_VISIBLE_COMPLETION_IDLE
    }
}

struct PermissionBridge {
    enabled: bool,
    requested_tool_use_ids: HashSet<String>,
    handled_tool_keys: Vec<ToolKey>,
    pending_tty_permission: Option<PendingTtyPermission>,
    last_internal_plan_input: Option<Value>,
    denied_current_turn: bool,
    handled_ask_user_question: bool,
    next_request: u64,
}

impl PermissionBridge {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            requested_tool_use_ids: HashSet::new(),
            handled_tool_keys: Vec::new(),
            pending_tty_permission: None,
            last_internal_plan_input: None,
            denied_current_turn: false,
            handled_ask_user_question: false,
            next_request: 0,
        }
    }

    async fn maybe_request_tty_permission(
        &mut self,
        process: &mut PtyProcess,
        input: &mut mpsc::Receiver<Value>,
    ) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }
        let recent_output = process.recent_output();
        let tool_use_id = format!("cctty_tty_tool_{}", self.next_request);
        let Some(tool_use) =
            tool_use_from_tty_permission_prompt(&recent_output, tool_use_id.clone()).or_else(
                || {
                    tool_use_from_tty_plan_approval_prompt(
                        &recent_output,
                        tool_use_id,
                        self.last_internal_plan_input.as_ref(),
                    )
                },
            )
        else {
            self.pending_tty_permission = None;
            return Ok(false);
        };
        if self.has_handled_tool(&tool_use) {
            self.pending_tty_permission = None;
            return Ok(false);
        }
        let tool_key = ToolKey::from(&tool_use);
        let should_request = if let Some(pending) = &mut self.pending_tty_permission {
            let pending_key = ToolKey::from(&pending.tool_use);
            if pending_key.matches(&tool_key) || pending.tool_use.name == tool_use.name {
                pending.tool_use = tool_use;
                pending.first_seen.elapsed() >= TTY_PERMISSION_TRANSCRIPT_GRACE
            } else {
                *pending = PendingTtyPermission {
                    tool_use,
                    first_seen: Instant::now(),
                };
                false
            }
        } else {
            self.pending_tty_permission = Some(PendingTtyPermission {
                tool_use,
                first_seen: Instant::now(),
            });
            false
        };
        if !should_request {
            return Ok(false);
        }
        let tool_use = self
            .pending_tty_permission
            .take()
            .expect("pending TTY permission exists after grace")
            .tool_use;
        self.mark_tool_handled(&tool_use);
        self.request_permission(process, input, &tool_use).await?;
        Ok(true)
    }

    async fn maybe_request_tool_permission(
        &mut self,
        process: &mut PtyProcess,
        input: &mut mpsc::Receiver<Value>,
        sdk_state: &mut SdkStreamState,
        transcript: &Value,
    ) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }

        let mut requested = false;
        for tool_use in tool_uses_from_assistant(transcript) {
            if is_mcp_tool_use(&tool_use.name) {
                continue;
            }
            if !self.requested_tool_use_ids.insert(tool_use.id.clone()) {
                continue;
            }
            if self.has_handled_tool(&tool_use) {
                continue;
            }
            if is_internal_claude_plan_write(&tool_use) {
                self.mark_tool_handled(&tool_use);
                self.last_internal_plan_input = internal_plan_input_from_write(&tool_use);
                if self.pending_tty_permission_matches(&tool_use) {
                    self.pending_tty_permission = None;
                }
                logging::event(format!(
                    "permission_skip_internal_plan_write tool_use_id={} path={}",
                    tool_use.id,
                    single_line_log_text(
                        tool_use
                            .input
                            .get("file_path")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                    )
                ));
                continue;
            }
            if is_internal_exit_plan_tool_search(&tool_use) {
                self.mark_tool_handled(&tool_use);
                logging::event(format!(
                    "permission_skip_internal_exit_plan_tool_search tool_use_id={}",
                    tool_use.id
                ));
                continue;
            }
            requested = true;
            self.mark_tool_handled(&tool_use);
            if tool_use.name == "AskUserQuestion" {
                self.handled_ask_user_question = true;
            }
            if self.pending_tty_permission_matches(&tool_use) {
                self.pending_tty_permission = None;
            }
            if tool_use.name == "AskUserQuestion"
                && self
                    .request_ask_user_question_via_sdk_mcp(process, input, sdk_state, &tool_use)
                    .await?
            {
                continue;
            }
            self.request_permission(process, input, &tool_use).await?;
        }
        Ok(requested)
    }

    async fn request_ask_user_question_via_sdk_mcp(
        &mut self,
        process: &mut PtyProcess,
        input: &mut mpsc::Receiver<Value>,
        sdk_state: &mut SdkStreamState,
        tool_use: &ToolUse,
    ) -> Result<bool> {
        let Some(server_name) = sdk_state.sdk_mcp_server_names().into_iter().next() else {
            return Ok(false);
        };
        let request_id = sdk_state.next_mcp_request_id();
        let message_id = request_id.clone();
        let control = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "mcp_message",
                "server_name": server_name,
                "message": {
                    "jsonrpc": "2.0",
                    "id": message_id,
                        "method": "tools/call",
                        "params": {
                            "name": "AskUserQuestion",
                            "arguments": sdk_ask_user_question_input(&tool_use.input),
                        },
                    },
                },
        });
        logging::event(format!(
            "permission_request_mcp server={} tool={} tool_use_id={}",
            server_name, tool_use.name, tool_use.id
        ));
        println!("{}", serde_json::to_string(&control)?);
        std::io::stdout().flush()?;
        let mcp_response = wait_for_mcp_control_response(input, sdk_state, &request_id).await?;
        let feedback = ask_user_question_feedback_from_mcp_response(&mcp_response, &tool_use.input)
            .or_else(|| ask_user_question_default_feedback(&tool_use.input))
            .unwrap_or_else(|| "用户已经回答了表单，请根据已有回答继续。".to_owned());
        logging::event(format!(
            "permission_response_mcp server={} tool={} feedback={}",
            server_name,
            tool_use.name,
            single_line_log_text(&feedback)
        ));
        let _ = wait_for_tty_question_form(process).await;
        cancel_tty_question(process, Some(&feedback)).await?;
        Ok(true)
    }

    async fn request_permission(
        &mut self,
        process: &mut PtyProcess,
        input: &mut mpsc::Receiver<Value>,
        tool_use: &ToolUse,
    ) -> Result<()> {
        let request_id = format!("cctty_permission_{}", self.next_request);
        self.next_request += 1;
        let request = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "can_use_tool",
                "tool_name": tool_use.name,
                "input": tool_use.input,
                "permission_suggestions": null,
                "blocked_path": null,
                "tool_use_id": tool_use.id,
            }
        });
        logging::event(format!(
            "permission_request tool={} tool_use_id={}",
            tool_use.name, tool_use.id
        ));
        println!("{}", serde_json::to_string(&request)?);
        std::io::stdout().flush()?;
        let response = wait_for_control_response(input, &request_id).await?;
        if apply_permission_decision(process, tool_use, &response).await? {
            self.denied_current_turn = true;
            logging::event(format!(
                "permission_response tool={} behavior=deny",
                tool_use.name
            ));
        } else {
            logging::event(format!(
                "permission_response tool={} behavior=allow",
                tool_use.name
            ));
        }
        Ok(())
    }

    fn has_handled_tool(&self, tool_use: &ToolUse) -> bool {
        if tool_use.name == "AskUserQuestion" && self.handled_ask_user_question {
            return true;
        }
        let key = ToolKey::from(tool_use);
        self.handled_tool_keys
            .iter()
            .any(|handled| handled.matches(&key))
    }

    fn mark_tool_handled(&mut self, tool_use: &ToolUse) {
        self.handled_tool_keys.push(ToolKey::from(tool_use));
    }

    fn denied_current_turn(&self) -> bool {
        self.denied_current_turn
    }

    fn handled_ask_user_question(&self) -> bool {
        self.handled_ask_user_question
    }

    fn mark_ask_user_question_handled(&mut self) {
        self.handled_ask_user_question = true;
    }

    fn pending_tty_permission_matches(&self, tool_use: &ToolUse) -> bool {
        self.pending_tty_permission
            .as_ref()
            .map(|pending| {
                ToolKey::from(&pending.tool_use).matches(&ToolKey::from(tool_use))
                    || pending.tool_use.name == tool_use.name
            })
            .unwrap_or(false)
    }
}

struct TtyQuestionBridge {
    enabled: bool,
    handled_questions: HashSet<String>,
    pending: Option<PendingTtyQuestion>,
    next_request: u64,
}

impl TtyQuestionBridge {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            handled_questions: HashSet::new(),
            pending: None,
            next_request: 0,
        }
    }

    async fn maybe_handle_tty_question(
        &mut self,
        process: &mut PtyProcess,
        input: Option<&mut mpsc::Receiver<Value>>,
        sdk_state: Option<&mut SdkStreamState>,
    ) -> Result<bool> {
        let Some(question) = tty_question_from_form(&process.recent_output()) else {
            self.pending = None;
            return Ok(false);
        };
        if self.handled_questions.contains(&question.question) {
            return Ok(false);
        }
        let should_handle = if let Some(pending) = &mut self.pending {
            if pending.question.question == question.question {
                pending.question = question;
                pending.first_seen.elapsed() >= TTY_QUESTION_FORM_SETTLE
            } else {
                *pending = PendingTtyQuestion {
                    question,
                    first_seen: Instant::now(),
                };
                false
            }
        } else {
            self.pending = Some(PendingTtyQuestion {
                question,
                first_seen: Instant::now(),
            });
            false
        };
        if !should_handle {
            return Ok(false);
        }
        let question = self
            .pending
            .take()
            .expect("pending TTY question exists after grace")
            .question;
        self.handled_questions.insert(question.question.clone());
        if !self.enabled {
            logging::event(format!(
                "question_response question={} behavior=cancel source=auto",
                single_line_log_text(&question.question)
            ));
            cancel_tty_question(process, Some(default_question_decline_feedback())).await?;
            return Ok(true);
        }
        let Some(input) = input else {
            return Ok(false);
        };
        self.request_question(process, input, sdk_state, question)
            .await?;
        Ok(true)
    }

    async fn request_question(
        &mut self,
        process: &mut PtyProcess,
        input: &mut mpsc::Receiver<Value>,
        sdk_state: Option<&mut SdkStreamState>,
        question: TtyQuestion,
    ) -> Result<()> {
        if let Some(sdk_state) = sdk_state
            && self
                .request_question_via_sdk_mcp(process, input, sdk_state, &question)
                .await?
        {
            return Ok(());
        }
        let request_id = format!("cctty_question_{}", self.next_request);
        self.next_request += 1;
        let tool_use = ToolUse {
            id: request_id.clone(),
            name: "AskUserQuestion".to_owned(),
            input: question.to_tool_input(),
        };
        let request = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "can_use_tool",
                "tool_name": tool_use.name,
                "input": tool_use.input,
                "permission_suggestions": null,
                "blocked_path": null,
                "tool_use_id": tool_use.id,
                "description": question.question,
            }
        });
        logging::event(format!(
            "question_request question={} options={}",
            single_line_log_text(&question.question),
            question
                .options
                .iter()
                .filter(|option| !option.special)
                .count()
        ));
        println!("{}", serde_json::to_string(&request)?);
        std::io::stdout().flush()?;
        let response = wait_for_control_response(input, &tool_use.id).await?;
        apply_question_decision(process, &question, &response).await?;
        Ok(())
    }

    async fn request_question_via_sdk_mcp(
        &mut self,
        process: &mut PtyProcess,
        input: &mut mpsc::Receiver<Value>,
        sdk_state: &mut SdkStreamState,
        question: &TtyQuestion,
    ) -> Result<bool> {
        let Some(server_name) = sdk_state.sdk_mcp_server_names().into_iter().next() else {
            return Ok(false);
        };
        let request_id = sdk_state.next_mcp_request_id();
        let message_id = request_id.clone();
        let tool_input = question.to_tool_input();
        let control = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "mcp_message",
                "server_name": server_name,
                "message": {
                    "jsonrpc": "2.0",
                    "id": message_id,
                        "method": "tools/call",
                        "params": {
                            "name": "AskUserQuestion",
                            "arguments": sdk_ask_user_question_input(&tool_input),
                        },
                    },
                },
        });
        logging::event(format!(
            "question_request_mcp server={} question={} options={}",
            server_name,
            single_line_log_text(&question.question),
            question
                .options
                .iter()
                .filter(|option| !option.special)
                .count()
        ));
        println!("{}", serde_json::to_string(&control)?);
        std::io::stdout().flush()?;
        let mcp_response = wait_for_mcp_control_response(input, sdk_state, &request_id).await?;
        let feedback = question_feedback_from_mcp_response(&mcp_response, question)
            .unwrap_or_else(|| default_question_answer_feedback(question));
        logging::event(format!(
            "question_response_mcp server={} question={} feedback={}",
            server_name,
            single_line_log_text(&question.question),
            single_line_log_text(&feedback)
        ));
        cancel_tty_question(process, Some(&feedback)).await?;
        Ok(true)
    }
}

#[derive(Debug)]
struct PendingTtyPermission {
    tool_use: ToolUse,
    first_seen: Instant,
}

#[derive(Debug)]
struct PendingTtyQuestion {
    question: TtyQuestion,
    first_seen: Instant,
}

#[derive(Debug, Clone)]
struct ToolUse {
    id: String,
    name: String,
    input: Value,
}

#[derive(Debug, Clone)]
struct ToolKey {
    name: String,
    command: Option<String>,
    file_path: Option<String>,
    input: String,
}

#[derive(Debug, Clone)]
struct TtyQuestion {
    question: String,
    header: String,
    options: Vec<TtyQuestionOption>,
}

impl TtyQuestion {
    fn to_tool_input(&self) -> Value {
        let options = self
            .options
            .iter()
            .filter(|option| !option.special)
            .map(|option| {
                json!({
                    "label": option.label,
                    "description": option.description,
                })
            })
            .collect::<Vec<_>>();
        json!({
            "questions": [
                {
                    "question": self.question,
                    "header": self.header,
                    "options": options,
                    "multiSelect": false,
                }
            ]
        })
    }
}

#[derive(Debug, Clone)]
struct TtyQuestionOption {
    label: String,
    description: String,
    special: bool,
}

impl ToolKey {
    fn from(tool_use: &ToolUse) -> Self {
        Self {
            name: tool_use.name.clone(),
            command: tool_use
                .input
                .get("command")
                .and_then(Value::as_str)
                .map(normalize_tool_command),
            file_path: tool_use
                .input
                .get("file_path")
                .and_then(Value::as_str)
                .map(normalize_tool_path),
            input: serde_json::to_string(&tool_use.input).unwrap_or_default(),
        }
    }

    fn matches(&self, other: &Self) -> bool {
        if self.name == "ExitPlanMode" && other.name == "ExitPlanMode" {
            return true;
        }
        if is_file_tool(&self.name)
            && is_file_tool(&other.name)
            && let (Some(left), Some(right)) = (&self.file_path, &other.file_path)
        {
            return left == right;
        }
        if self.name != other.name {
            return false;
        }
        match (&self.command, &other.command) {
            (Some(left), Some(right)) if !left.is_empty() && !right.is_empty() => {
                commands_match(left, right)
            }
            _ => self.input == other.input,
        }
    }
}

fn tty_question_from_form(output: &str) -> Option<TtyQuestion> {
    let plain = plain_tty_output(output);
    if !(plain.contains("Enter to select")
        && plain.contains("Esc to cancel")
        && plain.contains("Type something"))
    {
        return None;
    }
    let form = tty_question_form_region(&plain)?;
    let option_start = form.find("❯ 1. ").or_else(|| form.find("1. "))?;
    let prompt = form[..option_start].trim();
    let header_hint = tty_question_header(&plain);
    let (header, question) = split_tty_question_prompt(prompt, header_hint.as_deref());
    if question.is_empty() {
        return None;
    }
    let options = numbered_question_options(&form[option_start..]);
    if options.iter().filter(|option| !option.special).count() < 2 {
        return None;
    }
    Some(TtyQuestion {
        question,
        header,
        options,
    })
}

fn tty_question_form_region(plain: &str) -> Option<&str> {
    let after_submit = plain
        .rsplit_once("✔ Submit →")
        .map(|(_, after)| after.trim())
        .filter(|after| after.contains("1. "));
    if after_submit.is_some() {
        return after_submit;
    }
    plain
        .rsplit_once('☐')
        .map(|(_, after)| after.trim())
        .filter(|after| after.contains("1. "))
        .or(Some(plain))
}

fn tty_question_header(plain: &str) -> Option<String> {
    let before_submit = plain.rsplit_once("✔ Submit →")?.0;
    before_submit
        .rsplit_once('☐')
        .map(|(_, header)| header.trim())
        .filter(|header| !header.is_empty())
        .map(short_header)
}

fn split_tty_question_prompt(prompt: &str, header_hint: Option<&str>) -> (String, String) {
    let prompt = prompt.trim();
    if let Some(header) = header_hint
        && let Some(question) = prompt.strip_prefix(header)
    {
        let question = question.trim();
        if !question.is_empty() {
            return (short_header(header), question.to_owned());
        }
    }
    if let Some((header, question)) = prompt.split_once(' ') {
        let question = question.trim();
        if looks_like_question_text(question) && !looks_like_question_leader(header) {
            return (short_header(header), question.to_owned());
        }
    }
    if let Some(header) = header_hint {
        return (short_header(header), prompt.to_owned());
    }
    (short_header(prompt), prompt.to_owned())
}

fn looks_like_question_leader(text: &str) -> bool {
    matches!(
        text.trim_matches(|ch: char| !ch.is_alphanumeric())
            .to_ascii_lowercase()
            .as_str(),
        "what" | "which" | "how" | "who" | "when" | "where" | "why"
    )
}

fn looks_like_question_text(text: &str) -> bool {
    text.ends_with('?')
        || text.ends_with('？')
        || text.ends_with(':')
        || text.ends_with('：')
        || text.starts_with("Which ")
        || text.starts_with("What ")
        || text.starts_with("How ")
        || text.starts_with("请选择")
        || text.starts_with("请")
}

fn short_header(text: &str) -> String {
    let mut header = text.chars().take(12).collect::<String>().trim().to_owned();
    if header.is_empty() {
        header = "Question".to_owned();
    }
    header
}

fn numbered_question_options(text: &str) -> Vec<TtyQuestionOption> {
    let mut markers = Vec::new();
    for index in 1..=9 {
        let marker = format!("{index}. ");
        if let Some(pos) = text.find(&marker) {
            markers.push((index, pos + marker.len()));
        }
    }
    markers.sort_by_key(|(_, pos)| *pos);
    let mut options = Vec::new();
    for (marker_index, (_, start)) in markers.iter().enumerate() {
        let end = markers
            .get(marker_index + 1)
            .map(|(_, pos)| pos.saturating_sub(3))
            .unwrap_or(text.len());
        let Some(raw) = text.get(*start..end) else {
            continue;
        };
        let value = trim_tty_option_text(raw);
        if value.is_empty() {
            continue;
        }
        let special = value.eq_ignore_ascii_case("type something")
            || value.eq_ignore_ascii_case("type something.")
            || value.eq_ignore_ascii_case("chat about this");
        let (label, description) = split_question_option(&value);
        options.push(TtyQuestionOption {
            label,
            description,
            special,
        });
    }
    options
}

fn trim_tty_option_text(raw: &str) -> String {
    raw.split("Enter to select")
        .next()
        .unwrap_or(raw)
        .split('─')
        .next()
        .unwrap_or(raw)
        .trim()
        .trim_end_matches('.')
        .trim()
        .to_owned()
}

fn split_question_option(value: &str) -> (String, String) {
    let words = value.split_whitespace().collect::<Vec<_>>();
    if words.len() == 2 && words[1].starts_with(words[0]) {
        return (words[0].to_owned(), words[1].to_owned());
    }
    if words.len() <= 3 {
        return (value.to_owned(), String::new());
    }
    let label = words.iter().take(3).copied().collect::<Vec<_>>().join(" ");
    let description = words.iter().skip(3).copied().collect::<Vec<_>>().join(" ");
    (label, description)
}

fn is_file_tool(name: &str) -> bool {
    matches!(name, "Write" | "Edit" | "MultiEdit")
}

fn is_mcp_tool_use(name: &str) -> bool {
    name.starts_with("mcp__")
}

fn normalize_tool_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_tool_path(path: &str) -> String {
    path.trim().replace('\\', "/")
}

fn commands_match(left: &str, right: &str) -> bool {
    if left == right || left.contains(right) || right.contains(left) {
        return true;
    }
    let left_compact = left.split_whitespace().collect::<String>();
    let right_compact = right.split_whitespace().collect::<String>();
    if left_compact == right_compact {
        return true;
    }
    let left_command = left.split_whitespace().next();
    let right_command = right.split_whitespace().next();
    if left_command != right_command {
        return false;
    }
    let min_len = left_compact.len().min(right_compact.len());
    let len_delta = left_compact.len().abs_diff(right_compact.len());
    min_len >= 20 && len_delta <= 2 && edit_distance_at_most(&left_compact, &right_compact, 2)
}

fn edit_distance_at_most(left: &str, right: &str, limit: usize) -> bool {
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    if left.len().abs_diff(right.len()) > limit {
        return false;
    }
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];
    for (left_index, left_ch) in left.iter().enumerate() {
        current[0] = left_index + 1;
        let mut row_min = current[0];
        for (right_index, right_ch) in right.iter().enumerate() {
            let substitution_cost = usize::from(left_ch != right_ch);
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + substitution_cost);
            row_min = row_min.min(current[right_index + 1]);
        }
        if row_min > limit {
            return false;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()] <= limit
}

fn tool_uses_from_assistant(value: &Value) -> Vec<ToolUse> {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
                .filter_map(|item| {
                    Some(ToolUse {
                        id: item.get("id").and_then(Value::as_str)?.to_owned(),
                        name: item.get("name").and_then(Value::as_str)?.to_owned(),
                        input: item.get("input").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn is_internal_claude_plan_write(tool_use: &ToolUse) -> bool {
    if tool_use.name != "Write" {
        return false;
    }
    let Some(file_path) = tool_use.input.get("file_path").and_then(Value::as_str) else {
        return false;
    };
    let normalized = file_path.replace('\\', "/");
    normalized.ends_with(".md")
        && (normalized.contains("/.claude/plans/")
            || normalized.starts_with(".claude/plans/")
            || normalized.starts_with("~/.claude/plans/"))
}

fn internal_plan_input_from_write(tool_use: &ToolUse) -> Option<Value> {
    if !is_internal_claude_plan_write(tool_use) {
        return None;
    }
    let mut input = serde_json::Map::new();
    if let Some(content) = tool_use.input.get("content").and_then(Value::as_str)
        && !content.trim().is_empty()
    {
        input.insert("plan".to_owned(), Value::String(content.to_owned()));
    }
    if let Some(file_path) = tool_use.input.get("file_path").and_then(Value::as_str)
        && !file_path.trim().is_empty()
    {
        input.insert(
            "planFilePath".to_owned(),
            Value::String(file_path.to_owned()),
        );
    }
    (!input.is_empty()).then_some(Value::Object(input))
}

fn is_internal_exit_plan_tool_search(tool_use: &ToolUse) -> bool {
    if tool_use.name != "ToolSearch" {
        return false;
    }
    tool_use
        .input
        .get("query")
        .and_then(Value::as_str)
        .is_some_and(|query| query.trim().eq_ignore_ascii_case("select:ExitPlanMode"))
}

fn tool_use_from_tty_permission_prompt(output: &str, tool_use_id: String) -> Option<ToolUse> {
    let plain_output = plain_tty_output(output);
    if plain_tty_output_has_permission_prompt(&plain_output) {
        let command = bash_command_from_tty_permission_prompt(&output)?;
        return Some(ToolUse {
            id: tool_use_id,
            name: "Bash".to_owned(),
            input: json!({ "command": command }),
        });
    }
    file_tool_use_from_tty_permission_prompt(&plain_output, tool_use_id)
}

fn tool_use_from_tty_plan_approval_prompt(
    output: &str,
    tool_use_id: String,
    plan_input: Option<&Value>,
) -> Option<ToolUse> {
    tty_output_has_plan_approval_prompt(output).then(|| ToolUse {
        id: tool_use_id,
        name: "ExitPlanMode".to_owned(),
        input: plan_input.cloned().unwrap_or_else(|| {
            json!({
                "plan": tty_plan_text_from_approval_prompt(output)
                    .unwrap_or_else(|| "Plan is ready for approval.".to_owned()),
            })
        }),
    })
}

fn tty_plan_text_from_approval_prompt(output: &str) -> Option<String> {
    let output = plain_tty_output(output);
    let before_prompt = if let Some((before, _)) = output.split_once("Claude has written up a plan")
    {
        before
    } else if let Some((before, _)) = output.split_once("Claude wrote up a plan") {
        before
    } else {
        return None;
    };
    let plan = before_prompt
        .trim_matches(|ch: char| {
            ch.is_whitespace() || matches!(ch, '─' | '━' | '╌' | '╭' | '╮' | '╰' | '╯' | '│')
        })
        .trim();
    if plan.is_empty() {
        return None;
    }
    let char_count = plan.chars().count();
    let plan = if char_count > 2_000 {
        plan.chars()
            .skip(char_count.saturating_sub(2_000))
            .collect::<String>()
    } else {
        plan.to_owned()
    };
    Some(plan)
}

fn bash_command_from_tty_permission_prompt(output: &str) -> Option<String> {
    if let Some(command) = bash_structured_command_from_tty_output(output) {
        return Some(command);
    }
    if let Some(command) = bash_parenthetical_command_from_tty_output(output) {
        return Some(command);
    }
    let plain_output = plain_tty_output(output);
    let rest = plain_output.split("Bash command ").nth(1)?;
    let mut command = rest;
    for marker in [
        " Permission rule ",
        " /permissions to update rules",
        " Do you want to proceed?",
        " Do you want",
    ] {
        if let Some((before, _)) = command.split_once(marker) {
            command = before;
        }
    }
    let command = normalize_tty_bash_command(command);
    valid_tty_bash_command_candidate(&command).then_some(command)
}

fn bash_structured_command_from_tty_output(output: &str) -> Option<String> {
    let lines = visible_tty_lines(output);
    let spaced_lines = visible_tty_lines_preserving_spacing(output);
    for (index, line) in lines.iter().enumerate().rev() {
        if line != "Bash command" {
            continue;
        }
        let command = lines
            .iter()
            .zip(spaced_lines.iter())
            .skip(index + 1)
            .find_map(|(candidate, spaced_candidate)| {
                if candidate.starts_with("Permission rule")
                    || candidate.starts_with("/permissions")
                    || candidate.starts_with("Do you want")
                {
                    return None;
                }
                bash_command_from_visible_tty_line(candidate, spaced_candidate)
            })?;
        if valid_tty_bash_command_candidate(&command) {
            return Some(command);
        }
    }
    None
}

fn bash_command_from_visible_tty_line(line: &str, spaced_line: &str) -> Option<String> {
    let first_column = first_tty_column(spaced_line);
    let command = if valid_tty_bash_command_candidate(&first_column) {
        first_column
    } else {
        line
    };
    let command = normalize_tty_bash_command(command);
    valid_tty_bash_command_candidate(&command).then_some(command)
}

fn first_tty_column(line: &str) -> &str {
    let mut spaces = 0_usize;
    let mut start = None;
    for (index, ch) in line.char_indices() {
        if ch == ' ' {
            spaces += 1;
            if spaces == 3 {
                start = Some(index - 2);
            }
        } else {
            if let Some(start) = start {
                return line[..start].trim();
            }
            spaces = 0;
        }
    }
    line.trim()
}

fn strip_tty_bash_display_suffix(command: &str) -> &str {
    for suffix in [
        " Run shell command",
        " Run command",
        " Execute shell command",
        " Execute command",
    ] {
        if let Some((before, _)) = command.rsplit_once(suffix) {
            return before.trim();
        }
    }
    command
}

fn normalize_tty_bash_command(command: &str) -> String {
    normalize_tool_command(strip_tty_bash_display_suffix(command))
}

fn bash_parenthetical_command_from_tty_output(output: &str) -> Option<String> {
    let indices = output.match_indices("Bash(").collect::<Vec<_>>();
    for (index, _) in indices.into_iter().rev() {
        let after = &output[index + "Bash(".len()..];
        let Some((command, _)) = after.split_once(')') else {
            continue;
        };
        let command = normalize_tty_bash_command(command);
        if valid_tty_bash_command_candidate(&command) && !command.contains(":*") {
            return Some(command);
        }
    }
    None
}

fn valid_tty_bash_command_candidate(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() || command.chars().any(|ch| ch == '\u{1b}' || ch.is_control()) {
        return false;
    }
    let lower = command.to_ascii_lowercase();
    if lower.starts_with("execution request")
        || lower.starts_with("review ")
        || lower.starts_with("request ")
    {
        return false;
    }
    let words = lower
        .split_whitespace()
        .map(|word| word.trim_matches(|ch: char| !ch.is_ascii_alphanumeric()))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    if words.len() <= 4
        && words.iter().all(|word| {
            matches!(
                *word,
                "bash" | "command" | "execution" | "request" | "review" | "approval"
            )
        })
    {
        return false;
    }
    true
}

fn file_tool_use_from_tty_permission_prompt(output: &str, tool_use_id: String) -> Option<ToolUse> {
    if !plain_tty_output_has_file_permission_prompt(output) {
        return None;
    }
    let (name, file_path) = file_permission_tool_and_path(output)?;
    Some(ToolUse {
        id: tool_use_id,
        name: name.to_owned(),
        input: json!({ "file_path": file_path }),
    })
}

fn file_permission_tool_and_path(output: &str) -> Option<(&'static str, String)> {
    for (marker, tool_name) in [
        ("Do you want to create ", "Write"),
        ("Do you want to write ", "Write"),
        ("Do you want to edit ", "Edit"),
        ("Do you want to update ", "Edit"),
    ] {
        let Some((_, after)) = output.rsplit_once(marker) else {
            continue;
        };
        let mut path = after;
        for terminator in [
            " ?",
            "?",
            " ❯",
            " 1. Yes",
            " Esc to cancel",
            " Tab to amend",
        ] {
            if let Some((before, _)) = path.split_once(terminator) {
                path = before;
            }
        }
        let path = path
            .trim()
            .trim_matches('`')
            .trim_matches('"')
            .trim()
            .to_owned();
        if !path.is_empty() {
            return Some((tool_name, path));
        }
    }
    None
}

async fn wait_for_control_response(
    input: &mut mpsc::Receiver<Value>,
    request_id: &str,
) -> Result<Value> {
    loop {
        let value = input.recv().await.ok_or_else(|| {
            CcttyError::Tty(format!(
                "stdin closed while waiting for control_response {request_id}"
            ))
        })?;
        if value.get("type").and_then(Value::as_str) == Some("cctty_stdin_error") {
            return Err(CcttyError::Usage(
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("invalid stdin JSON")
                    .to_owned(),
            ));
        }
        if value.get("type").and_then(Value::as_str) != Some("control_response") {
            continue;
        }
        let matches_request = value
            .get("response")
            .and_then(|response| response.get("request_id"))
            .and_then(Value::as_str)
            == Some(request_id);
        if matches_request {
            return Ok(value);
        }
    }
}

fn permission_behavior(control_response: &Value) -> Option<String> {
    control_response
        .get("response")
        .and_then(|response| response.get("response"))
        .and_then(|response| {
            response
                .get("behavior")
                .or_else(|| response.get("decision"))
        })
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn permission_deny_message(control_response: &Value) -> Option<String> {
    let response = control_response
        .get("response")
        .and_then(|response| response.get("response"))?;
    response
        .get("message")
        .or_else(|| response.get("reason"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(ToOwned::to_owned)
}

async fn apply_permission_decision(
    process: &mut PtyProcess,
    tool_use: &ToolUse,
    control_response: &Value,
) -> Result<bool> {
    if tool_use.name == "ExitPlanMode" {
        return apply_exit_plan_mode_decision(process, control_response).await;
    }
    if tool_use.name == "AskUserQuestion" {
        return apply_ask_user_question_decision(process, tool_use, control_response).await;
    }
    let behavior = permission_behavior(control_response)
        .unwrap_or_else(|| "allow".to_owned())
        .to_ascii_lowercase();
    let denied = behavior == "deny";
    let saw_prompt = wait_for_tty_permission_prompt(process, tool_use).await;
    if denied {
        if saw_prompt {
            process.write_all(deny_selection_for_tty_permission_prompt(
                &process.recent_output(),
                tool_use,
            ))?;
            if let Some(message) = permission_deny_message(control_response)
                && wait_for_tty_permission_feedback_prompt(process).await
            {
                process.write_all(&bracketed_paste_input(&message))?;
            }
        } else {
            process.write_all(b"\x1b")?;
        }
    } else if saw_prompt {
        process.write_all(b"1\r")?;
    }
    Ok(denied)
}

async fn apply_exit_plan_mode_decision(
    process: &mut PtyProcess,
    control_response: &Value,
) -> Result<bool> {
    let behavior = permission_behavior(control_response)
        .unwrap_or_else(|| "allow".to_owned())
        .to_ascii_lowercase();
    let denied = matches!(behavior.as_str(), "deny" | "decline" | "cancel" | "error");
    let saw_prompt = wait_for_tty_plan_approval_prompt(process).await;
    if denied {
        if saw_prompt {
            let feedback = permission_deny_message(control_response);
            if feedback.is_some()
                && tty_plan_approval_prompt_has_feedback_choice(&process.recent_output())
            {
                process.write_all(b"4\r")?;
                if wait_for_tty_permission_feedback_prompt(process).await
                    && let Some(message) = feedback
                {
                    process.write_all(&bracketed_paste_input(&message))?;
                }
            } else {
                process.write_all(b"3\r")?;
            }
        } else {
            process.write_all(b"\x1b")?;
        }
    } else if saw_prompt {
        process.write_all(exit_plan_mode_allow_selection(control_response))?;
    }
    Ok(denied)
}

async fn apply_ask_user_question_decision(
    process: &mut PtyProcess,
    tool_use: &ToolUse,
    control_response: &Value,
) -> Result<bool> {
    let behavior = permission_behavior(control_response)
        .unwrap_or_else(|| "allow".to_owned())
        .to_ascii_lowercase();
    let denied = behavior == "deny";
    let feedback = if denied {
        permission_deny_message(control_response)
            .unwrap_or_else(|| default_question_decline_feedback().to_owned())
    } else {
        ask_user_question_feedback_from_response(control_response, tool_use)
            .unwrap_or_else(|| "用户已经回答了表单，请根据已有回答继续。".to_owned())
    };
    let _ = wait_for_tty_question_form(process).await;
    cancel_tty_question(process, Some(&feedback)).await?;
    Ok(denied)
}

fn ask_user_question_feedback_from_response(
    control_response: &Value,
    tool_use: &ToolUse,
) -> Option<String> {
    let body = control_response
        .get("response")
        .and_then(|response| response.get("response"))?;
    for candidate in question_answer_candidates(body) {
        if let Some(feedback) = feedback_from_answers(candidate.get("answers")) {
            return Some(feedback);
        }
        if let Some(feedback) = feedback_from_answers(candidate.get("content")) {
            return Some(feedback);
        }
    }
    ask_user_question_default_feedback(&tool_use.input)
}

fn ask_user_question_feedback_from_mcp_response(
    mcp_response: &Value,
    input: &Value,
) -> Option<String> {
    let body = mcp_response.get("result").unwrap_or(mcp_response);
    for candidate in question_answer_candidates(body) {
        if let Some(feedback) = feedback_from_answers(candidate.get("answers")) {
            return Some(feedback);
        }
        if let Some(answer) = mcp_text_content(candidate.get("content")) {
            return Some(answer);
        }
        if let Some(feedback) = feedback_from_answers(candidate.get("content")) {
            return Some(feedback);
        }
    }
    ask_user_question_default_feedback(input)
}

fn sdk_ask_user_question_input(input: &Value) -> Value {
    let mut rewritten = input.clone();
    let Some(questions) = rewritten.get_mut("questions").and_then(Value::as_array_mut) else {
        return rewritten;
    };
    for question in questions {
        let Some(options) = question.get_mut("options").and_then(Value::as_array_mut) else {
            continue;
        };
        let flattened = options
            .iter()
            .filter_map(|option| {
                option.as_str().map(ToOwned::to_owned).or_else(|| {
                    option
                        .get("label")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
            })
            .map(Value::String)
            .collect::<Vec<_>>();
        *options = flattened;
    }
    rewritten
}

fn ask_user_question_default_feedback(input: &Value) -> Option<String> {
    let questions = input.get("questions")?.as_array()?;
    let rendered = questions
        .iter()
        .filter_map(|question| {
            let question_text = question.get("question").and_then(Value::as_str)?;
            let first_option = question
                .get("options")
                .and_then(Value::as_array)
                .and_then(|options| options.first())
                .and_then(|option| option.get("label"))
                .and_then(Value::as_str)
                .unwrap_or("默认选择");
            Some(format!("- {question_text}: {first_option}"))
        })
        .collect::<Vec<_>>();
    if rendered.is_empty() {
        None
    } else {
        Some(format!("用户表单回答：\n{}", rendered.join("\n")))
    }
}

async fn apply_question_decision(
    process: &mut PtyProcess,
    question: &TtyQuestion,
    control_response: &Value,
) -> Result<bool> {
    let behavior = question_response_behavior(control_response);
    let denied = matches!(behavior.as_str(), "deny" | "decline" | "cancel" | "error");
    if denied {
        logging::event(format!(
            "question_response question={} behavior={behavior}",
            single_line_log_text(&question.question)
        ));
        cancel_tty_question(
            process,
            question_deny_message(control_response)
                .as_deref()
                .or_else(|| Some(default_question_decline_feedback())),
        )
        .await?;
        return Ok(true);
    }

    let feedback = question_feedback_from_response(control_response, question)
        .unwrap_or_else(|| default_question_answer_feedback(question));
    logging::event(format!(
        "question_response question={} behavior=allow feedback={}",
        single_line_log_text(&question.question),
        single_line_log_text(&feedback)
    ));
    cancel_tty_question(process, Some(&feedback)).await?;
    Ok(false)
}

fn question_response_behavior(control_response: &Value) -> String {
    if control_response
        .get("response")
        .and_then(|response| response.get("subtype"))
        .and_then(Value::as_str)
        == Some("error")
    {
        return "error".to_owned();
    }
    control_response
        .get("response")
        .and_then(|response| response.get("response"))
        .and_then(|response| {
            response
                .get("behavior")
                .or_else(|| response.get("decision"))
                .or_else(|| response.get("action"))
        })
        .and_then(Value::as_str)
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| "allow".to_owned())
}

fn question_deny_message(control_response: &Value) -> Option<String> {
    permission_deny_message(control_response)
}

fn default_question_decline_feedback() -> &'static str {
    "请不要使用表单，改用普通文字继续。"
}

async fn cancel_tty_question(process: &mut PtyProcess, feedback: Option<&str>) -> Result<()> {
    process.write_all(b"\x1b")?;
    if let Some(feedback) = feedback {
        match wait_for_tty_question_feedback_target(process).await {
            Some(QuestionFeedbackTarget::FeedbackPrompt) => {
                process.write_all(&bracketed_paste_input(feedback))?;
            }
            Some(QuestionFeedbackTarget::MainPrompt) => {
                process.write_all(&bracketed_paste_input(feedback))?;
                tokio::time::sleep(Duration::from_millis(120)).await;
                if tty_output_still_editing_prompt(&process.recent_output(), feedback) {
                    process.write_all(b"\r")?;
                    tokio::time::sleep(Duration::from_millis(120)).await;
                }
            }
            None => {}
        }
    }
    tokio::time::sleep(TTY_READY_SETTLE).await;
    Ok(())
}

fn question_feedback_from_response(
    control_response: &Value,
    question: &TtyQuestion,
) -> Option<String> {
    let body = control_response
        .get("response")
        .and_then(|response| response.get("response"))?;
    for candidate in question_answer_candidates(body) {
        if let Some(feedback) = feedback_from_answers(candidate.get("answers")) {
            return Some(feedback);
        }
        if let Some(feedback) = feedback_from_answers(candidate.get("content")) {
            return Some(feedback);
        }
        if let Some(answer) = candidate
            .get("answer")
            .or_else(|| candidate.get("value"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(format!("用户回答：{answer}"));
        }
    }
    question_answer_from_response(control_response, &question.question)
        .map(|answer| format!("用户回答：{answer}"))
}

fn question_feedback_from_mcp_response(
    mcp_response: &Value,
    question: &TtyQuestion,
) -> Option<String> {
    let body = mcp_response.get("result").unwrap_or(mcp_response);
    for candidate in question_answer_candidates(body) {
        if let Some(feedback) = feedback_from_answers(candidate.get("answers")) {
            return Some(feedback);
        }
        if let Some(answer) = mcp_text_content(candidate.get("content")) {
            return Some(answer);
        }
        if let Some(feedback) = feedback_from_answers(candidate.get("content")) {
            return Some(feedback);
        }
        if let Some(answer) = candidate
            .get("answer")
            .or_else(|| candidate.get("value"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(format!("用户回答：{answer}"));
        }
    }
    question_answer_from_mcp_response(mcp_response, &question.question)
        .map(|answer| format!("用户回答：{answer}"))
}

fn mcp_text_content(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(text) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        }
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(|item| {
                    if item.get("type").and_then(Value::as_str) == Some("text") {
                        item.get("text").and_then(Value::as_str)
                    } else {
                        item.as_str()
                    }
                })
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn feedback_from_answers(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return Some(format!("用户回答：{text}"));
    }
    let object = value.as_object()?;
    let pairs = object
        .iter()
        .filter_map(|(key, value)| {
            answer_text_from_value(value).map(|answer| format!("- {key}: {answer}"))
        })
        .collect::<Vec<_>>();
    if pairs.is_empty() {
        return None;
    }
    Some(format!("用户表单回答：\n{}", pairs.join("\n")))
}

fn answer_text_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        }
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(answer_text_from_value)
                .collect::<Vec<_>>()
                .join(", ");
            (!text.is_empty()).then_some(text)
        }
        Value::Object(object) => (!object.is_empty())
            .then(|| serde_json::to_string(value).ok())
            .flatten(),
        Value::Null => None,
        Value::Bool(_) | Value::Number(_) => Some(value.to_string()),
    }
}

fn default_question_answer_feedback(question: &TtyQuestion) -> String {
    let choices = question
        .options
        .iter()
        .filter(|option| !option.special)
        .map(|option| option.label.as_str())
        .collect::<Vec<_>>()
        .join(" / ");
    if choices.is_empty() {
        format!("用户需要回答这个问题：{}", question.question)
    } else {
        format!(
            "用户需要回答这个问题：{}。可选项：{}",
            question.question, choices
        )
    }
}

fn question_answer_from_response(control_response: &Value, question: &str) -> Option<String> {
    let body = control_response
        .get("response")
        .and_then(|response| response.get("response"))?;
    for candidate in question_answer_candidates(body) {
        if let Some(answer) = answer_from_question_map(candidate.get("answers"), question) {
            return Some(answer);
        }
        if let Some(answer) = answer_from_question_map(candidate.get("content"), question) {
            return Some(answer);
        }
        if let Some(answer) = candidate
            .get("answer")
            .or_else(|| candidate.get("value"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(answer.to_owned());
        }
    }
    None
}

fn question_answer_from_mcp_response(mcp_response: &Value, question: &str) -> Option<String> {
    let body = mcp_response.get("result").unwrap_or(mcp_response);
    for candidate in question_answer_candidates(body) {
        if let Some(answer) = answer_from_question_map(candidate.get("answers"), question) {
            return Some(answer);
        }
        if let Some(answer) = answer_from_question_map(candidate.get("content"), question) {
            return Some(answer);
        }
    }
    None
}

fn question_answer_candidates(body: &Value) -> Vec<&Value> {
    [
        body.get("updatedInput"),
        body.get("updated_input"),
        body.get("input"),
        Some(body),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn answer_from_question_map(value: Option<&Value>, question: &str) -> Option<String> {
    let value = value?;
    if let Some(answer) = value.get(question).and_then(answer_text_from_value) {
        return Some(answer);
    }
    let object = value.as_object()?;
    if object.len() == 1 {
        return object.values().next().and_then(answer_text_from_value);
    }
    None
}

fn deny_selection_for_tty_permission_prompt(output: &str, tool_use: &ToolUse) -> &'static [u8] {
    let output = plain_tty_output(output);
    if matches!(tool_use.name.as_str(), "Write" | "Edit" | "MultiEdit") && output.contains("3. No")
    {
        b"3\r"
    } else {
        b"2\r"
    }
}

fn exit_plan_mode_allow_selection(control_response: &Value) -> &'static [u8] {
    let Some(mode) = permission_response_target_mode(control_response) else {
        return b"2\r";
    };
    match mode.replace(['-', '_'], "").to_ascii_lowercase().as_str() {
        "acceptedits" | "auto" | "bypasspermissions" | "dontask" => b"1\r",
        _ => b"2\r",
    }
}

fn permission_response_target_mode(control_response: &Value) -> Option<String> {
    let body = control_response
        .get("response")
        .and_then(|response| response.get("response"))?;
    for value in [
        body.get("updated_input"),
        body.get("updatedInput"),
        body.get("input"),
        Some(body),
    ]
    .into_iter()
    .flatten()
    {
        for key in [
            "_targetMode",
            "targetMode",
            "mode",
            "permissionMode",
            "permission_mode",
        ] {
            if let Some(mode) = value.get(key).and_then(Value::as_str) {
                return Some(mode.to_owned());
            }
        }
    }
    for key in ["updated_permissions", "updatedPermissions"] {
        if let Some(permissions) = body.get(key).and_then(Value::as_array) {
            for permission in permissions {
                let is_set_mode = permission
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.eq_ignore_ascii_case("setMode"));
                if is_set_mode && let Some(mode) = permission.get("mode").and_then(Value::as_str) {
                    return Some(mode.to_owned());
                }
            }
        }
    }
    None
}

async fn wait_for_tty_permission_prompt(process: &PtyProcess, tool_use: &ToolUse) -> bool {
    let started = Instant::now();
    while started.elapsed() < PERMISSION_PROMPT_TIMEOUT {
        let output = process.recent_output();
        if tty_output_has_permission_prompt(&output, tool_use) {
            return true;
        }
        if classify_tty_screen(&output).is_prompt_ready()
            && output_mentions_tool_result(&output, tool_use)
        {
            return false;
        }
        tokio::time::sleep(TRANSCRIPT_POLL).await;
    }
    logging::event(format!(
        "permission_prompt_miss tool={} recent={}",
        single_line_log_text(&tool_use.name),
        single_line_log_text(&recent_tty_log_text(&process.recent_output(), 800))
    ));
    false
}

async fn wait_for_tty_plan_approval_prompt(process: &PtyProcess) -> bool {
    let started = Instant::now();
    while started.elapsed() < PERMISSION_PROMPT_TIMEOUT {
        let output = process.recent_output();
        if tty_output_has_plan_approval_prompt(&output) {
            return true;
        }
        if classify_tty_screen(&output).is_prompt_ready() {
            return false;
        }
        tokio::time::sleep(TRANSCRIPT_POLL).await;
    }
    logging::event(format!(
        "plan_approval_prompt_miss recent={}",
        single_line_log_text(&recent_tty_log_text(&process.recent_output(), 800))
    ));
    false
}

async fn wait_for_tty_permission_feedback_prompt(process: &PtyProcess) -> bool {
    let started = Instant::now();
    while started.elapsed() < PERMISSION_PROMPT_TIMEOUT {
        let output = process.recent_output();
        if tty_output_has_permission_feedback_prompt(&output) {
            return true;
        }
        tokio::time::sleep(TRANSCRIPT_POLL).await;
    }
    false
}

enum QuestionFeedbackTarget {
    FeedbackPrompt,
    MainPrompt,
}

async fn wait_for_tty_question_feedback_target(
    process: &PtyProcess,
) -> Option<QuestionFeedbackTarget> {
    let started = Instant::now();
    while started.elapsed() < PERMISSION_PROMPT_TIMEOUT {
        let output = process.recent_output();
        if tty_output_has_question_feedback_prompt(&output) {
            return Some(QuestionFeedbackTarget::FeedbackPrompt);
        }
        let plain = plain_tty_output(&output);
        let question_was_dismissed = plain.contains("User declined to answer questions")
            || plain.contains("declined to answer questions")
            || plain.contains("declined to answer");
        if (question_was_dismissed || tty_question_from_form(&plain).is_none())
            && classify_tty_screen(&output).is_prompt_ready()
        {
            return Some(QuestionFeedbackTarget::MainPrompt);
        }
        tokio::time::sleep(TRANSCRIPT_POLL).await;
    }
    None
}

async fn wait_for_tty_question_form(process: &PtyProcess) -> bool {
    let started = Instant::now();
    while started.elapsed() < PERMISSION_PROMPT_TIMEOUT {
        if tty_question_from_form(&process.recent_output()).is_some() {
            return true;
        }
        tokio::time::sleep(TRANSCRIPT_POLL).await;
    }
    false
}

fn tty_output_has_permission_prompt(output: &str, tool_use: &ToolUse) -> bool {
    let output = plain_tty_output(output);
    match tool_use.name.as_str() {
        "Write" | "Edit" | "MultiEdit" => {
            plain_tty_output_has_file_permission_prompt(&output)
                && tool_use
                    .input
                    .get("file_path")
                    .and_then(Value::as_str)
                    .is_none_or(|path| output.contains(path))
        }
        tool_name => plain_tty_output_has_permission_prompt_for_tool(&output, tool_name),
    }
}

fn plain_tty_output_has_permission_prompt(output: &str) -> bool {
    plain_tty_output_has_permission_prompt_for_tool(output, "Bash")
}

fn plain_tty_output_has_file_permission_prompt(output: &str) -> bool {
    let compact = compact_tty_output(output);
    let asks_for_file_action = [
        "Do you want to create ",
        "Do you want to edit ",
        "Do you want to update ",
        "Do you want to write ",
    ]
    .iter()
    .any(|marker| output.contains(marker));
    let has_allow_choice =
        output.contains("1. Yes") || output.contains("Yes, allow") || compact.contains("1.Yes");
    let has_deny_choice = output.contains("No") || compact.contains("No");
    let has_controls = output.contains("Esc to cancel")
        || output.contains("Tab to amend")
        || output.contains("Do you want")
        || compact.contains("Esctocancel")
        || compact.contains("Tabtoamend")
        || compact.contains("Doyouwant");
    asks_for_file_action && has_allow_choice && has_deny_choice && has_controls
}

fn plain_tty_output_has_permission_prompt_for_tool(output: &str, tool_name: &str) -> bool {
    let compact = compact_tty_output(output);
    let has_tool = output.contains(tool_name) || compact.contains(tool_name);
    let has_allow_choice =
        output.contains("Yes") || output.contains("Allow") || compact.contains("Yes");
    let has_deny_choice =
        output.contains("No") || output.contains("Deny") || compact.contains("No");
    let has_controls = output.contains("Enter to confirm")
        || output.contains("Esc to cancel")
        || output.contains("Do you want")
        || output.contains("Permission required")
        || compact.contains("Entertoconfirm")
        || compact.contains("Esctocancel")
        || compact.contains("Doyouwant")
        || compact.contains("Permissionrequired");
    let has_generic_permission_prompt =
        tool_name != "Bash" && plain_tty_output_has_generic_permission_prompt(output);
    (has_tool || has_generic_permission_prompt)
        && has_allow_choice
        && has_deny_choice
        && has_controls
}

fn plain_tty_output_has_generic_permission_prompt(output: &str) -> bool {
    let compact = compact_tty_output(output);
    let has_allow_choice =
        output.contains("Yes") || output.contains("Allow") || compact.contains("Yes");
    let has_deny_choice =
        output.contains("No") || output.contains("Deny") || compact.contains("No");
    let has_controls = output.contains("Enter to confirm")
        || output.contains("Esc to cancel")
        || output.contains("Do you want")
        || output.contains("Permission required")
        || compact.contains("Entertoconfirm")
        || compact.contains("Esctocancel")
        || compact.contains("Doyouwant")
        || compact.contains("Permissionrequired");
    let has_permission_language = output.contains("permission")
        || output.contains("Permission")
        || output.contains("Do you want")
        || compact.contains("permission")
        || compact.contains("Permission");
    has_allow_choice && has_deny_choice && has_controls && has_permission_language
}

fn tty_output_has_plan_approval_prompt(output: &str) -> bool {
    let output = plain_tty_output(output);
    let compact = compact_tty_output(&output);
    let has_plan_language = output.contains("Claude has written up a plan")
        || output.contains("Claude wrote up a plan")
        || output.contains("ready to execute")
        || compact.contains("Claudehaswrittenupaplan")
        || compact.contains("Claudewroteupaplan")
        || compact.contains("readytoexecute");
    let has_question = output.contains("Would you like to proceed")
        || output.contains("Would you like Claude to proceed")
        || compact.contains("Wouldyouliketoproceed")
        || compact.contains("WouldyoulikeClaudetoproceed");
    let has_allow_choice = output.contains("Yes, and use auto mode")
        || output.contains("Yes, manually approve edits")
        || compact.contains("Yesanduseautomode")
        || compact.contains("Yesmanuallyapproveedits");
    let has_deny_choice = output.contains("No, refine")
        || output.contains("Tell Claude what to change")
        || compact.contains("Norefine")
        || compact.contains("TellClaudewhattochange");
    has_plan_language && has_question && has_allow_choice && has_deny_choice
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TtyScreenState {
    Empty,
    WorkspaceTrustPrompt,
    StartupChoicePrompt,
    AutoModeConsentPrompt,
    BadResumeStartupError,
    PlanApproval,
    QuestionForm,
    PermissionPrompt,
    Busy,
    PromptReady,
    Other,
}

impl TtyScreenState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::WorkspaceTrustPrompt => "workspace_trust_prompt",
            Self::StartupChoicePrompt => "startup_choice_prompt",
            Self::AutoModeConsentPrompt => "auto_mode_consent_prompt",
            Self::BadResumeStartupError => "bad_resume_startup_error",
            Self::PlanApproval => "plan_approval",
            Self::QuestionForm => "question_form",
            Self::PermissionPrompt => "permission_prompt",
            Self::Busy => "busy",
            Self::PromptReady => "prompt_ready",
            Self::Other => "other",
        }
    }

    fn is_prompt_ready(self) -> bool {
        self == Self::PromptReady
    }
}

struct TtyScreen {
    text: String,
    compact: String,
    lines: Vec<String>,
}

impl TtyScreen {
    fn render(output: &str) -> Self {
        let lines = visible_tty_lines(output);
        let text = if lines.is_empty() {
            plain_tty_output(output)
        } else {
            lines.join(" ")
        };
        let compact = compact_tty_output(&text);
        Self {
            text,
            compact,
            lines,
        }
    }

    fn is_empty(&self) -> bool {
        self.compact.is_empty()
    }

    fn has_busy_indicator(&self) -> bool {
        (self.text.contains("Esc") && self.text.contains("interrupt"))
            || self.compact.contains("Esctointerrupt")
    }

    fn has_choice_menu_controls(&self) -> bool {
        self.text.contains("Enter to confirm")
            || self.text.contains("Enter to select")
            || self.text.contains("Esc to cancel")
            || self.text.contains("Tab/Arrow keys to navigate")
            || self.compact.contains("Entertoconfirm")
            || self.compact.contains("Entertoselect")
            || self.compact.contains("Esctocancel")
            || self.compact.contains("Tab/Arrowkeystonavigate")
    }

    fn has_prompt_input_marker(&self) -> bool {
        if self.has_choice_menu_controls() {
            return false;
        }
        if self.lines.is_empty() {
            return line_has_prompt_input_marker(&self.text);
        }
        self.lines
            .iter()
            .rev()
            .any(|line| line_has_prompt_input_marker(line))
    }
}

fn line_has_prompt_input_marker(line: &str) -> bool {
    line.contains('❯')
}

fn classify_tty_screen(output: &str) -> TtyScreenState {
    let screen = TtyScreen::render(output);
    if screen.is_empty() {
        return TtyScreenState::Empty;
    }
    if tty_output_has_workspace_trust_prompt(&screen.text) {
        return TtyScreenState::WorkspaceTrustPrompt;
    }
    if tty_output_has_startup_choice_prompt(&screen.text) {
        return TtyScreenState::StartupChoicePrompt;
    }
    if tty_output_has_auto_mode_consent_prompt(&screen.text) {
        return TtyScreenState::AutoModeConsentPrompt;
    }
    if tty_output_has_bad_resume_startup_error(&screen.text) {
        return TtyScreenState::BadResumeStartupError;
    }
    if tty_output_has_plan_approval_prompt(&screen.text) {
        return TtyScreenState::PlanApproval;
    }
    if tty_question_from_form(&screen.text).is_some() {
        return TtyScreenState::QuestionForm;
    }
    if plain_tty_output_has_file_permission_prompt(&screen.text)
        || plain_tty_output_has_permission_prompt_for_tool(&screen.text, "Bash")
        || plain_tty_output_has_generic_permission_prompt(&screen.text)
    {
        return TtyScreenState::PermissionPrompt;
    }
    if screen.has_busy_indicator() {
        return TtyScreenState::Busy;
    }
    if screen.has_prompt_input_marker() {
        return TtyScreenState::PromptReady;
    }
    TtyScreenState::Other
}

fn tty_wait_class(output: &str) -> &'static str {
    classify_tty_screen(output).as_str()
}

fn tty_progress_snapshot(process: &PtyProcess) -> String {
    compact_tty_output(&plain_tty_output(&process.recent_output()))
}

fn tty_output_has_visible_model_activity(output: &str) -> bool {
    let output = plain_tty_output(output);
    let compact = compact_tty_output(&output);
    output.contains("⏺")
        || output.contains("⎿")
        || output.contains("Bash (")
        || output.contains("Fetch (")
        || output.contains("Edit (")
        || output.contains("Write (")
        || output.contains("Read (")
        || output.contains("Wrote ")
        || output.contains("thinking with")
        || output.contains("thought for")
        || output.contains("running stop hook")
        || compact.contains("thinkingwith")
        || compact.contains("thoughtfor")
        || compact.contains("runningstophook")
}

fn tty_plan_approval_prompt_has_feedback_choice(output: &str) -> bool {
    let output = plain_tty_output(output);
    let compact = compact_tty_output(&output);
    output.contains("Tell Claude what to change") || compact.contains("TellClaudewhattochange")
}

fn tty_output_has_permission_feedback_prompt(output: &str) -> bool {
    let output = plain_tty_output(output);
    let compact = compact_tty_output(&output);
    output.contains("tell Claude what to do differently")
        || output.contains("Tell Claude what to do differently")
        || output.contains("Tell Claude what to change")
        || output.contains("What should Claude do")
        || output.contains("reason")
        || compact.contains("tellClaudewhattododifferently")
        || compact.contains("TellClaudewhattododifferently")
        || compact.contains("TellClaudewhattochange")
}

fn tty_output_has_question_feedback_prompt(output: &str) -> bool {
    let output = plain_tty_output(output);
    let compact = compact_tty_output(&output);
    output.contains("What should Claude do instead")
        || output.contains("What should Claude do")
        || output.contains("How should Claude proceed")
        || output.contains("Tell Claude what to do differently")
        || compact.contains("WhatshouldClaudedoinstead")
        || compact.contains("WhatshouldClaudedo")
        || compact.contains("HowshouldClaudeproceed")
        || compact.contains("TellClaudewhattododifferently")
}

fn output_mentions_tool_result(output: &str, tool_use: &ToolUse) -> bool {
    let output = plain_tty_output(output);
    output.contains(&tool_use.name)
        && (output.contains("Done")
            || output.contains("Running")
            || output.contains("Waiting")
            || output.contains("⎿"))
}

fn zero_usage() -> Value {
    json!({
        "input_tokens": 0,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "output_tokens": 0,
        "server_tool_use": {
            "web_search_requests": 0,
            "web_fetch_requests": 0,
        },
        "cache_creation": {
            "ephemeral_1h_input_tokens": 0,
            "ephemeral_5m_input_tokens": 0,
        },
        "service_tier": "standard",
    })
}

fn synthetic_result(state: &TranscriptState, duration: Duration) -> Value {
    json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": duration.as_millis() as i64,
        "duration_api_ms": 0,
        "is_error": false,
        "num_turns": 1,
        "session_id": state.session_id.clone().unwrap_or_default(),
        "result": state.assistant_text,
        "stop_reason": "end_turn",
        "usage": zero_usage(),
        "total_cost_usd": 0.0,
        "modelUsage": {},
        "permission_denials": [],
        "terminal_reason": "completed",
        "fast_mode_state": "off",
    })
}

fn synthetic_prompt_ready_result(state: &TranscriptState, duration: Duration) -> Value {
    json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": duration.as_millis() as i64,
        "duration_api_ms": 0,
        "is_error": false,
        "num_turns": 1,
        "session_id": state.session_id.clone().unwrap_or_default(),
        "result": "Claude returned to the terminal prompt without writing a transcript result.",
        "stop_reason": "end_turn",
        "usage": zero_usage(),
        "total_cost_usd": 0.0,
        "modelUsage": {},
        "permission_denials": [],
        "terminal_reason": "prompt_ready_without_transcript",
        "fast_mode_state": "off",
    })
}

fn synthetic_permission_denied_result(state: &TranscriptState, duration: Duration) -> Value {
    json!({
        "type": "result",
        "subtype": "error",
        "duration_ms": duration.as_millis() as i64,
        "duration_api_ms": 0,
        "is_error": true,
        "num_turns": 1,
        "session_id": state.session_id.clone().unwrap_or_default(),
        "result": if state.assistant_text.is_empty() {
            "Permission denied"
        } else {
            state.assistant_text.as_str()
        },
        "stop_reason": "end_turn",
        "usage": zero_usage(),
        "total_cost_usd": 0.0,
        "modelUsage": {},
        "permission_denials": [],
        "terminal_reason": "completed",
        "fast_mode_state": "off",
    })
}

fn write_final_output(state: &TranscriptState, output_format: OutputFormat) -> Result<()> {
    match output_format {
        OutputFormat::StreamJson => {}
        OutputFormat::Text => {
            print!("{}", state.assistant_text);
            std::io::stdout().flush()?;
        }
        OutputFormat::Json => {
            let mut result = state.result.clone().unwrap_or_else(|| {
                json!({
                    "type": "result",
                    "subtype": "success",
                    "is_error": false,
                    "result": state.assistant_text,
                    "session_id": state.session_id.clone().unwrap_or_default(),
                })
            });
            if result.get("result").is_none()
                && let Some(object) = result.as_object_mut()
            {
                object.insert(
                    "result".to_owned(),
                    Value::String(state.assistant_text.clone()),
                );
            }
            println!("{}", serde_json::to_string(&result)?);
        }
    }
    Ok(())
}

fn prompt_from_invocation(invocation: &Invocation) -> Result<String> {
    if let Some(prompt) = &invocation.prompt {
        return Ok(prompt.clone());
    }
    if std::io::stdin().is_terminal() {
        return Ok(String::new());
    }
    let mut prompt = String::new();
    std::io::stdin().read_to_string(&mut prompt)?;
    Ok(prompt)
}

fn user_prompt_from_sdk_message(value: &Value) -> Result<String> {
    let content = value
        .get("message")
        .and_then(|message| message.get("content"))
        .ok_or_else(|| CcttyError::Usage("user message missing message.content".to_owned()))?;
    match content {
        Value::String(text) => Ok(text.clone()),
        Value::Array(blocks) => Ok(blocks
            .iter()
            .map(|block| {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                } else {
                    block.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")),
        other => Ok(other.to_string()),
    }
}

fn bracketed_paste_input(prompt: &str) -> Vec<u8> {
    let normalized = prompt.replace("\r\n", "\n").replace('\r', "\n");
    let mut bytes = Vec::with_capacity(normalized.len() + 16);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(normalized.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~\r");
    bytes
}

async fn prepare_tty_for_prompt(process: &mut PtyProcess) -> Result<()> {
    let started = Instant::now();
    let mut trust_prompt_ack_sent = false;
    let mut startup_choice_ack_sent = false;
    let mut auto_mode_ack_sent = false;
    loop {
        let output = process.recent_output();
        let screen_state = classify_tty_screen(&output);
        if screen_state == TtyScreenState::WorkspaceTrustPrompt && !trust_prompt_ack_sent {
            process.write_all(b"\r")?;
            trust_prompt_ack_sent = true;
            tokio::time::sleep(TRUST_PROMPT_SETTLE).await;
            continue;
        }
        if screen_state == TtyScreenState::StartupChoicePrompt && !startup_choice_ack_sent {
            process.write_all(b"\r")?;
            startup_choice_ack_sent = true;
            tokio::time::sleep(TRUST_PROMPT_SETTLE).await;
            continue;
        }
        if screen_state == TtyScreenState::AutoModeConsentPrompt && !auto_mode_ack_sent {
            process.write_all(b"2\r")?;
            auto_mode_ack_sent = true;
            tokio::time::sleep(TRUST_PROMPT_SETTLE).await;
            continue;
        }
        if screen_state == TtyScreenState::BadResumeStartupError {
            let recent = recent_tty_log_text(&output, 600);
            logging::event(format!("tty_startup_bad_resume recent={recent}"));
            return Err(CcttyError::Tty(recent));
        }
        if screen_state.is_prompt_ready() {
            tokio::time::sleep(TTY_READY_SETTLE).await;
            logging::event(format!(
                "tty_ready stage=prepare elapsed_ms={}",
                started.elapsed().as_millis()
            ));
            return Ok(());
        }
        if started.elapsed() > TTY_STARTUP_TIMEOUT {
            let recent = plain_tty_output(&process.recent_output());
            let recent = recent
                .chars()
                .rev()
                .take(600)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            logging::event(format!("tty_startup_timeout recent={recent}"));
            return Err(CcttyError::Timeout(format!(
                "timed out waiting for Claude prompt; recent tty output: {recent}"
            )));
        }
        tokio::time::sleep(TRANSCRIPT_POLL).await;
    }
}

async fn prepare_tty_for_prompt_with_mcp(
    process: &mut PtyProcess,
    input: &mut mpsc::Receiver<Value>,
    runtime: &SdkMcpRuntime,
    sdk_state: &mut SdkStreamState,
) -> Result<()> {
    let started = Instant::now();
    let mut trust_prompt_ack_sent = false;
    let mut startup_choice_ack_sent = false;
    let mut auto_mode_ack_sent = false;
    loop {
        service_pending_mcp_proxy_requests_for_runtime(input, runtime, sdk_state).await?;
        let output = process.recent_output();
        let screen_state = classify_tty_screen(&output);
        if screen_state == TtyScreenState::WorkspaceTrustPrompt && !trust_prompt_ack_sent {
            process.write_all(b"\r")?;
            trust_prompt_ack_sent = true;
            tokio::time::sleep(TRUST_PROMPT_SETTLE).await;
            continue;
        }
        if screen_state == TtyScreenState::StartupChoicePrompt && !startup_choice_ack_sent {
            process.write_all(b"\r")?;
            startup_choice_ack_sent = true;
            tokio::time::sleep(TRUST_PROMPT_SETTLE).await;
            continue;
        }
        if screen_state == TtyScreenState::AutoModeConsentPrompt && !auto_mode_ack_sent {
            process.write_all(b"2\r")?;
            auto_mode_ack_sent = true;
            tokio::time::sleep(TRUST_PROMPT_SETTLE).await;
            continue;
        }
        if screen_state == TtyScreenState::BadResumeStartupError {
            let recent = recent_tty_log_text(&output, 600);
            logging::event(format!("tty_startup_bad_resume recent={recent}"));
            return Err(CcttyError::Tty(recent));
        }
        if screen_state.is_prompt_ready() {
            tokio::time::sleep(TTY_READY_SETTLE).await;
            logging::event(format!(
                "tty_ready stage=prepare_mcp elapsed_ms={}",
                started.elapsed().as_millis()
            ));
            return Ok(());
        }
        if started.elapsed() > TTY_STARTUP_TIMEOUT {
            let recent = plain_tty_output(&process.recent_output());
            let recent = recent
                .chars()
                .rev()
                .take(600)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            logging::event(format!("tty_startup_timeout recent={recent}"));
            return Err(CcttyError::Timeout(format!(
                "timed out waiting for Claude prompt; recent tty output: {recent}"
            )));
        }
        tokio::time::sleep(TRANSCRIPT_POLL).await;
    }
}

fn tty_output_has_workspace_trust_prompt(output: &str) -> bool {
    let output = plain_tty_output(output);
    let compact = compact_tty_output(&output);
    // "Yes, I trust this folder" is the stable affirmative across Claude Code
    // versions. Older builds also rendered a "Quick safety check" title, but
    // 2.1.x dropped it (the dialog now reads "...take a moment to review what's
    // in this folder..." with a "Security guide" link). Requiring the old title
    // left the 2.1.x trust dialog unrecognized, so startup stalled on it instead
    // of auto-acknowledging. Match on the affirmative alone, keeping the legacy
    // title as an additional accepted marker.
    output.contains("Yes, I trust this folder")
        || compact.contains("Yes,Itrustthisfolder")
        || output.contains("Quick safety check")
        || compact.contains("Quicksafetycheck")
}

fn tty_output_has_startup_choice_prompt(output: &str) -> bool {
    let output = plain_tty_output(output);
    let compact = compact_tty_output(&output);
    output.contains("Syntax theme:") || compact.contains("Syntaxtheme:")
}

fn tty_output_has_auto_mode_consent_prompt(output: &str) -> bool {
    let output = plain_tty_output(output);
    let compact = compact_tty_output(&output);
    (output.contains("auto mode") || compact.contains("automode"))
        && (output.contains("Yes, enable auto mode") || compact.contains("Yes,enableautomode"))
        && (output.contains("No, exit") || compact.contains("No,exit"))
}

fn tty_output_has_bad_resume_startup_error(output: &str) -> bool {
    tty_output_has_session_lock_startup_error(output)
        || tty_output_has_missing_resume_startup_error(output)
}

fn tty_output_has_session_lock_startup_error(output: &str) -> bool {
    let output = plain_tty_output(output);
    let lower = output.to_ascii_lowercase();
    lower.contains("session id") && lower.contains("already in use")
}

fn tty_output_has_missing_resume_startup_error(output: &str) -> bool {
    let output = plain_tty_output(output);
    let lower = output.to_ascii_lowercase();
    lower.contains("no conversation found with session id")
}

fn tty_output_accepts_prompt(output: &str) -> bool {
    classify_tty_screen(output).is_prompt_ready()
}

fn tty_output_still_editing_prompt(output: &str, prompt: &str) -> bool {
    if !tty_output_accepts_prompt(output) {
        return false;
    }
    let plain = plain_tty_output(output);
    if tty_question_from_form(&plain).is_some()
        || plain_tty_output_has_permission_prompt_for_tool(&plain, "Bash")
        || plain_tty_output_has_file_permission_prompt(&plain)
        || plain_tty_output_has_generic_permission_prompt(&plain)
    {
        return false;
    }
    if tty_output_has_collapsed_paste_input(&plain) {
        return true;
    }
    let prompt = compact_tty_output(prompt);
    if prompt.is_empty() {
        return false;
    }
    let tail = prompt
        .char_indices()
        .rev()
        .nth(31)
        .map(|(index, _)| &prompt[index..])
        .unwrap_or(prompt.as_str());
    compact_tty_output(&plain).contains(tail)
}

fn tty_output_has_collapsed_paste_input(output: &str) -> bool {
    let recent = recent_tty_log_text(output, 1_200);
    let compact = compact_tty_output(&recent);
    (recent.contains("[Pasted text #") || compact.contains("[Pastedtext#"))
        && (recent.contains("paste again to expand") || compact.contains("pasteagaintoexpand"))
}

struct TailCursor {
    path: Option<PathBuf>,
    config_dir: PathBuf,
    project_dir: PathBuf,
    offset: u64,
    attach_existing: bool,
    started_at: SystemTime,
    session_alias: SessionIdAlias,
}

impl TailCursor {
    fn new(
        path: Option<PathBuf>,
        config_dir: &Path,
        attach_existing: bool,
        session_alias: SessionIdAlias,
    ) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        let project_dir = config_dir
            .join("projects")
            .join(crate::transcript::project_key(&cwd));
        Ok(Self {
            path,
            config_dir: config_dir.to_path_buf(),
            project_dir,
            offset: 0,
            attach_existing,
            started_at: SystemTime::now(),
            session_alias,
        })
    }

    fn prepare_offset(&mut self) -> Result<()> {
        self.started_at = SystemTime::now();
        if self.path.is_none() && self.attach_existing {
            self.path = newest_transcript(&self.project_dir)?;
        }
        self.offset = self
            .path
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        Ok(())
    }

    fn resolve_path(&mut self) -> Result<Option<PathBuf>> {
        if self.path.is_none() {
            self.path = if self.attach_existing {
                newest_transcript(&self.project_dir)?
            } else {
                newest_transcript_since(&self.project_dir, self.started_at)?
            };
        }
        let _ = &self.config_dir;
        Ok(self.path.clone())
    }

    fn reset_for_new_session(&mut self) {
        self.path = None;
        self.offset = 0;
        self.attach_existing = false;
        self.started_at = SystemTime::now();
    }

    fn externalize_value(&self, value: &mut Value) {
        self.session_alias.externalize_value(value);
    }

    fn path_log_label(&self) -> String {
        self.path
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|file_name| file_name.to_str())
            .unwrap_or("none")
            .to_owned()
    }

    fn remove_current_transcript(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        remove_dir_if_empty(&self.project_dir)?;
        remove_dir_if_empty(&self.config_dir.join("projects"))?;
        Ok(())
    }
}

fn remove_dir_if_empty(path: &Path) -> Result<()> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.kind() == std::io::ErrorKind::DirectoryNotEmpty =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn newest_transcript(project_dir: &Path) -> Result<Option<PathBuf>> {
    newest_transcript_matching(project_dir, |_| true)
}

fn newest_transcript_since(project_dir: &Path, started_at: SystemTime) -> Result<Option<PathBuf>> {
    newest_transcript_matching(project_dir, |modified| {
        modified.is_some_and(|modified| modified >= started_at)
    })
}

fn newest_transcript_matching(
    project_dir: &Path,
    include: impl Fn(Option<SystemTime>) -> bool,
) -> Result<Option<PathBuf>> {
    let Ok(entries) = std::fs::read_dir(project_dir) else {
        return Ok(None);
    };
    let mut newest = None;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = entry.metadata()?.modified().ok();
        if !include(modified) {
            continue;
        }
        if newest
            .as_ref()
            .and_then(|(_, modified)| *modified)
            .is_none_or(|current| modified.is_some_and(|candidate| candidate > current))
        {
            newest = Some((path, modified));
        }
    }
    Ok(newest.map(|(path, _)| path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_paste_wraps_prompt() {
        assert_eq!(
            String::from_utf8(bracketed_paste_input("a\r\nb")).unwrap(),
            "\u{1b}[200~a\nb\u{1b}[201~\r"
        );
    }

    #[test]
    fn recognizes_workspace_trust_prompt_across_versions() {
        // Claude Code 2.1.x dialog — the old "Quick safety check" title is gone.
        let v2 = "Do you trust the files in this folder? /tmp/x \
            take a moment to review what's in this folder first. \
            Claude Code'll be able to read, edit, and execute files here. Security guide \
            ❯ 1. Yes, I trust this folder  2. No, suggest changes (esc)";
        assert!(tty_output_has_workspace_trust_prompt(v2));
        assert_eq!(
            classify_tty_screen(v2),
            TtyScreenState::WorkspaceTrustPrompt
        );
        // Legacy dialog with the old title still matches.
        let legacy = "Quick safety check ❯ 1. Yes, I trust this folder  2. No";
        assert!(tty_output_has_workspace_trust_prompt(legacy));
        // A normal prompt-ready screen must not be misread as the trust dialog.
        let ready = "❯ Try \"fix lint errors\"  ? for shortcuts";
        assert!(!tty_output_has_workspace_trust_prompt(ready));
    }

    #[test]
    fn detects_prompt_left_in_tty_input() {
        let output = "❯ Write a compact document for SDK users\nContext 0% /mcp";
        assert!(tty_output_still_editing_prompt(
            output,
            "Write a compact document for SDK users"
        ));
    }

    #[test]
    fn detects_collapsed_paste_left_in_tty_input() {
        let output = "\
            [Opus 4.8] │ workspace git:( test ) Context 0% \
            ⏸ plan mode on (shift+tab to cycle) · ← for agents \
            ────── ↯ 1 MCP server failed · /mcp \
            ❯ B [Pasted text #1 +15 lines] paste again to expand";
        assert!(tty_output_still_editing_prompt(
            output,
            "system instructions\n\nUser prompt"
        ));
    }

    #[test]
    fn ignores_stale_collapsed_paste_from_previous_turn() {
        let output = format!(
            "[Pasted text #1 +15 lines] paste again to expand {} ❯",
            "assistant result ".repeat(200)
        );
        assert!(!tty_output_still_editing_prompt(
            &output,
            "next user prompt"
        ));
    }

    #[test]
    fn does_not_retry_submit_on_permission_prompt() {
        let output =
            "Bash command echo test\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel";
        assert!(!tty_output_still_editing_prompt(
            output,
            "Write a compact document for SDK users"
        ));
    }

    #[test]
    fn does_not_retry_submit_on_generic_permission_prompt() {
        let output = "\
            Permission required to load a deferred tool\r\n\
            Do you want to proceed?\r\n\
            ❯ 1. Yes\r\n\
              2. No\r\n\
            Enter to confirm · Esc to cancel";
        assert!(!tty_output_still_editing_prompt(
            output,
            "Write a compact document for SDK users"
        ));
    }

    #[test]
    fn does_not_accept_status_without_prompt_marker_as_ready() {
        let output = "Context permissions /mcp";

        assert_eq!(tty_wait_class(output), "other");
        assert!(!tty_output_accepts_prompt(output));
    }

    #[test]
    fn does_not_accept_choice_menu_prompt_marker_as_ready() {
        let output = "\
            Do you want to proceed?\r\n\
            ❯ 1. Yes\r\n\
              2. No\r\n\
            Enter to confirm · Esc to cancel";

        assert_eq!(tty_wait_class(output), "permission_prompt");
        assert!(!tty_output_accepts_prompt(output));
    }

    #[test]
    fn strips_session_resume_args_for_startup_retry() {
        let args = vec![
            "--model".to_owned(),
            "sonnet".to_owned(),
            "--resume-session-at".to_owned(),
            "message-1".to_owned(),
            "--session-id=locked-session".to_owned(),
            "--continue".to_owned(),
            "--permission-mode".to_owned(),
            "default".to_owned(),
        ];
        assert_eq!(
            strip_session_resume_args(&args),
            vec![
                "--model".to_owned(),
                "sonnet".to_owned(),
                "--permission-mode".to_owned(),
                "default".to_owned(),
            ]
        );
    }

    #[test]
    fn strips_ask_user_question_from_disallowed_tools_for_bridge() {
        let args = vec![
            "--disallowedTools".to_owned(),
            "AskUserQuestion,Write".to_owned(),
            "--disallowed-tools=Bash,askuserquestion".to_owned(),
            "--model".to_owned(),
            "haiku".to_owned(),
        ];
        assert_eq!(
            strip_disallowed_tool(&args, "AskUserQuestion"),
            vec![
                "--disallowedTools".to_owned(),
                "Write".to_owned(),
                "--disallowed-tools=Bash".to_owned(),
                "--model".to_owned(),
                "haiku".to_owned(),
            ]
        );
    }

    #[test]
    fn extracts_question_feedback_from_mcp_text_content() {
        let question = TtyQuestion {
            question: "Which style?".to_owned(),
            header: "Style".to_owned(),
            options: vec![TtyQuestionOption {
                label: "APA".to_owned(),
                description: "American Psychological Association".to_owned(),
                special: false,
            }],
        };
        let response = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "result": {
                "content": [
                    { "type": "text", "text": "User responses:\n1. 技术设计" }
                ]
            }
        });

        assert_eq!(
            question_feedback_from_mcp_response(&response, &question).as_deref(),
            Some("User responses:\n1. 技术设计")
        );
    }

    #[test]
    fn extracts_ask_user_question_feedback_from_mcp_text_content() {
        let input = json!({
            "questions": [
                {
                    "question": "Which style?",
                    "options": [{ "label": "APA", "description": "American Psychological Association" }]
                }
            ]
        });
        let response = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "result": {
                "content": [
                    { "type": "text", "text": "User responses:\n1. 技术设计" }
                ]
            }
        });

        assert_eq!(
            ask_user_question_feedback_from_mcp_response(&response, &input).as_deref(),
            Some("User responses:\n1. 技术设计")
        );
    }

    #[test]
    fn flattens_ask_user_question_options_for_sdk_mcp() {
        let input = json!({
            "questions": [
                {
                    "question": "Which style?",
                    "options": [
                        { "label": "APA", "description": "American Psychological Association" },
                        "MLA"
                    ]
                }
            ]
        });

        let rewritten = sdk_ask_user_question_input(&input);

        assert_eq!(rewritten["questions"][0]["options"], json!(["APA", "MLA"]));
    }

    #[test]
    fn detects_auto_mode_consent_prompt() {
        let output = "\
            Claude can run tools automatically. \
            Sessions are slightly more expensive. \
            Shift+Tab to change mode. \
            ❯ 1. Yes, and make it my default mode \
              2. Yes, enable auto mode \
              3. No, exit Enter to confirm";

        assert!(tty_output_has_auto_mode_consent_prompt(output));
    }

    #[test]
    fn parses_tty_permission_command_from_bash_invocation() {
        let output = "\
            Bash(printf cctty-live-allow) \
            ⎿ Waiting... \
            Bash command printf cctty-live-allow Print test string \
            Permission rule Bash(printf:*) requires confirmation for this command. \
            Do you want to proceed? ❯ 1. Yes 2. No Esc to cancel";

        assert_eq!(
            bash_command_from_tty_permission_prompt(output).as_deref(),
            Some("printf cctty-live-allow")
        );
    }

    #[test]
    fn parses_tty_permission_command_from_structured_form_before_damaged_status_line() {
        let output = "\
            \u{1b}[6ABash(printf cctty-live-p\u{1b}[28Grmission-allow)\r\n\
            Bash command\r\n\
            \u{1b}[4Gprintf\u{1b}[11Gcctty-live-permission-allow\r\n\
            \u{1b}[4GPrint\u{1b}[10Gtest\u{1b}[15Gstring\r\n\
            Permission rule Bash(printf:*) requires confirmation for this command.\r\n\
            Do you want to proceed? ❯ 1. Yes 2. No Esc to cancel";

        assert_eq!(
            bash_command_from_tty_permission_prompt(output).as_deref(),
            Some("printf cctty-live-permission-allow")
        );
    }

    #[test]
    fn ignores_bash_permission_title_without_command() {
        let output = "\
            Review bash command execution request \
            Do you want to allow Bash? \
            ❯ 1. Yes 2. No Esc to cancel";

        assert_eq!(bash_command_from_tty_permission_prompt(output), None);
        assert!(tool_use_from_tty_permission_prompt(output, "tool-1".to_owned()).is_none());
    }

    #[test]
    fn strips_tty_bash_description_column() {
        let output = "\
            Bash command\r\n\
            \u{1b}[1Gprintf CCTTY_PERMISSION_FILE_OK > file.txt\u{1b}[64GRun shell command\r\n\
            Permission rule Bash(printf:*) requires confirmation for this command.\r\n\
            Do you want to proceed? ❯ 1. Yes 2. No Esc to cancel";

        assert_eq!(
            bash_command_from_tty_permission_prompt(output).as_deref(),
            Some("printf CCTTY_PERMISSION_FILE_OK > file.txt")
        );
    }

    #[test]
    fn strips_flat_tty_bash_display_suffix() {
        let output = "\
            Bash command printf CCTTY_PERMISSION_FILE_OK > file.txt Run shell command \
            Permission rule Bash(printf:*) requires confirmation for this command. \
            Do you want to proceed? ❯ 1. Yes 2. No Esc to cancel";

        assert_eq!(
            bash_command_from_tty_permission_prompt(output).as_deref(),
            Some("printf CCTTY_PERMISSION_FILE_OK > file.txt")
        );
    }

    #[test]
    fn parses_tty_permission_command_across_cursor_position_inside_token() {
        let output = "\
            Bash command\r\n\
            \u{1b}[1Gprintf\u{1b}[8Gcctty-live-permis\u{1b}[25Gsion-allow\r\n\
            \u{1b}[1GPrint\u{1b}[7Gtest\u{1b}[12Gstring\r\n\
            Permission rule Bash(printf:*) requires confirmation for this command.\r\n\
            Do you want to proceed? ❯ 1. Yes 2. No Esc to cancel";

        assert_eq!(
            bash_command_from_tty_permission_prompt(output).as_deref(),
            Some("printf cctty-live-permission-allow")
        );
    }

    #[test]
    fn parses_tty_tool_use_from_raw_form_not_flattened_output() {
        let output = "\
            Bash(printf cc\u{1b}[18Gty-live-permissi\u{1b}[35Gn-allow)\r\n\
            Bash command\r\n\
            \u{1b}[4Gprintf\u{1b}[11Gcctty-live-permission-allow\r\n\
            \u{1b}[4GPrint\u{1b}[10Gtest\u{1b}[15Gstring\r\n\
            Permission rule Bash(printf:*) requires confirmation for this command.\r\n\
            Do you want to proceed?\r\n\
            ❯ 1. Yes\r\n\
            2. No\r\n\
            Esc to cancel · Tab to amend · ctrl+e to explain";
        let tool_use = tool_use_from_tty_permission_prompt(output, "tool-1".to_owned()).unwrap();

        assert_eq!(tool_use.name, "Bash");
        assert_eq!(
            tool_use.input["command"],
            Value::String("printf cctty-live-permission-allow".to_owned())
        );
    }

    #[test]
    fn parses_tty_write_permission_form_before_transcript_tool_use() {
        let output = "\
            Do you want to create index.html ?\r\n\
            ❯ 1. Yes\r\n\
              2. Yes, allow all edits during this session (shift+tab)\r\n\
              3. No\r\n\
            Esc to cancel · Tab to amend";
        let tool_use = tool_use_from_tty_permission_prompt(output, "tool-write-1".to_owned())
            .expect("file permission prompt should parse");

        assert_eq!(tool_use.name, "Write");
        assert_eq!(
            tool_use.input["file_path"],
            Value::String("index.html".to_owned())
        );
        assert!(tty_output_has_permission_prompt(output, &tool_use));
        assert_eq!(
            deny_selection_for_tty_permission_prompt(output, &tool_use),
            b"3\r"
        );
    }

    #[test]
    fn detects_tty_plan_approval_prompt() {
        let output = "\
            Plan Mode Test\r\n\
            Context\r\n\
            This is a test of the plan mode workflow.\r\n\
            Plan\r\n\
            1. Reply with CCTTY_PLAN_OK after approval.\r\n\
            Claude has written up a plan and is ready to execute. Would you like to proceed?\r\n\
            ❯ 1. Yes, and use auto mode\r\n\
              2. Yes, manually approve edits\r\n\
              3. No, refine with more details\r\n\
              4. Tell Claude what to change\r\n\
            Enter to confirm · Esc to cancel";

        assert!(tty_output_has_plan_approval_prompt(output));
        assert!(tty_plan_approval_prompt_has_feedback_choice(output));
        let tool_use =
            tool_use_from_tty_plan_approval_prompt(output, "tool-plan-approval".to_owned(), None)
                .expect("plan prompt should synthesize ExitPlanMode");
        assert_eq!(tool_use.name, "ExitPlanMode");
        assert!(
            tool_use.input["plan"]
                .as_str()
                .unwrap()
                .contains("CCTTY_PLAN_OK")
        );
        let clean_plan = json!({
            "plan": "# Clean Plan\n\n1. Continue.",
            "planFilePath": "/Users/test/.claude/plans/plan-mode-test.md"
        });
        let tool_use = tool_use_from_tty_plan_approval_prompt(
            output,
            "tool-plan-clean".to_owned(),
            Some(&clean_plan),
        )
        .expect("plan prompt should use cached clean plan input");
        assert_eq!(tool_use.input, clean_plan);
    }

    #[test]
    fn skips_internal_claude_plan_file_write() {
        let plan_write = ToolUse {
            id: "tool-plan-write".to_owned(),
            name: "Write".to_owned(),
            input: json!({
                "file_path": "/Users/test/.claude/plans/plan-mode-test.md",
                "content": "# Plan"
            }),
        };
        let project_write = ToolUse {
            id: "tool-project-write".to_owned(),
            name: "Write".to_owned(),
            input: json!({
                "file_path": "/workspace/.claude/plans-not-in-home.md",
                "content": "# Not internal"
            }),
        };

        assert!(is_internal_claude_plan_write(&plan_write));
        assert!(!is_internal_claude_plan_write(&project_write));
        assert_eq!(
            internal_plan_input_from_write(&plan_write)
                .unwrap()
                .get("plan")
                .and_then(Value::as_str),
            Some("# Plan")
        );
    }

    #[test]
    fn skips_internal_exit_plan_tool_search() {
        let tool_search = ToolUse {
            id: "tool-search-exit-plan".to_owned(),
            name: "ToolSearch".to_owned(),
            input: json!({ "query": "select:ExitPlanMode" }),
        };
        let other_tool_search = ToolUse {
            id: "tool-search-other".to_owned(),
            name: "ToolSearch".to_owned(),
            input: json!({ "query": "select:SomethingElse" }),
        };

        assert!(is_internal_exit_plan_tool_search(&tool_search));
        assert!(!is_internal_exit_plan_tool_search(&other_tool_search));
    }

    #[test]
    fn chooses_manual_plan_approval_by_default() {
        let response = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "cctty_permission_1",
                "response": { "behavior": "allow" }
            }
        });

        assert_eq!(exit_plan_mode_allow_selection(&response), b"2\r");
    }

    #[test]
    fn chooses_auto_plan_approval_for_accept_edits_target() {
        let response = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "cctty_permission_1",
                "response": {
                    "behavior": "allow",
                    "updated_input": { "_targetMode": "acceptEdits" }
                }
            }
        });

        assert_eq!(exit_plan_mode_allow_selection(&response), b"1\r");
    }

    #[test]
    fn parses_tty_ask_user_question_form() {
        let output = "\
            ← ☐ 文档类型 ☐ 目标读者 ☐ 语言 ☐ 输出位置 ✔ Submit → \
            你想写什么类型的文档？ \
            ❯ 1. 技术设计文档 描述某个功能/系统的架构、方案、实现细节 \
            2. README / 使用说明 项目介绍、安装、用法说明 \
            3. 产品需求文档 (PRD) 描述产品功能、用户场景、需求范围 \
            4. 操作/教程指南 一步步指导如何完成某项任务 \
            5. Type something. \
            6. Chat about this Enter to select · Tab/Arrow keys to navigate · Esc to cancel";
        let question = tty_question_from_form(output).expect("question form should parse");

        assert_eq!(question.question, "你想写什么类型的文档？");
        assert_eq!(
            question
                .options
                .iter()
                .filter(|option| !option.special)
                .count(),
            4
        );
        assert_eq!(
            question.options[0].label,
            "技术设计文档 描述某个功能/系统的架构、方案、实现细节"
        );
        assert_eq!(question.options[4].label, "Type something");
        assert!(question.options[4].special);
    }

    #[test]
    fn parses_tty_ask_user_question_form_after_noisy_screen() {
        let output = "\
            Claude Code v2.1.156 Tips for getting started Remote Control failed \
            [Sonnet 4.6] │ workspace 0 tokens Context ░░░░░░░░░░ 0% \
            Use the AskUserQuestion tool to ask me which document style to use. \
            ☐ 文档风格 请选择文档风格： \
            ❯ 1. 技术设计 技术设计文档风格 \
            2. 操作手册 操作手册文档风格 \
            3. Type something. \
            4. Chat about this Enter to select · Tab/Arrow keys to navigate · Esc to cancel";
        let question = tty_question_from_form(output).expect("question form should parse");

        assert_eq!(question.header, "文档风格");
        assert_eq!(question.question, "请选择文档风格：");
        assert_eq!(question.options[0].label, "技术设计");
        assert_eq!(question.options[0].description, "技术设计文档风格");
        assert_eq!(question.options[1].label, "操作手册");
        assert_eq!(question.options[1].description, "操作手册文档风格");
    }

    #[test]
    fn accepts_remote_control_active_screen_as_prompt_ready() {
        let output = "\
            ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents ◉ xhigh · /effort \
            Remote Control connecting… ⚠ 1 setup issue: MCP · /doctor ↯ /fast \
            ❯ Try \"edit main.go to...\" \
            [Opus 4.8] │ workspace git:( test/remote-control ) Context ░░░░░░░░░░ 0% \
            ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents \
            Remote Control active ────── ↯ Remote Control active";

        assert!(tty_output_accepts_prompt(output));
    }

    #[test]
    fn accepts_plain_claude_status_screen_as_prompt_ready() {
        let output = "\
            ───────────────────────────────────────────────────────────────────────────────────────────────────────── \
            ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents ● high · /effort \
            ▎ Opus 4.8 is here! Now defaults to high effort · /effort xhigh for your hardest tasks \
            ❯ T ry \"create a util logging.py that...\" \
            ──────────────────────────────────────────────────────────────────────────────────────────────────────── \
            ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents ● high · /effort \
            [Opus 4.8 (1M context)] ░░░░░░░░░░ 0% | git:( master ) \
            ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents";

        assert!(tty_output_accepts_prompt(output));
        assert_eq!(tty_wait_class(output), "prompt_ready");
    }

    #[test]
    fn classifies_rendered_screen_after_ansi_clear_as_prompt_ready() {
        let output = "\
            Do you want to proceed?\r\n\
            ❯ 1. Yes\r\n\
              2. No\r\n\
            Enter to confirm · Esc to cancel\
            \u{1b}[2J\u{1b}[H\
            [Opus 4.8] │ workspace Context ░░░░░░░░░░ 0%\r\n\
            ❯ ";

        assert_eq!(tty_wait_class(output), "prompt_ready");
        assert!(tty_output_accepts_prompt(output));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sdk_mcp_proxy_connection_without_line_does_not_block_prepare_loop() {
        use std::os::unix::net::UnixStream;

        let runtime = create_sdk_mcp_runtime(vec!["conductor".to_owned()]).unwrap();
        let _held_open = UnixStream::connect(&runtime.socket_path).unwrap();
        let (_tx, mut rx) = mpsc::channel(1);
        let mut sdk_state = SdkStreamState::new(&[]);
        let started = Instant::now();

        service_pending_mcp_proxy_requests_for_runtime(&mut rx, &runtime, &mut sdk_state)
            .await
            .unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn keeps_english_question_leader_in_tty_question_prompt() {
        let output = "\
            ← ☐ Doc type ✔ Submit → \
            What kind of document do you want? \
            ❯ 1. Technical design Architecture and implementation details \
            2. Product brief Audience, goals, and scope \
            3. Type something. \
            4. Chat about this Enter to select · Tab/Arrow keys to navigate · Esc to cancel";
        let question = tty_question_from_form(output).expect("question form should parse");

        assert_eq!(question.header, "Doc type");
        assert_eq!(question.question, "What kind of document do you want?");
    }

    #[test]
    fn parses_tty_permission_command_from_bash_command_fallback() {
        let output = "\
            Bash command echo tty fake \
            Permission rule Bash(echo:*) requires confirmation for this command. \
            Do you want to proceed? ❯ 1. Yes 2. No Esc to cancel";

        assert_eq!(
            bash_command_from_tty_permission_prompt(output).as_deref(),
            Some("echo tty fake")
        );
    }
}
