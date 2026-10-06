use reqwest::StatusCode;

/// Errors from renting, driving, or releasing a Popcorn session.
#[derive(Debug, thiserror::Error)]
pub enum PopcornError {
    /// The configuration is incomplete or malformed.
    #[error("popcorn configuration error: {message}")]
    Configuration {
        /// Human-readable explanation.
        message: String,
    },
    /// The control plane could not be reached.
    #[error("popcorn control plane request failed: {0}")]
    Transport(#[from] reqwest::Error),
    /// The control plane rejected the request.
    #[error("popcorn control plane returned {status}: {body}")]
    ControlPlane {
        /// HTTP status.
        status: StatusCode,
        /// Response body, truncated.
        body: String,
    },
    /// The MCP server could not be reached, authorized, or understood.
    #[error("popcorn MCP error: {message}")]
    Mcp {
        /// Human-readable explanation.
        message: String,
    },
    /// A Popcorn response did not carry a field the session needs.
    #[error("popcorn response is missing `{field}`")]
    MissingField {
        /// Missing field name.
        field: &'static str,
    },
    /// The account has no session credit left.
    #[error("popcorn has no session credit left: {message}")]
    InsufficientCredit {
        /// What the human should do next.
        message: String,
    },
    /// A browser action failed.
    #[error("popcorn browser error: {0}")]
    Browser(#[from] chromiumoxide::error::CdpError),
}
