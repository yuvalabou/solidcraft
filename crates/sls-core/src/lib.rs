//! Pure-Rust SLS 3D nesting core (`Class::Shared`, zero UI/GPU dependencies).

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

pub mod io;
pub mod model;

use thiserror::Error;

/// Top-level error type for `solidcraft-sls-core`.
#[derive(Debug, Error)]
pub enum SlsError {
    /// Input mesh or job data is invalid or malformed.
    #[error("invalid SLS input: {0}")]
    InvalidInput(String),
}

/// Convenience result alias for `solidcraft-sls-core`.
pub type Result<T> = std::result::Result<T, SlsError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_displays_message() {
        let err = SlsError::InvalidInput("empty mesh".to_owned());
        assert_eq!(err.to_string(), "invalid SLS input: empty mesh");
    }
}
