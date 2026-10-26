use crate::providers::{NetworkProvider, ServiceOperation};
use crate::relay::TcpRelay;
use anyhow::{Context, Result, ensure};
use rowboat_core::{ConnectionProfile, ConnectionTransport, ToolProvider, ToolSupport};
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;

#[derive(Debug, Clone, Default)]
pub enum ClientRuntime {
    #[default]
    Native,
    /// An already-present image containing the selected database's client tools.
    Podman { image: String },
}

pub async fn backup(
    profile: &ConnectionProfile,
    password: &str,
    destination: &Path,
    runtime: &ClientRuntime,
) -> Result<()> {
    crate::providers::service_support(profile.engine, ServiceOperation::Backup)?;
    if profile.engine.descriptor().tools.backup == ToolSupport::Supported(ToolProvider::Sqlite) {
        return crate::sqlite_backup::backup(profile, destination, runtime).await;
    }
    let provider = NetworkProvider::for_engine(profile.engine, ServiceOperation::Backup)?;
    ensure!(
        !destination.exists(),
        "Backup destination already exists; choose a new path"
    );
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let output = tempfile::NamedTempFile::new_in(parent).context("Create private backup file")?;
    output
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    let setup = ClientSetup::new(profile, password, runtime).await?;
    let tool = match provider {
        NetworkProvider::Postgres => "pg_dump",
        NetworkProvider::MySql => "mysqldump",
    };
    let mut command = setup.command(tool, runtime)?;
    match provider {
        NetworkProvider::Postgres => {
            command.args(["--format=plain", "--no-password"]);
        }
        NetworkProvider::MySql => {
            command.args([
                "--single-transaction",
                "--quick",
                "--hex-blob",
                "--",
                &profile.database,
            ]);
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.as_file().try_clone()?));
    run(command, tool, password, &setup).await?;
    output.as_file().sync_all()?;
    output
        .persist_noclobber(destination)
        .context("Publish backup (destination must not exist)")?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub async fn restore(
    profile: &ConnectionProfile,
    password: &str,
    source: &Path,
    runtime: &ClientRuntime,
) -> Result<()> {
    crate::providers::service_support(profile.engine, ServiceOperation::Restore)?;
    if profile.engine.descriptor().tools.backup == ToolSupport::Supported(ToolProvider::Sqlite) {
        return crate::sqlite_backup::restore(profile, source, runtime).await;
    }
    let provider = NetworkProvider::for_engine(profile.engine, ServiceOperation::Restore)?;
    ensure!(
        !profile.read_only,
        "Restore is disabled for read-only connections"
    );
    let input = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)
        .context("Open SQL backup")?;
    ensure!(
        input.metadata()?.is_file(),
        "SQL backup must be a regular file"
    );
    let setup = ClientSetup::new(profile, password, runtime).await?;
    let tool = match provider {
        NetworkProvider::Postgres => "psql",
        NetworkProvider::MySql => "mysql",
    };
    let mut command = setup.command(tool, runtime)?;
    match provider {
        NetworkProvider::Postgres => {
            command.args([
                "-X",
                "--no-password",
                "--set=ON_ERROR_STOP=1",
                "--single-transaction",
            ]);
        }
        NetworkProvider::MySql => {
            command.args([
                "--batch",
                "--binary-mode",
                &format!("--database={}", profile.database),
            ]);
        }
    }
    command.stdin(Stdio::from(input)).stdout(Stdio::null());
    run(command, tool, password, &setup).await
}

struct ClientSetup {
    // Drop aborts the accept task and every active relay connection.
    _relay: Option<TcpRelay>,
    directory: tempfile::TempDir,
    environment: Vec<(String, String)>,
    arguments: Vec<String>,
    hostname_override: Option<String>,
    container_name: Option<String>,
    connection_id: String,
}

impl ClientSetup {
    async fn new(
        profile: &ConnectionProfile,
        password: &str,
        runtime: &ClientRuntime,
    ) -> Result<Self> {
        let provider = NetworkProvider::for_engine(profile.engine, ServiceOperation::Backup)?;
        ensure!(
            profile.transport.is_none() || matches!(runtime, ClientRuntime::Podman { .. }),
            "Native backup/restore cannot preserve TLS hostname verification over an SSH socket transport; select Podman or use a direct connection"
        );
        ensure!(
            profile.ssh.is_none(),
            "Open an SSH Tunnel first and pass its forwarded connection profile to backup/restore"
        );
        ensure!(
            !profile.host.is_empty() && profile.port > 0 && !profile.database.is_empty(),
            "Backup/restore requires a host, nonzero port, and database"
        );
        ensure!(
            !profile.database.contains('=') && !profile.database.contains("://"),
            "Backup/restore requires a database name, not a connection string"
        );
        for value in [
            &profile.host,
            &profile.database,
            &profile.username,
            password,
        ] {
            ensure!(
                !value.contains(['\0', '\n', '\r']),
                "Native client connection fields/password cannot contain NUL or newlines"
            );
        }
        let mut effective = profile.clone();
        let mut relay = None;
        let mut hostname_override = None;
        if let Some(ConnectionTransport::UnixSocket { path }) = &profile.transport {
            crate::validate_profile(profile)?;
            ensure!(
                profile.host.parse::<std::net::IpAddr>().is_err(),
                "SSH Podman backup/restore requires a DNS hostname; an IP-literal TLS identity cannot be redirected by a container hosts mapping"
            );
            let host = match url::Host::parse(&profile.host)
                .context("Invalid SSH database hostname")?
            {
                url::Host::Domain(host) => host,
                _ => anyhow::bail!(
                    "SSH Podman backup/restore requires a DNS hostname; an IP-literal TLS identity cannot be redirected by a container hosts mapping"
                ),
            };
            let bound = TcpRelay::bind(path).await?;
            effective.port = bound.port();
            effective.host = host.clone();
            hostname_override = Some(host);
            relay = Some(bound);
        }
        let profile = &effective;
        let directory = tempfile::Builder::new()
            .prefix("rowboat-client-")
            .tempdir()?;
        let root = match runtime {
            ClientRuntime::Native => directory.path().to_path_buf(),
            ClientRuntime::Podman { .. } => PathBuf::from("/rowboat-private"),
        };
        let mut environment = Vec::new();
        let mut arguments = Vec::new();
        let mut certificates = Vec::new();
        for (name, path) in [
            ("ca", &profile.ssl_ca),
            ("cert", &profile.ssl_cert),
            ("key", &profile.ssl_key),
        ] {
            if let Some(path) = path.as_ref().filter(|p| !p.is_empty()) {
                let bytes =
                    fs::read(path).with_context(|| format!("Read configured TLS {name} file"))?;
                private_write(&directory.path().join(name), &bytes)?;
                certificates.push((name, root.join(name).to_string_lossy().into_owned()));
            }
        }
        match provider {
            NetworkProvider::Postgres => {
                ensure!(
                    [
                        "disable",
                        "allow",
                        "prefer",
                        "require",
                        "verify-ca",
                        "verify-full"
                    ]
                    .contains(&profile.ssl_mode.as_str()),
                    "Unsupported PostgreSQL TLS mode"
                );
                let escape = |s: &str| {
                    s.replace('\\', "\\\\")
                        .replace(':', "\\:")
                        .replace('*', "\\*")
                };
                let pass = format!(
                    "{}:{}:{}:{}:{}\n",
                    escape(&profile.host),
                    profile.port,
                    escape(&profile.database),
                    escape(&profile.username),
                    escape(password)
                );
                private_write(&directory.path().join("pgpass"), pass.as_bytes())?;
                environment.extend([
                    ("PGHOST".into(), profile.host.clone()),
                    ("PGPORT".into(), profile.port.to_string()),
                    ("PGDATABASE".into(), profile.database.clone()),
                    ("PGUSER".into(), profile.username.clone()),
                    ("PGSSLMODE".into(), profile.ssl_mode.clone()),
                    ("PGCONNECT_TIMEOUT".into(), "15".into()),
                    (
                        "PGPASSFILE".into(),
                        root.join("pgpass").to_string_lossy().into_owned(),
                    ),
                    ("PGAPPNAME".into(), "Rowboat backup/restore".into()),
                ]);
                for (name, path) in certificates {
                    let key = match name {
                        "ca" => "PGSSLROOTCERT",
                        "cert" => "PGSSLCERT",
                        _ => "PGSSLKEY",
                    };
                    environment.push((key.into(), path));
                }
            }
            NetworkProvider::MySql => {
                let mode = match profile.ssl_mode.as_str() {
                    "disable" => "DISABLED",
                    "prefer" => "PREFERRED",
                    "require" => "REQUIRED",
                    "verify-ca" => "VERIFY_CA",
                    "verify-full" => "VERIFY_IDENTITY",
                    _ => anyhow::bail!("Unsupported MySQL TLS mode"),
                };
                let mut config = format!(
                    "[client]\nhost={}\nport={}\nuser={}\npassword={}\nprotocol=TCP\nssl-mode={mode}\nget-server-public-key=1\n",
                    mysql_quote(&profile.host),
                    profile.port,
                    mysql_quote(&profile.username),
                    mysql_quote(password)
                );
                for (name, path) in certificates {
                    config.push_str(&format!("ssl-{name}={}\n", mysql_quote(&path)));
                }
                // mysqldump does not accept mysql's connect-timeout option.
                config.push_str("[mysql]\nconnect-timeout=15\n");
                private_write(&directory.path().join("mysql.cnf"), config.as_bytes())?;
                arguments.push(format!(
                    "--defaults-file={}",
                    root.join("mysql.cnf").display()
                ));
                arguments.push("--no-login-paths".into());
            }
        }
        Ok(Self {
            _relay: relay,
            directory,
            environment,
            arguments,
            hostname_override,
            container_name: matches!(runtime, ClientRuntime::Podman { .. })
                .then(|| format!("rowboat-client-{}", uuid::Uuid::new_v4())),
            connection_id: profile.id.clone(),
        })
    }

    fn command(&self, tool: &str, runtime: &ClientRuntime) -> Result<Command> {
        let mut command = match runtime {
            ClientRuntime::Native => {
                let mut command = Command::new(tool);
                // Do not inherit connection overrides or SQL startup options.
                for (key, _) in std::env::vars_os() {
                    let name = key.to_string_lossy();
                    if name.starts_with("PG") || name.starts_with("MYSQL") {
                        command.env_remove(key);
                    }
                }
                command.envs(self.environment.iter().cloned());
                command
            }
            ClientRuntime::Podman { image } => {
                ensure!(
                    !image.is_empty()
                        && !image.starts_with('-')
                        && !image.chars().any(char::is_whitespace),
                    "Provide an existing Podman client image name"
                );
                let mut command = Command::new("podman");
                command.args([
                    "run",
                    "--rm",
                    "--interactive",
                    "--pull=never",
                    "--network=host",
                    "--read-only",
                    "--cap-drop=ALL",
                    "--security-opt=no-new-privileges",
                ]);
                command.arg("--name").arg(
                    self.container_name
                        .as_deref()
                        .expect("Podman setup has a name"),
                );
                command
                    .args(["--label", "io.rowboat.client=true", "--label"])
                    .arg(format!("io.rowboat.connection-id={}", self.connection_id));
                if let Some(host) = &self.hostname_override {
                    // This modifies only Podman's private per-container hosts
                    // file. Native TLS still receives the original peer name.
                    command.arg("--add-host").arg(format!("{host}:127.0.0.1"));
                }
                command.arg("--volume").arg(format!(
                    "{}:/rowboat-private:ro",
                    self.directory.path().display()
                ));
                for (key, value) in &self.environment {
                    command.arg("--env").arg(format!("{key}={value}"));
                }
                command.args(["--entrypoint", tool, image]);
                command
            }
        };
        command.args(&self.arguments).kill_on_drop(true);
        Ok(command)
    }
}

fn mysql_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\t', "\\t")
    )
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

async fn run(mut command: Command, tool: &str, password: &str, setup: &ClientSetup) -> Result<()> {
    // Keep diagnostics private and off unbounded pipes; query output is never
    // included in application errors. This file is deleted on return/cancel.
    let errors = fs::OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .mode(0o600)
        .open(setup.directory.path().join("errors"))?;
    command.stderr(Stdio::from(errors.try_clone()?));
    let child = command.spawn().with_context(|| format!("Cannot run {tool}; select a preloaded Podman client image or make the native client available (Rowboat never installs tools)"))?;
    let mut process = RunningClient {
        child,
        container_name: setup.container_name.clone(),
    };
    let status = process.child.wait().await?;
    process.cleanup_container()?;
    if !status.success() {
        use std::io::{Read, Seek, SeekFrom};
        let mut errors = errors;
        errors.seek(SeekFrom::Start(0))?;
        let mut detail = String::new();
        errors.take(8192).read_to_string(&mut detail)?;
        if !password.is_empty() {
            detail = detail.replace(password, "[redacted]");
        }
        anyhow::bail!("{tool} failed ({status}): {}", detail.trim());
    }
    Ok(())
}

/// Killing `podman run` alone does not reliably kill the container. This guard
/// owns only its UUID-named container and removes it even if the operation's
/// future is dropped or its Tokio task is aborted. Synchronous Drop cleanup is
/// deliberate: it also works while a Tokio runtime is shutting down.
struct RunningClient {
    child: tokio::process::Child,
    container_name: Option<String>,
}

impl RunningClient {
    fn cleanup_container(&mut self) -> Result<()> {
        if let Some(name) = &self.container_name {
            let status = std::process::Command::new("podman")
                .args(["rm", "--force", "--ignore", "--time=0", name])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("Remove operation-owned Podman client")?;
            ensure!(
                status.success(),
                "Could not remove operation-owned Podman client container"
            );
            self.container_name = None;
        }
        Ok(())
    }
}

impl Drop for RunningClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        // Reap the local podman launcher before removing its container, so a
        // cancellation during startup cannot race a still-running `podman run`.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while matches!(self.child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let _ = self.cleanup_container();
    }
}
