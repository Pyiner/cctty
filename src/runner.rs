use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::args::{CommandMode, InputFormat, Invocation, OutputFormat};
use crate::error::{CcttyError, Result};
use crate::pty::{PtyProcess, PtySpawnSpec};
use crate::transcript::{TranscriptState, claude_config_dir, read_complete_lines, transcript_path};

const COMPLETION_IDLE: Duration = Duration::from_millis(1_500);
const TRANSCRIPT_POLL: Duration = Duration::from_millis(80);
const TRUST_PROMPT_SETTLE: Duration = Duration::from_millis(800);
const TTY_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const TTY_READY_SETTLE: Duration = Duration::from_millis(250);
const PERMISSION_PROMPT_TIMEOUT: Duration = Duration::from_secs(8);
const TTY_PERMISSION_TRANSCRIPT_GRACE: Duration = Duration::from_millis(1_500);
const RUN_TIMEOUT: Duration = Duration::from_secs(3600);

pub async fn run(invocation: Invocation) -> Result<i32> {
    match invocation.mode {
        CommandMode::Passthrough => run_passthrough(&invocation).await,
        CommandMode::Print => run_print(invocation).await,
    }
}

async fn run_passthrough(invocation: &Invocation) -> Result<i32> {
    let claude = resolve_claude_path()?;
    let mut child = tokio::process::Command::new(claude)
        .args(&invocation.passthrough_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let status = child.wait().await?;
    Ok(status.code().unwrap_or(1))
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
    let env = HashMap::new();

    let mut process = PtyProcess::spawn(&PtySpawnSpec {
        command: claude,
        args: invocation.passthrough_args.clone(),
        cwd,
        env,
    })?;
    prepare_tty_for_prompt(&mut process).await?;

    let mut tail = TailCursor::new(transcript, &config_dir)?;

    match invocation.input_format {
        InputFormat::Text => {
            let prompt = prompt_from_invocation(&invocation)?;
            let outcome =
                submit_prompt_and_tail(&mut process, &mut tail, &prompt, invocation.output_format)
                    .await?;
            write_final_output(&outcome, invocation.output_format)?;
        }
        InputFormat::StreamJson => {
            run_stream_json(
                &mut process,
                &mut tail,
                invocation.output_format,
                invocation.permission_prompt_tool_stdio,
            )
            .await?;
        }
    }

    process.kill();
    if invocation.no_session_persistence {
        tail.remove_current_transcript()?;
    }
    Ok(0)
}

async fn run_stream_json(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    output_format: OutputFormat,
    permission_prompt_tool_stdio: bool,
) -> Result<()> {
    if output_format != OutputFormat::StreamJson {
        return Err(CcttyError::Usage(
            "--input-format stream-json currently requires --output-format stream-json".to_owned(),
        ));
    }

    let mut input = spawn_stdin_json_reader();
    while let Some(value) = input.recv().await {
        match value.get("type").and_then(Value::as_str) {
            Some("control_request") => {
                handle_control_request(process, &value)?;
            }
            Some("control_response") => {}
            Some("control_cancel_request") => {}
            Some("user") => {
                let prompt = user_prompt_from_sdk_message(&value)?;
                let _ = submit_prompt_and_tail_stream(
                    process,
                    tail,
                    &mut input,
                    &prompt,
                    output_format,
                    permission_prompt_tool_stdio,
                )
                .await?;
            }
            _ => {}
        }
    }
    Ok(())
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

fn handle_control_request(process: &mut PtyProcess, value: &Value) -> Result<()> {
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
        "initialize" => control_success(request_id, json!({})),
        "interrupt" => {
            process.interrupt()?;
            control_success(request_id, Value::Null)
        }
        _ => control_error(
            request_id,
            format!("Unsupported control request: {subtype}"),
        ),
    };
    println!("{}", serde_json::to_string(&response)?);
    std::io::stdout().flush()?;
    Ok(())
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
) -> Result<TranscriptState> {
    tail.prepare_offset()?;
    process.write_all(&bracketed_paste_input(prompt))?;
    tail_until_complete(tail, output_format).await
}

async fn submit_prompt_and_tail_stream(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    input: &mut mpsc::Receiver<Value>,
    prompt: &str,
    output_format: OutputFormat,
    permission_prompt_tool_stdio: bool,
) -> Result<TranscriptState> {
    tail.prepare_offset()?;
    process.write_all(&bracketed_paste_input(prompt))?;
    tail_until_complete_stream(
        process,
        tail,
        input,
        output_format,
        permission_prompt_tool_stdio,
    )
    .await
}

async fn tail_until_complete(
    tail: &mut TailCursor,
    output_format: OutputFormat,
) -> Result<TranscriptState> {
    let started = Instant::now();
    let mut last_activity = Instant::now();
    let mut state = TranscriptState::default();

    loop {
        if started.elapsed() > RUN_TIMEOUT {
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
                        if output_format == OutputFormat::StreamJson {
                            println!("{}", serde_json::to_string(&value)?);
                            std::io::stdout().flush()?;
                        }
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
            return Ok(state);
        }
        if !state.assistant_text.is_empty() && last_activity.elapsed() >= COMPLETION_IDLE {
            if output_format == OutputFormat::StreamJson && !state.saw_result {
                let value = synthetic_result(&state, started.elapsed());
                println!("{}", serde_json::to_string(&value)?);
                std::io::stdout().flush()?;
                state.apply(&value);
            }
            return Ok(state);
        }
        tokio::time::sleep(TRANSCRIPT_POLL).await;
    }
}

async fn tail_until_complete_stream(
    process: &mut PtyProcess,
    tail: &mut TailCursor,
    input: &mut mpsc::Receiver<Value>,
    output_format: OutputFormat,
    permission_prompt_tool_stdio: bool,
) -> Result<TranscriptState> {
    let started = Instant::now();
    let mut last_activity = Instant::now();
    let mut state = TranscriptState::default();
    let mut permission = PermissionBridge::new(permission_prompt_tool_stdio);

    loop {
        if started.elapsed() > RUN_TIMEOUT {
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
                        permission
                            .maybe_request_tool_permission(process, input, &value)
                            .await?;
                        state.apply(&value);
                        if output_format == OutputFormat::StreamJson {
                            println!("{}", serde_json::to_string(&value)?);
                            std::io::stdout().flush()?;
                        }
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

        if state.saw_result {
            return Ok(state);
        }
        if permission.denied_current_turn() && last_activity.elapsed() >= COMPLETION_IDLE {
            if output_format == OutputFormat::StreamJson && !state.saw_result {
                let value = synthetic_permission_denied_result(&state, started.elapsed());
                println!("{}", serde_json::to_string(&value)?);
                std::io::stdout().flush()?;
                state.apply(&value);
            }
            return Ok(state);
        }
        if !state.assistant_text.is_empty() && last_activity.elapsed() >= COMPLETION_IDLE {
            if output_format == OutputFormat::StreamJson && !state.saw_result {
                let value = synthetic_result(&state, started.elapsed());
                println!("{}", serde_json::to_string(&value)?);
                std::io::stdout().flush()?;
                state.apply(&value);
            }
            return Ok(state);
        }
        tokio::time::sleep(TRANSCRIPT_POLL).await;
    }
}

struct PermissionBridge {
    enabled: bool,
    requested_tool_use_ids: HashSet<String>,
    handled_tool_keys: Vec<ToolKey>,
    pending_tty_permission: Option<PendingTtyPermission>,
    denied_current_turn: bool,
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
            if !self.requested_tool_use_ids.insert(tool_use.id.clone()) {
                continue;
            }
            if self.has_handled_tool(&tool_use) {
                continue;
            }
            requested = true;
            self.mark_tool_handled(&tool_use);
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
        println!("{}", serde_json::to_string(&request)?);
        std::io::stdout().flush()?;
        let response = wait_for_control_response(input, &request_id).await?;
        if apply_permission_decision(process, tool_use, &response).await? {
            self.denied_current_turn = true;
        }
        Ok(())
    }

    fn has_handled_tool(&self, tool_use: &ToolUse) -> bool {
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

#[derive(Debug)]
struct PendingTtyPermission {
    tool_use: ToolUse,
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
    input: String,
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
            input: serde_json::to_string(&tool_use.input).unwrap_or_default(),
        }
    }

    fn matches(&self, other: &Self) -> bool {
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

fn normalize_tool_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
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
    if !plain_tty_output_has_permission_prompt(&plain_output) {
        return None;
    }
    let command = bash_command_from_tty_permission_prompt(&output)?;
    Some(ToolUse {
        id: tool_use_id,
        name: "Bash".to_owned(),
        input: json!({ "command": command }),
    })
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
    let behavior = permission_behavior(control_response)
        .unwrap_or_else(|| "allow".to_owned())
        .to_ascii_lowercase();
    let denied = behavior == "deny";
    let saw_prompt = wait_for_tty_permission_prompt(process, tool_use).await;
    if denied {
        if saw_prompt {
            process.write_all(b"2\r")?;
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

fn tty_output_has_permission_prompt(output: &str, tool_use: &ToolUse) -> bool {
    let output = plain_tty_output(output);
    let tool_name = tool_use.name.as_str();
    plain_tty_output_has_permission_prompt_for_tool(&output, tool_name)
}

fn plain_tty_output_has_permission_prompt(output: &str) -> bool {
    plain_tty_output_has_permission_prompt_for_tool(output, "Bash")
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

fn output_mentions_tool_result(output: &str, tool_use: &ToolUse) -> bool {
    let output = plain_tty_output(output);
    output.contains(&tool_use.name)
        && (output.contains("Done")
            || output.contains("Running")
            || output.contains("Waiting")
            || output.contains("⎿"))
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
        "usage": {},
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
        "usage": {},
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
    if let Some(path) = std::env::var_os("CCTTY_CLAUDE_PATH") {
        let path = path.to_string_lossy().to_string();
        if !path.trim().is_empty() {
            return Ok(path);
        }
    }
    which::which("claude")
        .map(|path| path.to_string_lossy().to_string())
        .map_err(|error| CcttyError::ClaudeNotFound(error.to_string()))
}

struct TailCursor {
    path: Option<PathBuf>,
    config_dir: PathBuf,
    project_dir: PathBuf,
    offset: u64,
}

impl TailCursor {
    fn new(path: Option<PathBuf>, config_dir: &Path) -> Result<Self> {
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
        })
    }

    fn prepare_offset(&mut self) -> Result<()> {
        if self.path.is_none() {
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
            self.path = newest_transcript(&self.project_dir)?;
        }
        let _ = &self.config_dir;
        Ok(self.path.clone())
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
