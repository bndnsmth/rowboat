//! Invalid helper requests must fail before accessing any Keychain or GUI.
#![cfg(target_os = "macos")]

#[test]
fn private_keychain_helper_rejects_invalid_input_without_opening_a_store() {
    for arguments in [
        vec!["--rowboat-keychain-lookup"],
        vec!["--rowboat-keychain-lookup", ""],
        vec!["--rowboat-keychain-lookup", "", "extra"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_rowboatctl"))
            .args(arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Keychain lookup failed"));
    }
}
