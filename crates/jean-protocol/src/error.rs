use thiserror::Error;

pub type Result<T> = std::result::Result<T, ProtocolError>;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("configuration error: {0}")]
    Config(#[from] crate::config::ConfigError),

    #[error("tool definition error: {0}")]
    Tool(#[from] crate::tool::ToolValidationError),

    #[error("illegal task state transition: {0}")]
    Transition(#[from] crate::state::TransitionError),

    #[error("invalid task id {0:?}: {1}")]
    InvalidTaskId(String, String),

    #[error("serialization failed: {0}")]
    Serialization(String),
}
