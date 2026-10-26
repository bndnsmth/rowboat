//! Platform credential storage. Password bytes never enter argv or profile JSON.
use anyhow::{Result, ensure};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Rowboat currently supports Linux and macOS");

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && !id.contains('\0'),
        "Connection ID must not be empty or contain NUL"
    );
    Ok(())
}

fn validate_password(password: &str) -> Result<()> {
    ensure!(
        !password.contains('\0'),
        "Keyring credentials must not contain NUL"
    );
    // Keep the same contract across platforms, including Linux's helper limit.
    ensure!(
        password.len() < 8192,
        "Keyring credentials must be shorter than 8192 UTF-8 bytes"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_preserves_password_bytes_and_rejects_unsupported_input() {
        for password in ["", "日本語", "quote'\\:\n\n"] {
            assert!(validate_password(password).is_ok());
        }
        assert!(validate_password("has\0nul").is_err());
        assert!(validate_password(&"x".repeat(8192)).is_err());
        assert!(validate_password(&"x".repeat(8191)).is_ok());
        assert!(validate_password(&"é".repeat(4096)).is_err());
        assert!(validate_id("").is_err());
        assert!(validate_id("has\0nul").is_err());
        assert!(validate_id("--option-looking-日本語").is_ok());
    }
}
