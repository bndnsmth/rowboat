use crate::providers::{NetworkProvider, ServiceOperation};
use anyhow::{Context, Result, bail, ensure};
use rowboat_core::{ConnectionKind, ConnectionProfile, ConnectionTransport};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    time::{Instant, sleep, timeout},
};

/// Owns an SSH master process. Dropping the guard terminates the tunnel.
pub struct Tunnel {
    child: Option<Child>,
    _directory: Option<tempfile::TempDir>,
}

#[derive(Debug, Clone, Default)]
pub struct TunnelOptions {
    /// Read-only trust file; disables all other known-hosts files when provided.
    pub known_hosts_file: Option<PathBuf>,
}

impl Tunnel {
    pub async fn open(profile: &ConnectionProfile) -> Result<(Self, ConnectionProfile)> {
        Self::open_with_options(profile, &TunnelOptions::default()).await
    }

    pub async fn open_with_options(
        profile: &ConnectionProfile,
        options: &TunnelOptions,
    ) -> Result<(Self, ConnectionProfile)> {
        if profile.engine.descriptor().connection_kind == ConnectionKind::File {
            if profile.ssh.is_some() || profile.transport.is_some() {
                crate::service_support(profile.engine, ServiceOperation::SshTunnel)?;
            }
            crate::validate_profile(profile)?;
        }
        let Some(ssh) = &profile.ssh else {
            return Ok((
                Self {
                    child: None,
                    _directory: None,
                },
                profile.clone(),
            ));
        };
        let provider = NetworkProvider::for_engine(profile.engine, ServiceOperation::SshTunnel)?;
        ensure!(
            profile.transport.is_none(),
            "Cannot nest SSH over an existing tunnel transport"
        );
        valid_host(&ssh.host)?;
        valid_host(&profile.host)?;
        ensure!(
            ssh.port > 0 && profile.port > 0,
            "SSH and database ports must be nonzero"
        );
        ensure!(
            !ssh.username.is_empty() && !ssh.username.chars().any(char::is_control),
            "Invalid SSH username"
        );
        let directory = tempfile::Builder::new().prefix("rowboat-ssh-").tempdir()?;
        let socket = directory.path().join("control");
        let database_socket = directory.path().join(provider.socket_name(profile.port));
        // A private file avoids pipe backpressure and is deleted with the guard.
        let errors = tempfile::tempfile()?;
        let mut command = Command::new("ssh");
        if let Some(path) = &options.known_hosts_file {
            let path =
                std::fs::canonicalize(path).context("Open configured SSH known-hosts file")?;
            ensure!(
                path.is_file(),
                "SSH known-hosts path must be a regular file"
            );
            let path = path
                .to_str()
                .context("SSH known-hosts path must be UTF-8")?;
            ensure!(
                !path.chars().any(char::is_control),
                "Invalid SSH known-hosts path"
            );
            command
                .arg("-o")
                .arg(format!(
                    "UserKnownHostsFile=\"{}\"",
                    path.replace('\\', "\\\\").replace('"', "\\\"")
                ))
                .args(["-o", "GlobalKnownHostsFile=/dev/null"]);
        }
        if let Some(identity) = &ssh.identity_file {
            ensure!(
                std::fs::metadata(identity)
                    .context("Configured SSH identity is unavailable")?
                    .is_file(),
                "SSH identity must be a regular file"
            );
            command.args(["-i", identity, "-o", "IdentitiesOnly=yes"]);
        }
        let mut child = command
            .args(["-F", "/dev/null", "-M", "-N", "-T", "-S"]).arg(&socket)
            .args(["-o", "StrictHostKeyChecking=yes", "-o", "UpdateHostKeys=no",
                "-o", "BatchMode=yes", "-o", "ExitOnForwardFailure=yes",
                "-o", "ForwardAgent=no", "-o", "PermitLocalCommand=no",
                "-o", "ControlPersist=no", "-o", "ConnectTimeout=10",
                "-o", "StreamLocalBindUnlink=no", "-o", "StreamLocalBindMask=0177",
                "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"])
            .args(["-p", &ssh.port.to_string(), "-l", &ssh.username])
            .arg(&ssh.host)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::from(errors.try_clone()?))
            .kill_on_drop(true).spawn()
            .context("Cannot start ssh; an existing OpenSSH client and trusted known-hosts entry are required")?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait()? {
                bail!(
                    "SSH exited ({status}); verify the existing known-hosts entry, username, and agent/identity. {}",
                    read_errors(&errors)
                );
            }
            ensure!(
                Instant::now() < deadline,
                "SSH connection timed out; verify host trust and agent/identity"
            );
            if socket.exists() && control(&socket, &ssh.host, "check").await? {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        // A private Unix socket avoids TCP port reservation races. SQLx uses
        // its socket option for transport and the original host for TLS/SNI.
        let destination = if profile.host.contains(':') {
            format!("[{}]", profile.host)
        } else {
            profile.host.clone()
        };
        let forward = format!(
            "{}:{destination}:{}",
            database_socket.display(),
            profile.port
        );
        let output = timeout(
            Duration::from_secs(5),
            Command::new("ssh")
                .args(["-F", "/dev/null", "-S"])
                .arg(&socket)
                .args([
                    "-O",
                    "forward",
                    "-o",
                    "ExitOnForwardFailure=yes",
                    "-L",
                    &forward,
                    &ssh.host,
                ])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("SSH forwarding request timed out")??;
        if output.status.success() {
            ensure!(
                child.try_wait()?.is_none(),
                "SSH disconnected while establishing forwarding"
            );
            let mut forwarded = profile.clone();
            forwarded.ssh = None;
            forwarded.transport = Some(ConnectionTransport::UnixSocket {
                path: database_socket,
            });
            return Ok((
                Self {
                    child: Some(child),
                    _directory: Some(directory),
                },
                forwarded,
            ));
        }
        bail!("SSH could not establish its private database forwarding socket")
    }

    /// Terminate and reap SSH, then remove its private socket directory.
    pub async fn close(mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            child.kill().await.context("Terminate SSH tunnel")?;
        }
        Ok(())
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

async fn control(socket: &Path, host: &str, operation: &str) -> Result<bool> {
    Ok(timeout(
        Duration::from_secs(2),
        Command::new("ssh")
            .args(["-F", "/dev/null", "-S"])
            .arg(socket)
            .args(["-O", operation, host])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .context("SSH control check timed out")??
    .success())
}

fn valid_host(host: &str) -> Result<()> {
    ensure!(
        !host.is_empty()
            && !host.starts_with('-')
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-:_%".contains(c)),
        "Invalid SSH/database host"
    );
    Ok(())
}

fn read_errors(file: &std::fs::File) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = file.try_clone() else {
        return String::new();
    };
    let _ = file.seek(SeekFrom::Start(0));
    let mut text = String::new();
    let _ = file.take(4096).read_to_string(&mut text);
    text.trim().to_owned()
}
