use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GreppleError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("tool error: {0}")]
    Tool(String),
}

pub type Result<T> = std::result::Result<T, GreppleError>;
