//! Pure-Rust 3D nesting core (`Class::Shared`, zero UI/GPU dependencies).

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

pub mod collision;
pub mod compact;
pub mod interlock;
pub mod io;
pub mod manifold;
pub mod model;
pub mod nest2d;
pub mod nest3d;
pub mod recovery;
pub mod report;
pub mod sinterbox;
pub mod slice;
pub mod thermal;
pub mod voxel;

use thiserror::Error;

/// Top-level error type for `nestcraft-core`.
#[derive(Debug, Error)]
pub enum NestCraftError {
    /// Input mesh or job data is invalid or malformed.
    #[error("invalid NestCraft input: {0}")]
    InvalidInput(String),
}

/// Convenience result alias for `nestcraft-core`.
pub type Result<T> = std::result::Result<T, NestCraftError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_displays_message() {
        let err = NestCraftError::InvalidInput("empty mesh".to_owned());
        assert_eq!(err.to_string(), "invalid NestCraft input: empty mesh");
    }
}
