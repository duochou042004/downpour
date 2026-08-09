//! Per-daemon-session authentication token with redacted formatting.

use std::fmt;

use thiserror::Error;

use crate::SecretString;

/// Token generation or wire-decoding failure.
#[derive(Debug, Error)]
pub enum TokenError {
    /// The operating system could not provide secure entropy.
    #[error("the operating system could not generate an IPC session token")]
    Entropy,
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
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| TokenError::Entropy)?;
        Ok(Self(bytes))
    }

    /// Compare one 64-character wire token without exposing this token through formatting.
    #[must_use]
    pub fn matches_wire(&self, candidate: &str) -> bool {
        let bytes = candidate.as_bytes();
        if bytes.len() != 64 {
            return false;
        }
        let mut difference = 0_u8;
        for (index, expected) in self.0.iter().copied().enumerate() {
            let Some(high) = decode_lower_hex(bytes[index * 2]) else {
                return false;
            };
            let Some(low) = decode_lower_hex(bytes[index * 2 + 1]) else {
                return false;
            };
            difference |= expected ^ (high << 4 | low);
        }
        difference == 0
    }

    /// Render the token for the protected runtime file or hello payload.
    #[must_use]
    pub fn to_wire(&self) -> SecretString {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut wire = String::with_capacity(64);
        for byte in self.0 {
            wire.push(char::from(HEX[usize::from(byte >> 4)]));
            wire.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        SecretString::new(wire)
    }
}

fn decode_lower_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
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
