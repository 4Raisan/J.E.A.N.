//! Errors that say what happened to the request, and what is safe to do next.
//!
//! The distinction that matters throughout this crate is between *transport* — the provider was
//! unreachable, too slow, or returned something unreadable — and *content* — a plan that does not
//! validate. Only the first is retried, and never anything beyond one model call.

/// Every fallible call in this crate returns this, so `Result` never means the std one here.
pub type Result<T> = std::result::Result<T, ModelError>;

/// Whether a failure may be retried.
///
/// A transport error before the request was fully written, or one that says the provider never
/// answered, can be retried. A provider that answered with a body we could not read is *not*
/// retryable by this rule, because the answer existed: replanning is the honest response to a bad
/// plan, and a retry would be a second attempt at the same decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retryable {
    Yes,
    No,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// `model.provider = "none"`, so planning is not available at all.
    #[error(
        "no model provider is configured (model.provider = \"none\"); the deterministic commands \
         still work, and `jean model status` says what to set"
    )]
    ProviderDisabled,

    #[error("provider {0:?} is not implemented by this build")]
    UnsupportedProvider(String),

    /// The credential environment variable names an unset value where one is required.
    #[error(
        "the credential is not available: environment variable {name:?} is unset or blank. Its \
         value was never stored in a file, record or log by J.E.A.N."
    )]
    MissingCredential { name: String },

    /// `model.max_prompt_bytes` exceeded, checked before connecting.
    #[error("the prompt would be {bytes} bytes, over model.max_prompt_bytes = {limit}")]
    PromptTooLarge { bytes: usize, limit: usize },

    #[error("could not connect to {endpoint}: {reason}")]
    Connect { endpoint: String, reason: String },

    /// The socket deadline passed. Distinct from Cancelled: nobody pressed anything.
    #[error("the model request exceeded model.request_timeout_secs = {timeout_secs}s")]
    TimedOut { timeout_secs: u64 },

    /// The caller's cancellation flag was seen while talking to the provider.
    #[error("the model request was cancelled before it completed")]
    Cancelled,

    #[error("writing the request failed: {0}")]
    Write(String),

    /// The response body exceeded the configured ceiling. Reported rather than truncated: half a
    /// JSON document is not a smaller plan.
    #[error(
        "the response exceeded model.max_response_bytes = {limit} bytes (read {read} so far); it \
         was not truncated into a plan"
    )]
    ResponseTooLarge { read: usize, limit: usize },

    /// Status, headers or framing were not what this build supports.
    #[error("the provider's response could not be read as HTTP/1.1: {0}")]
    Protocol(String),

    /// A status the provider returned that means the request will not succeed as sent.
    #[error("the provider answered {status} for {endpoint}: {detail}")]
    HttpStatus {
        status: u16,
        endpoint: String,
        detail: String,
    },

    /// The body was valid JSON but not the shape this provider is expected to use.
    #[error("the response body does not match what {provider} should return: {detail}")]
    ResponseShape {
        provider: &'static str,
        detail: String,
    },

    #[error("the provider followed no redirect policy this build allows: {0}")]
    Redirect(String),
}

impl ModelError {
    /// Whether this failure may be answered with another attempt at the same request.
    pub fn retryable(&self) -> Retryable {
        match self {
            // Nothing reached the model, or it never answered: asking again is a new attempt.
            Self::Connect { .. } | Self::TimedOut { .. } | Self::Write(_) => Retryable::Yes,
            // A 429 or a 5xx is the provider asking to be asked again later. A 4xx that is not
            // 429 means the request itself is wrong, and repeating it is noise.
            Self::HttpStatus { status, .. } if *status == 429 || *status >= 500 => Retryable::Yes,
            Self::HttpStatus { .. } => Retryable::No,
            // A body arrived, so a decision was made and we could not read it. Re-asking is the
            // planner's call after it shows the operator what happened, not a silent retry here.
            Self::ResponseTooLarge { .. } | Self::Protocol(_) | Self::ResponseShape { .. } => {
                Retryable::No
            }
            // The operator stopped this. Nothing may restart it.
            Self::Cancelled | Self::ProviderDisabled | Self::PromptTooLarge { .. } => Retryable::No,
            Self::MissingCredential { .. } | Self::UnsupportedProvider(_) | Self::Redirect(_) => {
                Retryable::No
            }
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.retryable() == Retryable::Yes
    }

    /// A short, stable code for the record and for `jean status`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ProviderDisabled => "provider_disabled",
            Self::UnsupportedProvider(_) => "unsupported_provider",
            Self::MissingCredential { .. } => "missing_credential",
            Self::PromptTooLarge { .. } => "prompt_too_large",
            Self::Connect { .. } => "connect_failed",
            Self::TimedOut { .. } => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Write(_) => "request_write_failed",
            Self::ResponseTooLarge { .. } => "response_too_large",
            Self::Protocol(_) => "protocol_error",
            Self::HttpStatus { .. } => "http_status",
            Self::ResponseShape { .. } => "response_shape",
            Self::Redirect(_) => "redirect_refused",
        }
    }
}
