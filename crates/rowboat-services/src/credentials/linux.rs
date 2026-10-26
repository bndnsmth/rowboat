use super::{validate_id, validate_password};
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::Write,
    process::{Command, Stdio},
};

// Deliberately stable: existing remembered credentials remain available to both
// releases. Rebranding never enumerates, copies, or migrates keyring entries.
const CREDENTIAL_APPLICATION: &str = "keel";

fn command(operation: &str, id: &str) -> Result<Command> {
    validate_id(id)?;
    let mut command = Command::new("secret-tool");
    command.arg(operation);
    if operation == "store" {
        command.arg("--label=Rowboat database connection");
    }
    command.args([
        "--",
        "application",
        CREDENTIAL_APPLICATION,
        "connection-id",
        id,
    ]);
    command.stderr(Stdio::piped());
    Ok(command)
}

pub fn credential_get(connection_id: &str) -> Result<Option<String>> {
    let output = command("lookup", connection_id)?.stdin(Stdio::null()).output()
        .context("Cannot run secret-tool; provide a session password or enable a Secret Service keyring with secret-tool")?;
    decode_lookup(output)
}

fn decode_lookup(output: std::process::Output) -> Result<Option<String>> {
    if !output.status.success() {
        if output.status.code() == Some(1) && output.stderr.is_empty() && output.stdout.is_empty() {
            return Ok(None);
        }
        bail!(
            "Keyring lookup failed; check that your Secret Service keyring is running and unlocked"
        );
    }
    let password = String::from_utf8(output.stdout)
        .map_err(|_| anyhow::anyhow!("Keyring credential is not UTF-8"))?;
    // secret-tool adds a newline only for a TTY. Piped stdout is exact bytes.
    Ok(Some(password))
}

/// An owned asynchronous lookup. Keep this handle outside a cancellable future
/// and call `cancel().await` when abandoning `get()` to confirm helper reaping.
/// Drop requests termination as a fallback, but is not a cleanup acknowledgment.
pub struct CredentialLookup {
    child: tokio::process::Child,
}

impl CredentialLookup {
    pub fn start(connection_id: &str) -> Result<Self> {
        let mut command = tokio::process::Command::from(command("lookup", connection_id)?);
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Cannot run secret-tool; provide a session password or enable a Secret Service keyring with secret-tool")?;
        Ok(Self { child })
    }

    /// Read the exact credential bytes once, concurrently draining both pipes.
    /// A successful return also confirms the helper was reaped. Cancellation of
    /// this future leaves the child owned by this handle for explicit cleanup.
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
                return Err(error).context("Read keyring lookup response");
            }
        };
        decode_lookup(std::process::Output {
            status,
            stdout: out,
            stderr: err,
        })
    }

    /// Terminate only this lookup helper and explicitly await its exit/reaping.
    /// Callers may bound this await; dropping the handle retains kill-on-drop.
    pub async fn cancel(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            self.child
                .start_kill()
                .context("Terminate keyring lookup")?;
        }
        self.child.wait().await.context("Reap keyring lookup")?;
        Ok(())
    }
}

pub fn credential_set(connection_id: &str, password: &str) -> Result<()> {
    validate_password(password)?;
    let mut child = command("store", connection_id)?.stdin(Stdio::piped()).stdout(Stdio::null()).spawn()
        .context("Cannot run secret-tool; provide a session password or enable a Secret Service keyring with secret-tool")?;
    let write_result = child
        .stdin
        .take()
        .context("Keyring stdin unavailable")?
        .write_all(password.as_bytes());
    let output = child.wait_with_output()?;
    write_result.context("Write credential to keyring")?;
    ensure!(
        output.status.success(),
        "Keyring storage failed; check that your Secret Service keyring is running and unlocked"
    );
    Ok(())
}

pub fn credential_delete(connection_id: &str) -> Result<()> {
    let output = command("clear", connection_id)?
        .stdin(Stdio::null())
        .output()
        .context("Cannot run secret-tool; it is required to remove a saved keyring credential")?;
    ensure!(
        output.status.success() || (output.status.code() == Some(1) && output.stderr.is_empty()),
        "Keyring deletion failed; check that your Secret Service keyring is running and unlocked"
    );
    Ok(())
}
