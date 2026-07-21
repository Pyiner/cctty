use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{CcttyError, Result};
use crate::logging;
use crate::pty::{PtyProcess, PtySpawnSpec};
use crate::runner::{
    claude_path::resolve_claude_path_with_options, interactive_claude_env,
    interactive_claude_unset_env, plain_tty_output,
};

pub const DEFAULT_AUTH_LOGIN_TIMEOUT: Duration = Duration::from_secs(3600);

const AUTH_LOGIN_POLL: Duration = Duration::from_millis(100);
const PTY_TERMINATE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct AuthLoginOptions {
    pub passthrough_args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub claude_path: Option<PathBuf>,
    /// Environment values applied only to this Claude auth process.
    ///
    /// Values override cctty's interactive defaults without mutating the
    /// parent process environment, which keeps concurrent auth sessions
    /// isolated from one another.
    pub env: HashMap<String, String>,
    pub timeout: Duration,
}

impl Default for AuthLoginOptions {
    fn default() -> Self {
        Self {
            passthrough_args: vec!["auth".to_owned(), "login".to_owned()],
            cwd: None,
            claude_path: None,
            env: HashMap::new(),
            timeout: DEFAULT_AUTH_LOGIN_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AuthStatusOptions {
    pub cwd: Option<PathBuf>,
    pub claude_path: Option<PathBuf>,
    /// Environment values applied only to this Claude status process.
    pub env: HashMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthLoginEvent {
    Started { command: String, args: Vec<String> },
    AuthorizationUrl { url: String },
    InputRequested { input: String, prompt: String },
    Success { message: String },
    Error { message: String },
    Exit { exit_code: i32 },
}

pub struct AuthLoginSession {
    input: AuthLoginInput,
    events: Option<mpsc::Receiver<AuthLoginEvent>>,
    join: Option<JoinHandle<Result<i32>>>,
}

impl AuthLoginSession {
    pub fn start(options: AuthLoginOptions) -> Result<Self> {
        let cwd = match options.cwd {
            Some(cwd) => cwd,
            None => std::env::current_dir()?,
        };
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        let claude = resolve_claude_path_with_options(Some(&cwd), options.claude_path.as_deref())?;
        let mut env = interactive_claude_env();
        env.extend(options.env);
        logging::event(format!(
            "auth_login_session_spawn claude={} args={}",
            claude,
            options.passthrough_args.len()
        ));

        let process = PtyProcess::spawn(&PtySpawnSpec {
            command: claude.clone(),
            args: options.passthrough_args.clone(),
            cwd,
            env,
            unset_env: interactive_claude_unset_env(&options.passthrough_args),
        })?;
        let (event_tx, event_rx) = mpsc::channel(64);
        let (input_tx, input_rx) = mpsc::channel(8);
        let input = AuthLoginInput::new(input_tx);
        let join = tokio::spawn(run_auth_login_session(
            process,
            input_rx,
            event_tx,
            claude,
            options.passthrough_args,
            options.timeout,
        ));

        Ok(Self {
            input,
            events: Some(event_rx),
            join: Some(join),
        })
    }

    pub fn input(&self) -> AuthLoginInput {
        self.input.clone()
    }

    pub fn take_events(&mut self) -> mpsc::Receiver<AuthLoginEvent> {
        self.events
            .take()
            .expect("auth login events receiver already taken")
    }

    pub async fn wait(mut self) -> Result<i32> {
        let join = self
            .join
            .take()
            .expect("auth login session completion already taken");
        match join.await {
            Ok(result) => result,
            Err(error) => Err(CcttyError::Tty(format!(
                "Claude auth login task failed: {error}"
            ))),
        }
    }
}

impl Drop for AuthLoginSession {
    fn drop(&mut self) {
        self.input.close();
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

#[derive(Clone)]
pub struct AuthLoginInput {
    sender: Arc<Mutex<Option<mpsc::Sender<String>>>>,
}

impl AuthLoginInput {
    fn new(sender: mpsc::Sender<String>) -> Self {
        Self {
            sender: Arc::new(Mutex::new(Some(sender))),
        }
    }

    pub async fn submit_code(&self, code: impl Into<String>) -> Result<()> {
        let sender = self
            .sender
            .lock()
            .map_err(|_| CcttyError::Tty("Claude auth login input lock failed".to_owned()))?
            .clone()
            .ok_or_else(|| CcttyError::Tty("Claude auth login session is closed".to_owned()))?;
        sender
            .send(code.into())
            .await
            .map_err(|_| CcttyError::Tty("Claude auth login session is closed".to_owned()))
    }

    pub fn close(&self) {
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
    }
}

pub async fn auth_status_json(options: AuthStatusOptions) -> Result<Value> {
    let cwd = match options.cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir()?,
    };
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let claude = resolve_claude_path_with_options(Some(&cwd), options.claude_path.as_deref())?;
    let output = tokio::process::Command::new(claude)
        .args(["auth", "status", "--json"])
        .current_dir(cwd)
        .envs(options.env)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(CcttyError::Tty(if detail.is_empty() {
            format!("Claude Code auth status exited with {}.", output.status)
        } else {
            detail
        }));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

async fn run_auth_login_session(
    mut process: PtyProcess,
    mut input_rx: mpsc::Receiver<String>,
    event_tx: mpsc::Sender<AuthLoginEvent>,
    claude: String,
    args: Vec<String>,
    timeout: Duration,
) -> Result<i32> {
    send_auth_event(
        &event_tx,
        AuthLoginEvent::Started {
            command: claude,
            args,
        },
    )
    .await?;

    let started = Instant::now();
    let mut state = AuthLoginEventState::default();
    loop {
        while let Ok(line) = input_rx.try_recv() {
            process.write_all(line.as_bytes())?;
            process.write_all(b"\n")?;
        }

        process_auth_login_output(&process.recent_output(), &mut state, &event_tx).await?;

        if let Some(code) = process.try_wait()? {
            process_auth_login_output(&process.recent_output(), &mut state, &event_tx).await?;
            if code != 0 {
                send_auth_event(
                    &event_tx,
                    AuthLoginEvent::Error {
                        message: format!("Claude auth login exited with code {code}"),
                    },
                )
                .await?;
            }
            send_auth_event(&event_tx, AuthLoginEvent::Exit { exit_code: code }).await?;
            logging::event(format!("auth_login_session_exit exit_code={code}"));
            return Ok(code);
        }

        if started.elapsed() >= timeout {
            process.terminate(PTY_TERMINATE_TIMEOUT);
            send_auth_event(
                &event_tx,
                AuthLoginEvent::Error {
                    message: "Claude auth login timed out".to_owned(),
                },
            )
            .await?;
            send_auth_event(&event_tx, AuthLoginEvent::Exit { exit_code: 124 }).await?;
            logging::event("auth_login_session_timeout");
            return Ok(124);
        }

        tokio::time::sleep(AUTH_LOGIN_POLL).await;
    }
}

async fn send_auth_event(
    event_tx: &mpsc::Sender<AuthLoginEvent>,
    event: AuthLoginEvent,
) -> Result<()> {
    event_tx
        .send(event)
        .await
        .map_err(|_| CcttyError::Tty("Claude auth login event receiver closed".to_owned()))
}

#[derive(Default)]
struct AuthLoginEventState {
    seen_urls: HashSet<String>,
    input_requested: bool,
    success: bool,
}

async fn process_auth_login_output(
    output: &str,
    state: &mut AuthLoginEventState,
    event_tx: &mpsc::Sender<AuthLoginEvent>,
) -> Result<()> {
    let plain = plain_tty_output(output);
    for url in auth_login_urls(&plain) {
        if state.seen_urls.insert(url.clone()) {
            send_auth_event(event_tx, AuthLoginEvent::AuthorizationUrl { url }).await?;
        }
    }
    if !state.input_requested && plain.contains("Paste code here if prompted") {
        state.input_requested = true;
        send_auth_event(
            event_tx,
            AuthLoginEvent::InputRequested {
                input: "authorization_code".to_owned(),
                prompt: "Paste code here if prompted >".to_owned(),
            },
        )
        .await?;
    }
    if !state.success && plain.contains("Login successful") {
        state.success = true;
        send_auth_event(
            event_tx,
            AuthLoginEvent::Success {
                message: "Login successful.".to_owned(),
            },
        )
        .await?;
    }
    Ok(())
}

fn auth_login_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = text;
    while let Some(offset) = rest.find("https://") {
        let candidate = &rest[offset..];
        let end = candidate
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>' | ')' | ']'))
            .unwrap_or(candidate.len());
        let url = candidate[..end]
            .trim_end_matches(|ch| matches!(ch, '.' | ',' | ';' | ':'))
            .to_owned();
        if !url.is_empty() {
            urls.push(url);
        }
        rest = &candidate[end..];
    }
    urls
}

pub(crate) fn auth_event_to_json(event: &AuthLoginEvent) -> Result<Value> {
    Ok(serde_json::to_value(event)?)
}
