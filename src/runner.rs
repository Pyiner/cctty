use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead, BufReader as StdBufReader, IsTerminal, Read, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
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

const COMPLETION_IDLE: Duration = Duration::from_millis(1_500);
const TRANSCRIPT_POLL: Duration = Duration::from_millis(80);
const TRUST_PROMPT_SETTLE: Duration = Duration::from_millis(800);
const TTY_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const TTY_READY_SETTLE: Duration = Duration::from_millis(250);
const PERMISSION_PROMPT_TIMEOUT: Duration = Duration::from_secs(8);
const TTY_PERMISSION_TRANSCRIPT_GRACE: Duration = Duration::from_millis(1_500);
const TTY_QUESTION_FORM_SETTLE: Duration = Duration::from_millis(250);
const MCP_PROXY_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const PTY_TERMINATE_TIMEOUT: Duration = Duration::from_secs(5);
const RUN_TIMEOUT: Duration = Duration::from_secs(3600);

pub async fn run(invocation: Invocation) -> Result<i32> {
    match invocation.mode {
        CommandMode::Passthrough => run_passthrough(&invocation).await,
        CommandMode::Print => run_print(invocation).await,
    }
}

pub(crate) fn run_mcp_proxy(argv: Vec<String>) -> Result<i32> {
    let socket_path = argv
        .get(2)
        .ok_or_else(|| CcttyError::Usage("__cctty-mcp-proxy missing socket path".to_owned()))?;
    let server_name = argv
        .get(3)
        .ok_or_else(|| CcttyError::Usage("__cctty-mcp-proxy missing server name".to_owned()))?;
    run_mcp_proxy_stdio(Path::new(socket_path), server_name)
}

#[cfg(unix)]
fn run_mcp_proxy_stdio(socket_path: &Path, server_name: &str) -> Result<i32> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let reader = StdBufReader::new(stdin.lock());
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let message = serde_json::from_str::<Value>(trimmed)?;
        let mut stream = UnixStream::connect(socket_path)?;
        writeln!(
            stream,
            "{}",
            serde_json::to_string(&json!({
                "server_name": server_name,
                "message": message,
            }))?
        )?;
        stream.flush()?;

        let mut response_line = String::new();
        StdBufReader::new(stream).read_line(&mut response_line)?;
        let response = serde_json::from_str::<Value>(response_line.trim())?;
        let mcp_response = response.get("mcp_response").cloned().unwrap_or(Value::Null);
        writeln!(stdout, "{}", serde_json::to_string(&mcp_response)?)?;
        stdout.flush()?;
    }
    Ok(0)
}

#[cfg(not(unix))]
fn run_mcp_proxy_stdio(_socket_path: &Path, _server_name: &str) -> Result<i32> {
    Err(CcttyError::Usage(
        "SDK MCP proxy is only supported on Unix platforms".to_owned(),
    ))
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
    let transcript = if invocation.continue_conversation {
        None
    } else {
        session_id
            .as_ref()
            .map(|session_id| transcript_path(&config_dir, &cwd, session_id))
    };
    let env = interactive_claude_env();
    let mut passthrough_args = invocation.passthrough_args.clone();
    let mut current_session_id = session_id.clone();
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
        if let Err(error) = prepare_tty_for_prompt(&mut process).await {
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
                    prepare_tty_for_prompt(&mut process).await?;
                } else {
                    return Err(error);
                }
            } else {
                return Err(error);
            }
        }
    }

    let mut tail = TailCursor::new(transcript, &config_dir, attach_existing)?;

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
        unset_env: interactive_claude_unset_env(),
    })
}

fn interactive_claude_env() -> HashMap<String, String> {
    HashMap::from([
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("COLORTERM".to_owned(), "truecolor".to_owned()),
    ])
}

fn interactive_claude_unset_env() -> Vec<String> {
    [
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_AGENT_SDK_VERSION",
        "NO_COLOR",
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect()
}

fn is_bad_resume_startup_error(error: &CcttyError) -> bool {
    matches!(error, CcttyError::Tty(message) if tty_output_has_bad_resume_startup_error(message))
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
}

struct SdkStreamState {
    mcp_servers: Vec<McpServerStatus>,
    mcp_runtime: Option<SdkMcpRuntime>,
    deferred_input: VecDeque<Value>,
    next_mcp_request_id: u64,
}

impl SdkStreamState {
    fn new(args: &[String]) -> Self {
        let mut state = Self {
            mcp_servers: mcp_servers_from_args(args),
            mcp_runtime: None,
            deferred_input: VecDeque::new(),
            next_mcp_request_id: 0,
        };
        state.add_sdk_mcp_servers(sdk_mcp_servers_from_args(args));
        state
    }

    fn add_sdk_mcp_servers(&mut self, names: Vec<String>) {
        for name in names {
            if self.mcp_servers.iter().any(|server| server.name == name) {
                continue;
            }
            self.mcp_servers.push(McpServerStatus {
                name,
                kind: "sdk".to_owned(),
            });
        }
    }

    fn sdk_mcp_server_names(&self) -> Vec<String> {
        self.mcp_servers
            .iter()
            .filter(|server| server.kind == "sdk")
            .map(|server| server.name.clone())
            .collect()
    }

    fn next_mcp_request_id(&mut self) -> String {
        self.next_mcp_request_id += 1;
        format!("cctty-mcp-{}", self.next_mcp_request_id)
    }

    fn mcp_status(&self) -> Value {
        let sdk_runtime_started = self.mcp_runtime.is_some();
        let servers = self
            .mcp_servers
            .iter()
            .map(|server| {
                let is_sdk = server.kind == "sdk";
                json!({
                    "name": server.name,
                    "status": if is_sdk && !sdk_runtime_started { "pending" } else { "connected" },
                    "serverInfo": {
                        "name": server.name,
                        "version": if is_sdk { "cctty-sdk-proxy" } else { "unknown" },
                    },
                    "config": if is_sdk {
                        json!({ "type": "sdk", "name": server.name })
                    } else {
                        json!({ "type": server.kind, "source": "mcp-config" })
                    },
                    "scope": "session",
                    "tools": [],
                })
            })
            .collect::<Vec<_>>();
        json!({ "mcpServers": servers })
    }
}

struct McpServerStatus {
    name: String,
    kind: String,
}

#[cfg(unix)]
struct SdkMcpRuntime {
    socket_path: PathBuf,
    listener: UnixListener,
    servers: Vec<String>,
}

#[cfg(unix)]
impl Drop for SdkMcpRuntime {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(not(unix))]
struct SdkMcpRuntime {
    servers: Vec<String>,
}

fn mcp_config_values_from_args(args: &[String]) -> Vec<String> {
    let mut configs = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--mcp-config" {
            if let Some(value) = args.get(index + 1) {
                configs.push(value.clone());
            }
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--mcp-config=") {
            configs.push(value.to_owned());
        }
        index += 1;
    }
    configs
}

fn mcp_servers_from_args(args: &[String]) -> Vec<McpServerStatus> {
    let mut servers = Vec::new();
    for config in mcp_config_values_from_args(args) {
        if let Ok(value) = serde_json::from_str::<Value>(&config) {
            if let Some(mcp_servers) = value.get("mcpServers").and_then(Value::as_object) {
                for (name, server) in mcp_servers {
                    let kind = server
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned();
                    if servers
                        .iter()
                        .any(|existing: &McpServerStatus| existing.name == name.as_str())
                    {
                        continue;
                    }
                    servers.push(McpServerStatus {
                        name: name.clone(),
                        kind,
                    });
                }
            }
        }
    }
    servers
}

fn sdk_mcp_servers_from_args(args: &[String]) -> Vec<String> {
    let mut servers = Vec::new();
    for config in mcp_config_values_from_args(args) {
        if let Ok(value) = serde_json::from_str::<Value>(&config) {
            if let Some(mcp_servers) = value.get("mcpServers").and_then(Value::as_object) {
                for (name, server) in mcp_servers {
                    if server.get("type").and_then(Value::as_str) == Some("sdk") {
                        let server_name = server
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(name)
                            .to_owned();
                        if !servers.contains(&server_name) {
                            servers.push(server_name);
                        }
                    }
                }
            }
        }
    }
    servers
}

fn sdk_mcp_servers_from_initialize(value: &Value) -> Vec<String> {
    value
        .get("request")
        .and_then(|request| request.get("sdkMcpServers"))
        .and_then(Value::as_array)
        .map(|servers| {
            servers
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

async fn ensure_sdk_mcp_runtime(
    process: &mut PtyProcess,
    input: &mut mpsc::Receiver<Value>,
    sdk_state: &mut SdkStreamState,
    spawn: &StreamSpawnContext<'_>,
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
    prepare_tty_for_prompt_with_mcp(process, input, &runtime, sdk_state).await?;
    sdk_state.mcp_runtime = Some(runtime);
    Ok(true)
}

#[cfg(unix)]
fn create_sdk_mcp_runtime(servers: Vec<String>) -> Result<SdkMcpRuntime> {
    let socket_path = std::env::temp_dir().join(format!("ct-{}.sock", Uuid::new_v4()));
    let listener = UnixListener::bind(&socket_path)?;
    listener.set_nonblocking(true)?;
    Ok(SdkMcpRuntime {
        socket_path,
        listener,
        servers,
    })
}

#[cfg(not(unix))]
fn create_sdk_mcp_runtime(servers: Vec<String>) -> Result<SdkMcpRuntime> {
    let _ = servers;
    Err(CcttyError::Usage(
        "SDK MCP proxy is only supported on Unix platforms".to_owned(),
    ))
}

#[cfg(unix)]
fn sdk_mcp_runtime_socket_path(runtime: &SdkMcpRuntime) -> String {
    runtime.socket_path.to_string_lossy().into_owned()
}

#[cfg(not(unix))]
fn sdk_mcp_runtime_socket_path(_runtime: &SdkMcpRuntime) -> String {
    "-".to_owned()
}

fn sdk_mcp_runtime_servers(runtime: &SdkMcpRuntime) -> &[String] {
    &runtime.servers
}

fn args_with_mcp_runtime(args: &[String], runtime: &SdkMcpRuntime) -> Result<Vec<String>> {
    let mut rewritten = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--mcp-config" {
            if let Some(value) = args.get(index + 1) {
                if let Some(stripped) = mcp_config_without_sdk_servers(value)? {
                    rewritten.push(arg.clone());
                    rewritten.push(stripped);
                }
            }
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--mcp-config=") {
            if let Some(stripped) = mcp_config_without_sdk_servers(value)? {
                rewritten.push(format!("--mcp-config={stripped}"));
            }
            index += 1;
            continue;
        }
        rewritten.push(arg.clone());
        index += 1;
    }
    rewritten.push("--mcp-config".to_owned());
    rewritten.push(sdk_mcp_proxy_config(runtime)?);
    Ok(rewritten)
}

fn mcp_config_without_sdk_servers(raw: &str) -> Result<Option<String>> {
    let Ok(mut value) = serde_json::from_str::<Value>(raw) else {
        return Ok(Some(raw.to_owned()));
    };
    let Some(object) = value.as_object_mut() else {
        return Ok(Some(raw.to_owned()));
    };
    if let Some(mcp_servers) = object.get_mut("mcpServers").and_then(Value::as_object_mut) {
        mcp_servers.retain(|_, server| server.get("type").and_then(Value::as_str) != Some("sdk"));
        if mcp_servers.is_empty() {
            object.remove("mcpServers");
        }
    }
    if object.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string(&value)?))
}

#[cfg(unix)]
fn sdk_mcp_proxy_config(runtime: &SdkMcpRuntime) -> Result<String> {
    let executable = std::env::current_exe()?;
    let mut mcp_servers = serde_json::Map::new();
    for server in &runtime.servers {
        mcp_servers.insert(
            server.clone(),
            json!({
                "type": "stdio",
                "command": executable,
                "args": [
                    "__cctty-mcp-proxy",
                    runtime.socket_path.to_string_lossy(),
                    server,
                ],
            }),
        );
    }
    Ok(serde_json::to_string(
        &json!({ "mcpServers": mcp_servers }),
    )?)
}

#[cfg(not(unix))]
fn sdk_mcp_proxy_config(_runtime: &SdkMcpRuntime) -> Result<String> {
    Err(CcttyError::Usage(
        "SDK MCP proxy is only supported on Unix platforms".to_owned(),
    ))
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
                if ensure_sdk_mcp_runtime(process, &mut input, &mut sdk_state, spawn).await? {
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
                let prompt = user_prompt_from_sdk_message(&value)?;
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
    spawn: &StreamSpawnContext<'_>,
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
    spawn: &StreamSpawnContext<'_>,
) -> Result<()> {
    match prepare_tty_for_prompt(process).await {
        Ok(()) => Ok(()),
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
            prepare_tty_for_prompt(process).await?;
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
    spawn: &StreamSpawnContext<'_>,
) -> Result<()> {
    match prepare_tty_for_prompt_with_mcp(process, input, runtime, sdk_state).await {
        Ok(()) => Ok(()),
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
            prepare_tty_for_prompt_with_mcp(process, input, runtime, sdk_state).await?;
            tail.reset_for_new_session();
            Ok(())
        }
        Err(error) => Err(error),
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
        let mut line = String::new();
        {
            let mut reader = StdBufReader::new(stream.try_clone()?);
            reader.read_line(&mut line)?;
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
                "message": message,
            },
        });
        println!("{}", serde_json::to_string(&control)?);
        std::io::stdout().flush()?;
        let mcp_response = wait_for_mcp_control_response(input, sdk_state, &request_id).await?;
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

async fn handle_control_request(
    process: &mut PtyProcess,
    _input: &mut mpsc::Receiver<Value>,
    sdk_state: &mut SdkStreamState,
    spawn: &mut StreamSpawnContext<'_>,
    tty_prepared: &mut bool,
    value: &Value,
) -> Result<()> {
    let request_id = value
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or_else(|| CcttyError::Usage("control_request missing request_id".to_owned()))?;
    let subtype = value
        .get("request")
        .and_then(|request| request.get("subtype"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let response = match subtype {
        "initialize" => {
            sdk_state.add_sdk_mcp_servers(sdk_mcp_servers_from_initialize(value));
            control_success(request_id, sdk_initialize_response())
        }
        "interrupt" => {
            process.interrupt()?;
            control_success(request_id, Value::Null)
        }
        "set_model" => {
            if let Some(model) = control_request_string(value, &["model"]) {
                spawn.args = args_with_option_value(&spawn.args, "--model", &model);
                restart_tty_after_control_update(process, sdk_state, spawn, tty_prepared)?;
                logging::event(format!(
                    "control_update subtype=set_model model={}",
                    single_line_log_text(&model)
                ));
            }
            control_success(request_id, Value::Null)
        }
        "set_permission_mode" => {
            if let Some(mode) =
                control_request_string(value, &["mode", "permission_mode", "permissionMode"])
            {
                spawn.args = args_with_option_value(&spawn.args, "--permission-mode", &mode);
                restart_tty_after_control_update(process, sdk_state, spawn, tty_prepared)?;
                logging::event(format!(
                    "control_update subtype=set_permission_mode mode={}",
                    single_line_log_text(&mode)
                ));
            }
            control_success(request_id, Value::Null)
        }
        "set_max_thinking_tokens" => control_success(request_id, Value::Null),
        "apply_flag_settings" => control_success(request_id, Value::Null),
        "get_context_usage" => {
            control_success(request_id, json!({ "total": 0, "used": 0, "remaining": 0 }))
        }
        "mcp_status" => control_success(request_id, sdk_state.mcp_status()),
        _ => control_error(
            request_id,
            format!("Unsupported control request: {subtype}"),
        ),
    };
    logging::event(format!("control_request subtype={subtype}"));
    println!("{}", serde_json::to_string(&response)?);
    std::io::stdout().flush()?;
    Ok(())
}

fn control_request_string(value: &Value, keys: &[&str]) -> Option<String> {
    let request = value.get("request")?;
    keys.iter().find_map(|key| {
        request
            .get(*key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn restart_tty_after_control_update(
    process: &mut PtyProcess,
    sdk_state: &SdkStreamState,
    spawn: &StreamSpawnContext<'_>,
    tty_prepared: &mut bool,
) -> Result<()> {
    let args = args_with_runtime_mcp(&spawn.args, sdk_state.mcp_runtime.as_ref())?;
    logging::event(format!(
        "tty_restart reason=control_update args={}",
        sanitized_arg_shape(&args)
    ));
    process.terminate(PTY_TERMINATE_TIMEOUT);
    *process = spawn_tty_process(
        spawn.claude,
        spawn.cwd,
        spawn.env,
        &args,
        &spawn.session_id,
        spawn.invocation,
    )?;
    *tty_prepared = false;
    Ok(())
}

fn args_with_runtime_mcp(args: &[String], runtime: Option<&SdkMcpRuntime>) -> Result<Vec<String>> {
    match runtime {
        Some(runtime) => args_with_mcp_runtime(args, runtime),
        None => Ok(args.to_vec()),
    }
}

fn args_with_option_value(args: &[String], flag: &str, value: &str) -> Vec<String> {
    let mut updated = Vec::with_capacity(args.len() + 2);
    let mut index = 0;
    let mut replaced = false;
    while index < args.len() {
        let arg = &args[index];
        if arg == flag {
            updated.push(arg.clone());
            updated.push(value.to_owned());
            replaced = true;
            index += 1;
            if index < args.len() && !args[index].starts_with('-') {
                index += 1;
            }
            continue;
        }
        if arg.starts_with(&format!("{flag}=")) {
            updated.push(format!("{flag}={value}"));
            replaced = true;
            index += 1;
            continue;
        }
        updated.push(arg.clone());
        index += 1;
    }
    if !replaced {
        updated.push(flag.to_owned());
        updated.push(value.to_owned());
    }
    updated
}

fn sdk_initialize_response() -> Value {
    json!({
        "commands": [],
        "agents": [],
        "output_style": "default",
        "available_output_styles": ["default"],
        "models": [
            {
                "value": "default",
                "displayName": "Default (recommended)",
                "description": "Claude Code default model through cctty",
                "supportsEffort": true,
                "supportedEffortLevels": ["low", "medium", "high", "max"],
                "supportsAdaptiveThinking": true,
                "supportsAutoMode": true,
            },
            {
                "value": "sonnet[1m]",
                "displayName": "Sonnet (1M context)",
                "description": "Claude Code Sonnet 1M context alias through cctty",
                "supportsEffort": true,
                "supportedEffortLevels": ["low", "medium", "high", "max"],
                "supportsAdaptiveThinking": true,
                "supportsAutoMode": true,
            },
            {
                "value": "opus",
                "displayName": "Opus",
                "description": "Claude Code Opus alias through cctty",
                "supportsEffort": true,
                "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"],
                "supportsAdaptiveThinking": true,
                "supportsFastMode": true,
                "supportsAutoMode": true,
            },
            {
                "value": "haiku",
                "displayName": "Haiku",
                "description": "Claude Code Haiku alias through cctty",
            },
            {
                "value": "claude-opus-4-8",
                "displayName": "Opus 4.8",
                "description": "Claude Code Opus 4.8 model through cctty",
                "supportsEffort": true,
                "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"],
                "supportsAdaptiveThinking": true,
                "supportsFastMode": true,
                "supportsAutoMode": true,
            },
        ],
        "account": {
            "tokenSource": "cctty",
            "apiProvider": "firstParty",
        },
        "fast_mode_state": "off",
        "pid": std::process::id(),
    })
}

fn control_success(request_id: &str, response: Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
        }
    })
}

fn control_error(request_id: &str, error: String) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": error,
        }
    })
}

async fn submit_prompt_and_tail(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    prompt: &str,
    output_format: OutputFormat,
    include_partial_messages: bool,
) -> Result<TranscriptState> {
    tail.prepare_offset()?;
    submit_prompt_to_tty(process, prompt).await?;
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
    submit_prompt_to_tty(process, prompt).await?;
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

async fn submit_prompt_to_tty(process: &mut PtyProcess, prompt: &str) -> Result<()> {
    process.write_all(&bracketed_paste_input(prompt))?;
    tokio::time::sleep(Duration::from_millis(120)).await;
    if tty_output_still_editing_prompt(&process.recent_output(), prompt) {
        logging::event("prompt_submit_retry reason=prompt_still_visible");
        process.write_all(b"\r")?;
    }
    Ok(())
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
    let mut questions = TtyQuestionBridge::new(false);

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
                        let value: Value = serde_json::from_str(&line)?;
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
        if questions.maybe_handle_tty_question(process, None).await? {
            last_activity = Instant::now();
        }
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
                        let value: Value = serde_json::from_str(&line)?;
                        permission
                            .maybe_request_tool_permission(process, input, &value)
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
                .maybe_handle_tty_question(process, Some(input))
                .await?
        {
            permission.mark_ask_user_question_handled();
            last_activity = Instant::now();
        }

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
    if output_format != OutputFormat::StreamJson
        || state.saw_idle_session_state
        || std::env::var("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS")
            .ok()
            .as_deref()
            != Some("1")
    {
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

struct PermissionBridge {
    enabled: bool,
    requested_tool_use_ids: HashSet<String>,
    handled_tool_keys: Vec<ToolKey>,
    pending_tty_permission: Option<PendingTtyPermission>,
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
        let Some(tool_use) = tool_use_from_tty_permission_prompt(
            &process.recent_output(),
            format!("cctty_tty_tool_{}", self.next_request),
        ) else {
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
            requested = true;
            self.mark_tool_handled(&tool_use);
            if tool_use.name == "AskUserQuestion" {
                self.handled_ask_user_question = true;
            }
            if self.pending_tty_permission_matches(&tool_use) {
                self.pending_tty_permission = None;
            }
            self.request_permission(process, input, &tool_use).await?;
        }
        Ok(requested)
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
        self.request_question(process, input, question).await?;
        Ok(true)
    }

    async fn request_question(
        &mut self,
        process: &mut PtyProcess,
        input: &mut mpsc::Receiver<Value>,
        question: TtyQuestion,
    ) -> Result<()> {
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
    let form = plain
        .rsplit_once("✔ Submit →")
        .map(|(_, after)| after)
        .unwrap_or(&plain);
    let option_start = form.find("❯ 1. ").or_else(|| form.find("1. "))?;
    let question = form[..option_start].trim();
    if question.is_empty() {
        return None;
    }
    let options = numbered_question_options(&form[option_start..]);
    if options.iter().filter(|option| !option.special).count() < 2 {
        return None;
    }
    Some(TtyQuestion {
        question: question.to_owned(),
        header: tty_question_header(&plain).unwrap_or_else(|| short_header(question)),
        options,
    })
}

fn tty_question_header(plain: &str) -> Option<String> {
    let before_submit = plain.rsplit_once("✔ Submit →")?.0;
    before_submit
        .rsplit_once('☐')
        .map(|(_, header)| header.trim())
        .filter(|header| !header.is_empty())
        .map(short_header)
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

fn bash_command_from_tty_permission_prompt(output: &str) -> Option<String> {
    if let Some(command) = bash_structured_command_from_tty_output(output) {
        return Some(command);
    }
    if let Some(command) = bash_parenthetical_command_from_tty_output(output) {
        return Some(command);
    }
    let rest = output.split("Bash command ").nth(1)?;
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
    let command = normalize_tool_command(command);
    (!command.is_empty()).then_some(command)
}

fn bash_structured_command_from_tty_output(output: &str) -> Option<String> {
    let lines = visible_tty_lines(output);
    for (index, line) in lines.iter().enumerate().rev() {
        if line != "Bash command" {
            continue;
        }
        let command = lines
            .iter()
            .skip(index + 1)
            .find(|candidate| {
                !candidate.starts_with("Permission rule")
                    && !candidate.starts_with("/permissions")
                    && !candidate.starts_with("Do you want")
            })?
            .clone();
        if !command.is_empty() {
            return Some(command);
        }
    }
    None
}

fn bash_parenthetical_command_from_tty_output(output: &str) -> Option<String> {
    let indices = output.match_indices("Bash(").collect::<Vec<_>>();
    for (index, _) in indices.into_iter().rev() {
        let after = &output[index + "Bash(".len()..];
        let Some((command, _)) = after.split_once(')') else {
            continue;
        };
        let command = normalize_tool_command(command);
        if !command.is_empty() && !command.contains(":*") {
            return Some(command);
        }
    }
    None
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
        process.write_all(b"\r")?;
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
                submit_prompt_to_tty(process, feedback).await?;
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

async fn wait_for_tty_permission_prompt(process: &PtyProcess, tool_use: &ToolUse) -> bool {
    let started = Instant::now();
    while started.elapsed() < PERMISSION_PROMPT_TIMEOUT {
        let output = process.recent_output();
        if tty_output_has_permission_prompt(&output, tool_use) {
            return true;
        }
        if tty_output_accepts_prompt(&output) && output_mentions_tool_result(&output, tool_use) {
            return false;
        }
        tokio::time::sleep(TRANSCRIPT_POLL).await;
    }
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
            && tty_output_accepts_prompt(&plain)
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
    has_tool && has_allow_choice && has_deny_choice && has_controls
}

fn tty_output_has_permission_feedback_prompt(output: &str) -> bool {
    let output = plain_tty_output(output);
    let compact = compact_tty_output(&output);
    output.contains("tell Claude what to do differently")
        || output.contains("Tell Claude what to do differently")
        || output.contains("What should Claude do")
        || output.contains("reason")
        || compact.contains("tellClaudewhattododifferently")
        || compact.contains("TellClaudewhattododifferently")
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
        if tty_output_has_workspace_trust_prompt(&output) && !trust_prompt_ack_sent {
            process.write_all(b"\r")?;
            trust_prompt_ack_sent = true;
            tokio::time::sleep(TRUST_PROMPT_SETTLE).await;
            continue;
        }
        if tty_output_has_startup_choice_prompt(&output) && !startup_choice_ack_sent {
            process.write_all(b"\r")?;
            startup_choice_ack_sent = true;
            tokio::time::sleep(TRUST_PROMPT_SETTLE).await;
            continue;
        }
        if tty_output_has_auto_mode_consent_prompt(&output) && !auto_mode_ack_sent {
            process.write_all(b"2\r")?;
            auto_mode_ack_sent = true;
            tokio::time::sleep(TRUST_PROMPT_SETTLE).await;
            continue;
        }
        if tty_output_has_bad_resume_startup_error(&output) {
            let recent = recent_tty_log_text(&output, 600);
            logging::event(format!("tty_startup_bad_resume recent={recent}"));
            return Err(CcttyError::Tty(recent));
        }
        if tty_output_accepts_prompt(&output) {
            tokio::time::sleep(TTY_READY_SETTLE).await;
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
        if tty_output_has_workspace_trust_prompt(&output) && !trust_prompt_ack_sent {
            process.write_all(b"\r")?;
            trust_prompt_ack_sent = true;
            tokio::time::sleep(TRUST_PROMPT_SETTLE).await;
            continue;
        }
        if tty_output_has_startup_choice_prompt(&output) && !startup_choice_ack_sent {
            process.write_all(b"\r")?;
            startup_choice_ack_sent = true;
            tokio::time::sleep(TRUST_PROMPT_SETTLE).await;
            continue;
        }
        if tty_output_has_auto_mode_consent_prompt(&output) && !auto_mode_ack_sent {
            process.write_all(b"2\r")?;
            auto_mode_ack_sent = true;
            tokio::time::sleep(TRUST_PROMPT_SETTLE).await;
            continue;
        }
        if tty_output_has_bad_resume_startup_error(&output) {
            let recent = recent_tty_log_text(&output, 600);
            logging::event(format!("tty_startup_bad_resume recent={recent}"));
            return Err(CcttyError::Tty(recent));
        }
        if tty_output_accepts_prompt(&output) {
            tokio::time::sleep(TTY_READY_SETTLE).await;
            service_pending_mcp_proxy_requests_for_runtime(input, runtime, sdk_state).await?;
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
    (output.contains("Quick safety check") || compact.contains("Quicksafetycheck"))
        && (output.contains("Yes, I trust this folder") || compact.contains("Yes,Itrustthisfolder"))
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
    let output = plain_tty_output(output);
    let lower = output.to_ascii_lowercase();
    (lower.contains("session id") && lower.contains("already in use"))
        || lower.contains("no conversation found with session id")
}

fn tty_output_accepts_prompt(output: &str) -> bool {
    let output = plain_tty_output(output);
    let compact = compact_tty_output(&output);
    let has_status = output.contains("permissions")
        || output.contains("Remote Control failed")
        || output.contains("MCP server failed")
        || output.contains("/mcp")
        || compact.contains("permissions")
        || compact.contains("RemoteControlfailed")
        || compact.contains("MCPserverfailed")
        || compact.contains("/mcp");
    let has_prompt_marker = output.contains('❯') || compact.contains('❯');
    ((output.contains("Context") || compact.contains("Context")) && has_status)
        || (has_prompt_marker && has_status)
}

fn tty_output_still_editing_prompt(output: &str, prompt: &str) -> bool {
    if !tty_output_accepts_prompt(output) {
        return false;
    }
    let plain = plain_tty_output(output);
    if tty_question_from_form(&plain).is_some()
        || plain_tty_output_has_permission_prompt_for_tool(&plain, "Bash")
        || plain_tty_output_has_file_permission_prompt(&plain)
    {
        return false;
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

fn compact_tty_output(output: &str) -> String {
    output.split_whitespace().collect()
}

fn plain_tty_output(output: &str) -> String {
    let mut plain = String::with_capacity(output.len());
    let mut chars = output.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            strip_ansi_sequence(&mut chars);
            plain.push(' ');
        } else if ch.is_control() {
            plain.push(' ');
        } else {
            plain.push(ch);
        }
    }
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn recent_tty_log_text(output: &str, max_chars: usize) -> String {
    plain_tty_output(output)
        .chars()
        .rev()
        .take(max_chars)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
}

fn single_line_log_text(text: &str) -> String {
    text.chars()
        .flat_map(|ch| match ch {
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            other if other.is_control() => format!("\\u{{{:x}}}", other as u32).chars().collect(),
            other => vec![other],
        })
        .collect()
}

fn visible_tty_lines(output: &str) -> Vec<String> {
    let mut chars = output.chars().peekable();
    let mut line = Vec::<char>::new();
    let mut column = 0_usize;
    let mut lines = Vec::<String>::new();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            apply_ansi_sequence_to_line(&mut chars, &mut line, &mut column);
        } else if ch == '\r' {
            column = 0;
        } else if ch == '\r' || ch == '\n' {
            push_visible_line(&mut lines, &mut line);
            column = 0;
        } else if ch.is_control() {
            write_visible_char(&mut line, &mut column, ' ');
        } else {
            write_visible_char(&mut line, &mut column, ch);
        }
    }
    push_visible_line(&mut lines, &mut line);
    lines
}

fn push_visible_line(lines: &mut Vec<String>, line: &mut Vec<char>) {
    let rendered = normalize_tool_command(&line.iter().collect::<String>());
    if !rendered.is_empty() {
        lines.push(rendered);
    }
    line.clear();
}

fn write_visible_char(line: &mut Vec<char>, column: &mut usize, ch: char) {
    if *column >= line.len() {
        line.resize(*column + 1, ' ');
    }
    line[*column] = ch;
    *column += 1;
}

fn apply_ansi_sequence_to_line<I>(
    chars: &mut std::iter::Peekable<I>,
    line: &mut Vec<char>,
    column: &mut usize,
) where
    I: Iterator<Item = char>,
{
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            let mut sequence = String::new();
            for ch in chars.by_ref() {
                sequence.push(ch);
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
            apply_csi_sequence_to_line(&sequence, line, column);
        }
        Some(']') => {
            chars.next();
            for ch in chars.by_ref() {
                if ch == '\u{7}' {
                    break;
                }
            }
        }
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

fn apply_csi_sequence_to_line(sequence: &str, line: &mut Vec<char>, column: &mut usize) {
    let Some(command) = sequence.chars().last() else {
        return;
    };
    let amount = sequence
        .trim_end_matches(command)
        .split(';')
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    match command {
        'G' | '`' => {
            *column = amount.saturating_sub(1);
        }
        'C' => {
            *column += amount;
        }
        'D' => {
            *column = column.saturating_sub(amount);
        }
        'K' => {
            line.truncate((*column).min(line.len()));
        }
        _ => {}
    }
}

fn strip_ansi_sequence<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        }
        Some(']') => {
            chars.next();
            for ch in chars.by_ref() {
                if ch == '\u{7}' {
                    break;
                }
            }
        }
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

fn resolve_claude_path() -> Result<String> {
    let own_exe = std::env::current_exe().ok();
    let current_dir = std::env::current_dir().ok();
    resolve_claude_path_from(
        own_exe.as_deref(),
        current_dir.as_deref(),
        std::env::var_os("CCTTY_CLAUDE_PATH"),
        std::env::var_os("CONDUCTOR_AGENT_BINARIES_DIR"),
        std::env::var_os("PATH"),
    )
}

fn resolve_claude_path_from(
    own_exe: Option<&Path>,
    current_dir: Option<&Path>,
    explicit_path: Option<OsString>,
    conductor_agent_binaries_dir: Option<OsString>,
    path_env: Option<OsString>,
) -> Result<String> {
    let own_exe = own_exe.and_then(canonical_path);
    if let Some(path) = explicit_path
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return usable_claude_candidate(&path, own_exe.as_deref()).ok_or_else(|| {
            CcttyError::ClaudeNotFound(format!(
                "CCTTY_CLAUDE_PATH points to unusable Claude binary: {}",
                path.display()
            ))
        });
    }

    for candidate in current_dir_claude_candidates(current_dir) {
        if let Some(path) = usable_claude_candidate(&candidate, own_exe.as_deref()) {
            logging::event(format!("claude_resolved source=current_dir path={path}"));
            return Ok(path);
        }
    }

    for candidate in relative_claude_candidates(own_exe.as_deref()) {
        if let Some(path) = usable_claude_candidate(&candidate, own_exe.as_deref()) {
            logging::event(format!("claude_resolved source=relative path={path}"));
            return Ok(path);
        }
    }

    for candidate in conductor_agent_binary_candidates(conductor_agent_binaries_dir) {
        if let Some(path) = usable_claude_candidate(&candidate, own_exe.as_deref()) {
            logging::event(format!("claude_resolved source=conductor path={path}"));
            return Ok(path);
        }
    }

    for candidate in path_claude_candidates(path_env) {
        if let Some(path) = usable_claude_candidate(&candidate, own_exe.as_deref()) {
            logging::event(format!("claude_resolved source=path path={path}"));
            return Ok(path);
        }
    }

    Err(CcttyError::ClaudeNotFound(
        "no usable real Claude binary found via CCTTY_CLAUDE_PATH, current directory, cctty-relative paths, Conductor agent-binaries, or PATH"
            .to_owned(),
    ))
}

fn current_dir_claude_candidates(current_dir: Option<&Path>) -> Vec<PathBuf> {
    let Some(dir) = current_dir else {
        return Vec::new();
    };
    ["claude.real", "claude.orig", "claude-code", "claude"]
        .into_iter()
        .map(|name| dir.join(name))
        .collect()
}

fn relative_claude_candidates(own_exe: Option<&Path>) -> Vec<PathBuf> {
    let Some(dir) = own_exe.and_then(Path::parent) else {
        return Vec::new();
    };
    [
        "claude.real",
        "claude.orig",
        "claude-code",
        "claude",
        "../claude/claude",
        "../claude-code/claude",
        "../vendor/claude-code/claude",
    ]
    .into_iter()
    .map(|name| dir.join(name))
    .collect()
}

fn conductor_agent_binary_candidates(dir: Option<OsString>) -> Vec<PathBuf> {
    let Some(root) = dir.map(PathBuf::from).filter(|path| path.is_dir()) else {
        return Vec::new();
    };
    let claude_root = root.join("claude");
    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&claude_root) {
        for entry in entries.flatten() {
            candidates.push(entry.path().join("claude"));
        }
    }
    candidates.sort_by(|left, right| right.cmp(left));
    candidates
}

fn path_claude_candidates(path_env: Option<OsString>) -> Vec<PathBuf> {
    path_env
        .map(|value| {
            std::env::split_paths(&value)
                .map(|dir| dir.join("claude"))
                .collect()
        })
        .unwrap_or_default()
}

fn usable_claude_candidate(path: &Path, own_exe: Option<&Path>) -> Option<String> {
    let canonical = canonical_path(path)?;
    if own_exe.is_some_and(|own| own == canonical) {
        return None;
    }
    is_executable_file(&canonical).then(|| canonical.to_string_lossy().to_string())
}

fn canonical_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

struct TailCursor {
    path: Option<PathBuf>,
    config_dir: PathBuf,
    project_dir: PathBuf,
    offset: u64,
    attach_existing: bool,
    started_at: SystemTime,
}

impl TailCursor {
    fn new(path: Option<PathBuf>, config_dir: &Path, attach_existing: bool) -> Result<Self> {
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
    fn detects_prompt_left_in_tty_input() {
        let output = "❯ Write a compact document for SDK users\nContext 0% /mcp";
        assert!(tty_output_still_editing_prompt(
            output,
            "Write a compact document for SDK users"
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
    fn resolves_real_claude_from_current_directory_before_path() {
        let current = tempfile::tempdir().unwrap();
        let path_dir = tempfile::tempdir().unwrap();
        let own = current.path().join("cctty");
        let current_claude = current.path().join("claude");
        let path_claude = path_dir.path().join("claude");
        write_executable(&own);
        write_executable(&current_claude);
        write_executable(&path_claude);

        let resolved = resolve_claude_path_from(
            Some(&own),
            Some(current.path()),
            None,
            None,
            Some(OsString::from(path_dir.path().as_os_str())),
        )
        .unwrap();

        assert_eq!(
            resolved,
            std::fs::canonicalize(current_claude)
                .unwrap()
                .to_string_lossy()
        );
    }

    #[test]
    fn skips_cctty_itself_when_resolving_path_claude() {
        let current = tempfile::tempdir().unwrap();
        let path_dir = tempfile::tempdir().unwrap();
        let own = current.path().join("claude");
        let path_claude = path_dir.path().join("claude");
        write_executable(&own);
        write_executable(&path_claude);
        let path_env = std::env::join_paths([current.path(), path_dir.path()]).unwrap();

        let resolved =
            resolve_claude_path_from(Some(&own), Some(current.path()), None, None, Some(path_env))
                .unwrap();

        assert_eq!(
            resolved,
            std::fs::canonicalize(path_claude)
                .unwrap()
                .to_string_lossy()
        );
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

    fn write_executable(path: &Path) {
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
    }
}
