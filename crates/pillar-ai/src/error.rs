//! pillar-ai error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AiError {
    #[error("{0}")]
    Aborted(String),
    #[error("{0}")]
    Other(String),
}
