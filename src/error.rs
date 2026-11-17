//! Errors surfaced by the thin runtime.

use crate::port::PortError;

/// Error returned by [`crate::GatewayClient`] operations.
#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    /// The gateway port could not be read.
    #[error(transparent)]
    Port(#[from] PortError),
    /// A `PUT /gw/services/{name}` request failed local validation.
    #[error("invalid registration: {0}")]
    InvalidRegistration(String),
    /// A `PATCH /gw/services/{name}` request failed local validation.
    #[error("invalid patch: {0}")]
    InvalidPatch(String),
    /// The HTTP request did not produce a usable response.
    #[error("gateway transport error: {0}")]
    Transport(String),
    /// The gateway returned a non-2xx response.
    #[error("gateway returned HTTP {status}: {message}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Stable error code from the gateway error envelope, when present.
        code: Option<String>,
        /// Human-readable message.
        message: String,
    },
}

impl SdkError {
    /// Whether the error came from an expired or missing session token.
    #[must_use]
    pub const fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Api { status: 401, .. })
    }

    /// Whether the request never produced a usable HTTP response.
    ///
    /// Connection failures land here when the gateway restarted or moved to a
    /// different port; the client refreshes a discovered base URL and retries
    /// once.
    #[must_use]
    pub const fn is_transport(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::Port(_))
    }
}
