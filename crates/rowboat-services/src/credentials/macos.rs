use super::{validate_id, validate_password};
use anyhow::{Context, Result, ensure};
use security_framework::passwords::{
    PasswordOptions, delete_generic_password_options, generic_password,
    set_generic_password_options,
};
use security_framework_sys::base::errSecItemNotFound;
use std::{io::Write, process::Stdio};

const SERVICE: &str = "dev.bndnsmth.rowboat";
const LOOKUP_ARGUMENT: &str = "--rowboat-keychain-lookup";

fn options(id: &str) -> Result<PasswordOptions> {
    validate_id(id)?;
    let mut options = PasswordOptions::new_generic_password(SERVICE, id);
    // Remembering a local database password must not opt it into iCloud sync.
    options.set_access_synchronized(Some(false));
    Ok(options)
}

pub fn credential_get(connection_id: &str) -> Result<Option<String>> {
    match generic_password(options(connection_id)?) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| anyhow::anyhow!("Keychain credential is not UTF-8")),
        Err(error) if error.code() == errSecItemNotFound => Ok(None),
        Err(error) => Err(error).context("Cannot read password from macOS Keychain"),
    }
}

pub fn credential_set(connection_id: &str, password: &str) -> Result<()> {
    validate_password(password)?;
    set_generic_password_options(password.as_bytes(), options(connection_id)?)
        .context("Cannot save password in macOS Keychain")
}

pub fn credential_delete(connection_id: &str) -> Result<()> {
    match delete_generic_password_options(options(connection_id)?) {
        Ok(()) => Ok(()),
        Err(error) if error.code() == errSecItemNotFound => Ok(()),
        Err(error) => Err(error).context("Cannot remove password from macOS Keychain"),
    }
}

/// Handle the private lookup subprocess before initializing Clap, Tokio or GPUI.
/// A native Keychain call may block on authorization. Running it in an owned
/// subprocess keeps connection deadlines/cancellation enforceable: unlike a
/// spawn_blocking task, the process can be terminated and reaped explicitly.
pub fn credential_helper_main() -> Option<i32> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.first().is_none_or(|arg| arg != LOOKUP_ARGUMENT) {
        return None;
    }
    let result = (|| -> Result<()> {
        ensure!(arguments.len() == 2, "Invalid Keychain lookup request");
        let id = arguments[1].to_str().context("Invalid connection ID")?;
        let password = credential_get(id)?;
        let mut output = std::io::stdout().lock();
        serde_json::to_writer(&mut output, &password).context("Cannot return Keychain response")?;
        output.flush().context("Flush Keychain response")?;
        Ok(())
    })();
    Some(if result.is_ok() {
        0
    } else {
        // Do not log a response, connection ID, or any secret bytes.
        eprintln!(
            "macOS Keychain lookup failed; check that the keychain is unlocked and access is allowed"
        );
        1
    })
}

pub struct CredentialLookup {
    child: tokio::process::Child,
}

impl CredentialLookup {
    pub fn start(connection_id: &str) -> Result<Self> {
        validate_id(connection_id)?;
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command.args([LOOKUP_ARGUMENT, connection_id]);
        Self::spawn(command)
    }

    fn spawn(mut command: tokio::process::Command) -> Result<Self> {
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Cannot start macOS Keychain lookup")?;
        Ok(Self { child })
    }

    pub async fn get(&mut self) -> Result<Option<String>> {
        use tokio::io::AsyncReadExt;
        let mut stdout = self
            .child
            .stdout
            .take()
            .context("Lookup already consumed")?;
        let mut stderr = self
            .child
            .stderr
            .take()
            .context("Lookup stderr unavailable")?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = tokio::try_join!(
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            self.child.wait(),
        );
        let (_, _, status) = match result {
            Ok(result) => result,
            Err(error) => {
                self.cancel().await?;
                return Err(error).context("Read Keychain lookup response");
            }
        };
        ensure!(
            status.success(),
            "macOS Keychain lookup failed; check that the keychain is unlocked and access is allowed"
        );
        serde_json::from_slice(&out)
            .map_err(|_| anyhow::anyhow!("Invalid Keychain lookup response"))
    }

    pub async fn cancel(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            self.child
                .start_kill()
                .context("Terminate Keychain lookup")?;
        }
        self.child.wait().await.context("Reap Keychain lookup")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn helper_protocol_preserves_empty_unicode_and_trailing_newlines() {
        for password in [None, Some(""), Some("日本語\n\n")] {
            let bytes = serde_json::to_string(&password).unwrap();
            let mut command = tokio::process::Command::new("/usr/bin/printf");
            command.args(["%s", &bytes]);
            let mut lookup = CredentialLookup::spawn(command).unwrap();
            let decoded = lookup.get().await.unwrap();
            assert!(decoded.as_deref() == password);
            lookup.cancel().await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancellation_terminates_and_reaps_only_the_owned_helper() {
        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("30");
        let mut lookup = CredentialLookup::spawn(command).unwrap();
        let pid = lookup.child.id().unwrap() as i32;
        tokio::time::timeout(std::time::Duration::from_secs(2), lookup.cancel())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
