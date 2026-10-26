//! Query-only deadlines, operation-scoped cancellation and interruptible output.
use crate::{Cli, Command, Format, response::Response};
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use rowboat_core::{
    ConnectionKind, ConnectionProfile, Database, QueryCancellation, QueryEvent, QueryStream,
    QueryStreamOptions,
};
use rowboat_services::{CredentialLookup, Store, Tunnel};
use std::{future::Future, io, sync::Arc, time::Duration};
use tokio::{
    signal::unix::{Signal, SignalKind, signal},
    time::Instant,
};

const CLEANUP: Duration = Duration::from_secs(2);
const FINAL_OUTPUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, ValueEnum)]
pub enum QueryFormat {
    Table,
    Csv,
    Json,
    Sql,
    JsonEnvelope,
}

#[derive(Clone, Copy, Args)]
pub struct Timing {
    /// Execution and output deadline (integer ms, s, m; 0 disables). Excludes connection setup.
    #[arg(long, default_value = "0", value_parser = duration)]
    pub timeout: Duration,
    /// Connection/setup deadline (integer ms, s, m; 0 disables).
    #[arg(long, default_value = "10s", value_parser = duration)]
    pub connect_timeout: Duration,
}

fn duration(text: &str) -> std::result::Result<Duration, String> {
    if text == "0" {
        return Ok(Duration::ZERO);
    }
    let (digits, multiplier) = if let Some(n) = text.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = text.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = text.strip_suffix('m') {
        (n, 60_000)
    } else {
        return Err("use an integer with ms, s or m, or 0 to disable".into());
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err("duration must be an unsigned integer with ms, s or m".into());
    }
    let millis = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .filter(|n| *n <= 7 * 24 * 60 * 60 * 1000)
        .ok_or_else(|| "duration must be at most 7 days (overflow is rejected)".to_string())?;
    Ok(Duration::from_millis(millis))
}

#[derive(Clone)]
pub struct Failure {
    pub status: &'static str,
    pub code: &'static str,
    pub message: String,
    exit: i32,
}
impl Failure {
    fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: "error",
            code,
            message: message.into(),
            exit: 1,
        }
    }
    fn timeout(connecting: bool) -> Self {
        Self {
            status: "timeout",
            code: "timeout",
            message: if connecting {
                "Connection setup timed out"
            } else {
                "Query deadline exceeded; earlier statements may already have committed"
            }
            .into(),
            exit: 124,
        }
    }
    fn cancelled(terminate: bool) -> Self {
        Self {
            status: "cancelled",
            code: "cancelled",
            message: format!(
                "Query cancelled by {}; earlier statements may already have committed",
                if terminate { "SIGTERM" } else { "SIGINT" }
            ),
            exit: if terminate { 143 } else { 130 },
        }
    }
}

struct Control {
    interrupt: Signal,
    terminate: Signal,
}
impl Control {
    // signal() installs the OS handlers synchronously, before reading files,
    // credentials, opening a tunnel or starting a database handshake.
    fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }
    async fn wait<T>(
        &mut self,
        deadline: Option<Instant>,
        connecting: bool,
        future: impl Future<Output = T>,
    ) -> std::result::Result<T, Failure> {
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                _ = self.interrupt.recv() => return Err(Failure::cancelled(false)),
                _ = self.terminate.recv() => return Err(Failure::cancelled(true)),
                _ = sleep_until(deadline) => return Err(Failure::timeout(connecting)),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {
                    if stdout_closed() { return Err(Failure::error("output_error", "Standard output closed; query cancelled")); }
                }
                value = &mut future => return Ok(value),
            }
        }
    }
}
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
fn deadline(duration: Duration) -> Option<Instant> {
    (!duration.is_zero()).then(|| Instant::now() + duration)
}

// Tokio stdout uses a blocking writer which cannot be stopped when its pipe
// fills (and can hang runtime shutdown). Use nonblocking OS writes instead:
// one bounded write at a time, no worker thread, queue, or stdout mutex.
struct Output {
    flags: libc::c_int,
    partial: std::cell::Cell<bool>,
    ready: Option<tokio::io::unix::AsyncFd<libc::c_int>>,
}
impl Output {
    fn new() -> io::Result<Self> {
        let flags = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            flags,
            partial: std::cell::Cell::new(false),
            // epoll cannot register regular files, which have no pipe backpressure.
            // SAFETY: stdout was verified open above and is never closed or
            // replaced during query execution; registration drops before exit.
            ready: unsafe { tokio::io::unix::AsyncFd::register(libc::STDOUT_FILENO) }.ok(),
        })
    }
    async fn write(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let count = unsafe {
                libc::write(
                    libc::STDOUT_FILENO,
                    bytes.as_ptr().cast(),
                    bytes.len().min(16 * 1024),
                )
            };
            if count > 0 {
                self.partial.set(true);
                bytes = &bytes[count as usize..];
            } else if count == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            } else {
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::WouldBlock => match &self.ready {
                        Some(ready) => ready.writable().await?.clear_ready(),
                        None => tokio::time::sleep(Duration::from_millis(5)).await,
                    },
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(error),
                }
            }
            // Large output to a ready sink must also allow signal/deadline polls.
            if !bytes.is_empty() {
                tokio::task::yield_now().await;
            }
        }
        self.partial.set(false);
        Ok(())
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        unsafe {
            libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, self.flags);
        }
    }
}
fn stdout_closed() -> bool {
    let mut fd = libc::pollfd {
        fd: libc::STDOUT_FILENO,
        events: libc::POLLOUT,
        revents: 0,
    };
    unsafe {
        libc::poll(&mut fd, 1, 0) > 0
            && fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
    }
}

struct Connected {
    db: Database,
    profile: ConnectionProfile,
    _tunnel: Option<Tunnel>,
    secrets: Vec<String>,
}
async fn connect(cli: Cli, lookup: &mut Option<CredentialLookup>) -> Result<Connected> {
    let (profile, password, known_hosts, mut secrets) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let keyring = cli.connection.is_some();
            let secrets = cli.url.iter().cloned().collect::<Vec<_>>();
            let (mut profile, url_password) = if let Some(path) = cli.database_file {
                (
                    crate::profiles::file_profile(path, cli.create, cli.read_only)?,
                    None,
                )
            } else if let Some(url) = cli.url {
                crate::parse_url(&url)?
            } else if let Some(name) = cli.connection {
                (
                    crate::profiles::select(&Store::open()?.load()?.connections, &name)?.clone(),
                    None,
                )
            } else {
                bail!("Provide --connection, --url (or ROWBOAT_URL), or --database-file");
            };
            profile.read_only |= cli.read_only;
            let password = if profile.engine.descriptor().connection_kind == ConnectionKind::File {
                Some(String::new())
            } else if let Some(password) = rowboat_services::environment_text("ROWBOAT_PASSWORD")? {
                Some(password)
            } else if let Some(password) = url_password {
                Some(password)
            } else if keyring {
                None
            } else {
                Some(String::new())
            };
            Ok((profile, password, cli.ssh_known_hosts, secrets))
        })
        .await??;
    let password = match password {
        Some(password) => password,
        None => {
            // The caller owns the child outside this cancellable setup future.
            // Dropping get() cannot discard the handle before kill + wait.
            *lookup = Some(CredentialLookup::start(&profile.id)?);
            let password = lookup.as_mut().expect("owned lookup").get().await?;
            lookup.take(); // get() has already reaped the helper.
            password.unwrap_or_default()
        }
    };
    if !password.is_empty() {
        secrets.push(password.clone());
    }
    let (tunnel, profile) =
        if profile.engine.descriptor().connection_kind == ConnectionKind::Network {
            let (tunnel, profile) = Tunnel::open_with_options(
                &profile,
                &rowboat_services::TunnelOptions {
                    known_hosts_file: known_hosts,
                },
            )
            .await?;
            (Some(tunnel), profile)
        } else {
            anyhow::ensure!(
                known_hosts.is_none(),
                "File connections do not support --ssh-known-hosts"
            );
            rowboat_services::validate_profile(&profile)?;
            (None, profile)
        };
    let db = Database::connect(&profile, &password).await?;
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    Ok(Connected {
        db,
        profile,
        _tunnel: tunnel,
        secrets,
    })
}

fn redact(mut message: String, secrets: &[String]) -> String {
    for secret in secrets {
        message = message.replace(secret, "[redacted]");
    }
    // Driver messages can contain URLs supplied by SQL itself, too. Omit URI
    // tokens rather than trying to recognize every possible credential encoding.
    message
        .split_whitespace()
        .map(|word| {
            if word.contains("://") {
                "[redacted URL]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn run(mut cli: Cli) -> i32 {
    let started = Instant::now();
    let mut control = match Control::new() {
        Ok(control) => control,
        Err(_) => {
            eprintln!("rowboatctl: could not install signal handlers");
            return 1;
        }
    };
    let command = std::mem::replace(&mut cli.command, Command::Ping);
    let (sql, file, options, format, timing) = match command {
        Command::Query {
            sql,
            file,
            limit,
            format,
            timing,
        } => (
            sql,
            file,
            QueryStreamOptions {
                cancellation: QueryCancellation::RetireSession,
                // Match the legacy buffered query's absolute row/cell bounds.
                row_limit: Some(limit.min(100_000)),
                batch_bytes: 32 * 1024 * 1024,
                max_cell_bytes: 16 * 1024 * 1024,
                ..Default::default()
            },
            Some(format),
            timing,
        ),
        Command::QueryStream(args) => (
            args.sql,
            args.file,
            QueryStreamOptions {
                cancellation: QueryCancellation::RetireSession,
                row_limit: (!args.unlimited).then_some(args.limit),
                byte_limit: if args.unlimited {
                    None
                } else {
                    QueryStreamOptions::default().byte_limit
                },
                batch_rows: args.batch_rows as usize,
                ..Default::default()
            },
            None,
            args.timing,
        ),
        _ => unreachable!(),
    };
    let envelope = matches!(format, Some(QueryFormat::JsonEnvelope));
    let output = match Output::new() {
        Ok(output) => output,
        Err(_) => {
            eprintln!("rowboatctl: standard output unavailable");
            return 1;
        }
    };
    let mut response = Arc::new(Response::default());
    let setup_deadline = deadline(timing.connect_timeout);
    let mut execution_deadline = None;
    let mut secrets = Vec::new();
    let result: std::result::Result<(), Failure> = async {
        if matches!(format, Some(QueryFormat::Sql)) {
            return Err(Failure::error("input_error", "INSERT SQL output requires a table: use export --format sql"));
        }
        let sql = match sql {
            Some(sql) => sql,
            None => control.wait(setup_deadline, true, tokio::fs::read_to_string(file.expect("clap SQL input"))).await?
                .map_err(|_| Failure::error("input_error", "Could not read SQL file as UTF-8"))?,
        };
        let mut lookup = None;
        let connection = control.wait(setup_deadline, true, connect(cli, &mut lookup)).await;
        if let Some(mut lookup) = lookup {
            // Timeout/signal drops the setup future, not the owned helper. Reap
            // explicitly before publishing its terminal response or exiting.
            let _ = tokio::time::timeout(CLEANUP, lookup.cancel()).await;
        }
        let connected = connection?
            // Setup errors may echo credentials from external providers. Keep the
            // public diagnostic deliberately credential-free, including stderr.
            .map_err(|_| Failure::error("connection_error", "Connection setup failed; check connection selection, credentials, transport and database availability"))?;
        secrets = connected.secrets.clone();
        execution_deadline = deadline(timing.timeout);
        let mut stream = control.wait(execution_deadline, false, connected.db.query_stream(&sql, options)).await?
            .map_err(|error| Failure::error("query_error", redact(error.to_string(), &secrets)))?;
        let outcome = drain(&mut stream, Arc::get_mut(&mut response).expect("sole collector"), format.is_none(), &output, &mut control, execution_deadline, &secrets).await;
        if outcome.is_err() {
            cleanup(stream, Arc::get_mut(&mut response).expect("sole collector"), format.is_some()).await;
            return outcome;
        }
        // The terminal event already proves worker cleanup; close never cancels
        // another operation and does not claim transaction commit.
        let _ = tokio::time::timeout(CLEANUP, stream.close()).await;
        if let Some(format) = format {
            let retained = response.clone();
            let engine = connected.profile.engine;
            let elapsed = started.elapsed().as_millis() as u64;
            // Serialization of a bounded but large result must not keep this
            // task from polling its deadline/signals. There is at most one normal
            // renderer and one terminal renderer, never a queued writer thread.
            let bytes = control.wait(execution_deadline, false, tokio::task::spawn_blocking(move || render(&retained, format, engine, elapsed))).await?
                .map_err(|_| Failure::error("output_error", "Response encoder stopped"))?
                .map_err(|_| Failure::error("output_error", "Could not encode query response"))?;
            control.wait(execution_deadline, false, output.write(&bytes)).await?
                .map_err(|_| Failure::error("output_error", "Standard output closed; query cancelled"))?;
            if !envelope {
                for result in &response.results {
                    let result = &result.data;
                    eprintln!("{} · {} ms{}",
                        if result.columns.is_empty() { format!("{} affected", result.rows_affected) }
                        else { format!("{} rows", result.rows.len()) },
                        result.elapsed_ms,
                        if result.truncated { " · truncated" } else { "" });
                }
            }
        }
        Ok(())
    }.await;
    match result {
        Ok(()) => 0,
        Err(mut failure) => {
            failure.message = redact(failure.message, &secrets);
            // A blocked/closed consumer cannot receive a terminal record. Best
            // effort is strictly bounded, and the nonzero exit remains decisive.
            // Never append a second JSON document to an interrupted document or
            // a terminal event to an interrupted JSONL line. The consumer must
            // reject incomplete output and inspect the nonzero exit status.
            if (envelope || format.is_none()) && !output.partial.get() {
                let report = failure.clone();
                let elapsed = started.elapsed().as_millis() as u64;
                let _ = tokio::time::timeout(FINAL_OUTPUT, async {
                    let bytes = tokio::task::spawn_blocking(move || {
                        let value = if envelope { response.envelope(elapsed, Some(&report)) }
                            else { serde_json::json!({"event":"error", "message":report.message, "cancelled":report.status == "cancelled" || report.status == "timeout", "code":report.code}) };
                        let mut bytes = serde_json::to_vec(&value).expect("JSON response");
                        bytes.push(b'\n');
                        bytes
                    }).await?;
                    output.write(&bytes).await?;
                    Ok::<_, anyhow::Error>(())
                }).await;
            }
            eprintln!("rowboatctl: {}", failure.message);
            failure.exit
        }
    }
}

async fn drain(
    stream: &mut QueryStream,
    response: &mut Response,
    streaming: bool,
    output: &Output,
    control: &mut Control,
    deadline: Option<Instant>,
    secrets: &[String],
) -> std::result::Result<(), Failure> {
    loop {
        let event = control
            .wait(deadline, false, stream.recv())
            .await?
            .ok_or_else(|| Failure::error("query_error", "Query stream ended without Complete"))?;
        // Streaming stays bounded: never retain its rows in the envelope builder.
        if !streaming && !response.observe(&event) {
            return Err(Failure::error(
                "response_limit",
                "Buffered query response exceeded the 64 MiB aggregate retention budget; use query-stream for larger results",
            ));
        }
        if let QueryEvent::Error { message, cancelled } = &event {
            let mut failure = Failure::error("query_error", redact(message.clone(), secrets));
            if *cancelled {
                failure.status = "cancelled";
                failure.code = "cancelled";
            }
            return Err(failure);
        }
        let complete = matches!(event, QueryEvent::Complete { .. });
        if streaming {
            let mut bytes = serde_json::to_vec(&event).expect("core query event");
            bytes.push(b'\n');
            control
                .wait(deadline, false, output.write(&bytes))
                .await?
                .map_err(|_| {
                    Failure::error("output_error", "Standard output closed; query cancelled")
                })?;
        }
        if complete {
            return Ok(());
        }
    }
}

async fn cleanup(mut stream: QueryStream, response: &mut Response, collect: bool) {
    stream.cancel();
    let _ = tokio::time::timeout(CLEANUP, async {
        while let Some(event) = stream.recv().await {
            if collect {
                response.observe(&event);
            }
            if matches!(
                event,
                QueryEvent::Complete { .. } | QueryEvent::Error { .. }
            ) {
                break;
            }
        }
        let _ = stream.close().await;
    })
    .await;
}

fn render(
    response: &Response,
    format: QueryFormat,
    engine: rowboat_core::Engine,
    elapsed: u64,
) -> Result<Vec<u8>> {
    let mut text = match format {
        QueryFormat::JsonEnvelope => serde_json::to_string(&response.envelope(elapsed, None))?,
        QueryFormat::Json => {
            let sets = response
                .results
                .iter()
                .map(|result| {
                    serde_json::from_str::<serde_json::Value>(&rowboat_services::export_json(
                        &result.data,
                    )?)
                    .context("Encode legacy JSON")
                })
                .collect::<Result<Vec<_>>>()?;
            let value = if sets.len() == 1 {
                sets.into_iter().next().unwrap()
            } else {
                serde_json::Value::Array(sets)
            };
            serde_json::to_string_pretty(&value)?
        }
        other => {
            let format = match other {
                QueryFormat::Table => Format::Table,
                QueryFormat::Csv => Format::Csv,
                _ => bail!("unsupported query format"),
            };
            response
                .results
                .iter()
                .map(|result| crate::emit(&result.data, format, engine, None))
                .collect::<Result<Vec<_>>>()?
                .concat()
        }
    };
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text.into_bytes())
}
