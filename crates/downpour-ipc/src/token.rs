//! Per-daemon-session authentication token with redacted formatting.

use std::fmt;

use thiserror::Error;

/// Token generation or wire-decoding failure.
#[derive(Debug, Error)]
pub enum TokenError {
    /// The deliberate pre-build scaffold has no generator yet.
    #[error("IPC token generation is not implemented")]
    Unavailable,
}

/// A 256-bit per-daemon secret. Formatting never reveals it.
#[derive(Clone, Eq, PartialEq)]
pub struct SessionToken([u8; 32]);

impl SessionToken {
    /// Construct deterministic token bytes for tests and injected daemon configuration.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Generate a fresh token from the operating-system CSPRNG.
    pub fn generate() -> Result<Self, TokenError> {
        Err(TokenError::Unavailable)
    }

    /// Compare one 64-character wire token without exposing this token through formatting.
    #[must_use]
    pub fn matches_wire(&self, _candidate: &str) -> bool {
        false
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionToken([redacted])")
    }
}

impl fmt::Display for SessionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}
