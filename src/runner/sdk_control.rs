use std::io::Write;

use super::*;

pub(super) async fn handle_control_request(
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
