use std::collections::VecDeque;
use std::io::{BufRead, BufReader as StdBufReader, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{CcttyError, Result};

use super::single_line_log_text;

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

pub(super) fn mcp_tool_names_for_log(response: &Value) -> Option<String> {
    let tools = response
        .get("result")
        .and_then(|result| result.get("tools"))
        .and_then(Value::as_array)?;
    let mut names = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(single_line_log_text)
        .collect::<Vec<_>>();
    if names.is_empty() {
        return Some("count=0 names=-".to_owned());
    }
    let total = names.len();
    names.truncate(30);
    let suffix = if total > names.len() { ",..." } else { "" };
    Some(format!("count={total} names={}{}", names.join(","), suffix))
}

pub(super) fn rewrite_mcp_tools_for_claude(mut response: Value) -> Value {
    let Some(tools) = response
        .get_mut("result")
        .and_then(|result| result.get_mut("tools"))
        .and_then(Value::as_array_mut)
    else {
        return response;
    };
    for tool in tools {
        let Some(object) = tool.as_object_mut() else {
            continue;
        };
        if object.get("name").and_then(Value::as_str) == Some("AskUserQuestion") {
            object.insert(
                "name".to_owned(),
                Value::String("ask_user_question".to_owned()),
            );
        }
    }
    response
}

pub(super) fn rewrite_mcp_tool_call_for_sdk(mut message: Value) -> Value {
    if message.get("method").and_then(Value::as_str) != Some("tools/call") {
        return message;
    }
    let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) else {
        return message;
    };
    if params.get("name").and_then(Value::as_str) == Some("ask_user_question") {
        params.insert(
            "name".to_owned(),
            Value::String("AskUserQuestion".to_owned()),
        );
    }
    message
}

pub(super) struct SdkStreamState {
    mcp_servers: Vec<McpServerStatus>,
    pub(super) mcp_runtime: Option<SdkMcpRuntime>,
    pub(super) deferred_input: VecDeque<Value>,
    next_mcp_request_id: u64,
}

impl SdkStreamState {
    pub(super) fn new(args: &[String]) -> Self {
        let mut state = Self {
            mcp_servers: mcp_servers_from_args(args),
            mcp_runtime: None,
            deferred_input: VecDeque::new(),
            next_mcp_request_id: 0,
        };
        state.add_sdk_mcp_servers(sdk_mcp_servers_from_args(args));
        state
    }

    pub(super) fn add_sdk_mcp_servers(&mut self, names: Vec<String>) {
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

    pub(super) fn sdk_mcp_server_names(&self) -> Vec<String> {
        self.mcp_servers
            .iter()
            .filter(|server| server.kind == "sdk")
            .map(|server| server.name.clone())
            .collect()
    }

    pub(super) fn next_mcp_request_id(&mut self) -> String {
        self.next_mcp_request_id += 1;
        format!("cctty-mcp-{}", self.next_mcp_request_id)
    }

    pub(super) fn mcp_status(&self) -> Value {
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
pub(super) struct SdkMcpRuntime {
    pub(super) socket_path: PathBuf,
    pub(super) listener: UnixListener,
    servers: Vec<String>,
}

#[cfg(unix)]
impl Drop for SdkMcpRuntime {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(not(unix))]
pub(super) struct SdkMcpRuntime {
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

pub(super) fn sdk_mcp_servers_from_initialize(value: &Value) -> Vec<String> {
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

#[cfg(unix)]
pub(super) fn create_sdk_mcp_runtime(servers: Vec<String>) -> Result<SdkMcpRuntime> {
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
pub(super) fn create_sdk_mcp_runtime(servers: Vec<String>) -> Result<SdkMcpRuntime> {
    let _ = servers;
    Err(CcttyError::Usage(
        "SDK MCP proxy is only supported on Unix platforms".to_owned(),
    ))
}

#[cfg(unix)]
pub(super) fn sdk_mcp_runtime_socket_path(runtime: &SdkMcpRuntime) -> String {
    runtime.socket_path.to_string_lossy().into_owned()
}

#[cfg(not(unix))]
pub(super) fn sdk_mcp_runtime_socket_path(_runtime: &SdkMcpRuntime) -> String {
    "-".to_owned()
}

pub(super) fn sdk_mcp_runtime_servers(runtime: &SdkMcpRuntime) -> &[String] {
    &runtime.servers
}

pub(super) fn args_with_mcp_runtime(
    args: &[String],
    runtime: &SdkMcpRuntime,
) -> Result<Vec<String>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_sdk_ask_user_question_mcp_tool_name_for_claude() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [
                    { "name": "AskUserQuestion" },
                    { "name": "GetWorkspaceDiff" }
                ]
            }
        });
        let rewritten = rewrite_mcp_tools_for_claude(response);
        assert_eq!(rewritten["result"]["tools"][0]["name"], "ask_user_question");
        assert_eq!(rewritten["result"]["tools"][1]["name"], "GetWorkspaceDiff");
    }

    #[test]
    fn rewrites_claude_ask_user_question_mcp_tool_call_for_sdk() {
        let message = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "ask_user_question",
                "arguments": {}
            }
        });
        let rewritten = rewrite_mcp_tool_call_for_sdk(message);
        assert_eq!(rewritten["params"]["name"], "AskUserQuestion");
    }
}
