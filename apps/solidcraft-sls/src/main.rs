//! SolidCraft SLS 3D nesting application entry point.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

fn main() {
    let _placeholder = solidcraft_sls_core::SlsError::InvalidInput(String::new());
}

#[cfg(test)]
mod tests {
    #[test]
    fn links_sls_core_error() {
        let err = solidcraft_sls_core::SlsError::InvalidInput("test".to_owned());
        assert!(err.to_string().contains("test"));
    }
}
