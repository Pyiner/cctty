use thiserror::Error;

pub type Result<T> = std::result::Result<T, CcttyError>;

#[derive(Debug, Error)]
pub enum CcttyError {
    #[error("{0}")]
    Usage(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Claude CLI not found: {0}")]
    ClaudeNotFound(String),
    #[error("Claude TTY failed: {0}")]
    Tty(String),
    #[error("Claude transcript failed: {0}")]
    Transcript(String),
    #[error("Claude timed out: {0}")]
    Timeout(String),
}

impl CcttyError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            Self::ClaudeNotFound(_) => 127,
            Self::Timeout(_) => 124,
            Self::Io(_) | Self::Json(_) | Self::Tty(_) | Self::Transcript(_) => 1,
        }
    }
}
