use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::error::Result;

#[derive(Debug, Default, Clone)]
pub struct TranscriptState {
    pub session_id: Option<String>,
    pub assistant_text: String,
    pub result: Option<Value>,
    pub saw_result: bool,
    pub saw_assistant: bool,
}

impl TranscriptState {
    pub fn apply(&mut self, value: &Value) {
        if self.session_id.is_none() {
            self.session_id = session_id(value);
        }
        match value.get("type").and_then(Value::as_str) {
            Some("system") => {
                if self.session_id.is_none() {
                    self.session_id = session_id(value);
                }
            }
            Some("assistant") => {
                self.saw_assistant = true;
                if self.session_id.is_none() {
                    self.session_id = session_id(value);
                }
                self.assistant_text
                    .push_str(&assistant_text(value).unwrap_or_default());
            }
            Some("result") => {
                self.saw_result = true;
                self.session_id = session_id(value).or_else(|| self.session_id.take());
                self.result = Some(value.clone());
            }
            _ => {}
        }
    }
}

pub async fn read_complete_lines(path: &Path, offset: u64) -> std::io::Result<(Vec<String>, u64)> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    if buf.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let text = String::from_utf8_lossy(&buf);
    let Some(last_newline) = text.rfind('\n') else {
        return Ok((Vec::new(), 0));
    };
    let complete = &text[..last_newline];
    let consumed = complete.len() as u64 + 1;
    Ok((complete.lines().map(ToOwned::to_owned).collect(), consumed))
}

pub fn claude_config_dir() -> Result<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
        .ok_or_else(|| {
            crate::error::CcttyError::Transcript(
                "HOME is not set; cannot locate Claude config".to_owned(),
            )
        })
}

pub fn transcript_path(config_dir: &Path, cwd: &Path, session_id: &str) -> PathBuf {
    config_dir
        .join("projects")
        .join(project_key(cwd))
        .join(format!("{session_id}.jsonl"))
}

pub fn project_key(cwd: &Path) -> String {
    let mapped = cwd
        .to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    if mapped.is_empty() {
        "-".to_owned()
    } else {
        mapped
    }
}

fn session_id(value: &Value) -> Option<String> {
    value
        .get("session_id")
        .or_else(|| value.get("sessionId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn assistant_text(value: &Value) -> Option<String> {
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
