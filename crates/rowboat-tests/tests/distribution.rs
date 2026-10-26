//! Exercise mise's real GitHub backend against a loopback-only release fixture.
//! No GitHub publishing, user config, credentials, or user database is involved.
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

struct ReleaseServer {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ReleaseServer {
    fn start(archive: &Path) -> Result<Self> {
        let archive = archive
            .canonicalize()
            .context("Open explicit distribution archive; supply an absolute path")?;
        let name = archive.file_name().unwrap().to_str().unwrap().to_owned();
        let checksum_name = format!("{name}.sha256");
        let checksum = std::fs::read(archive.with_file_name(&checksum_name))?;
        let digest = std::str::from_utf8(&checksum)?
            .split_whitespace()
            .next()
            .context("Missing archive checksum")?
            .to_owned();
        let bytes = std::fs::read(&archive)?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let base = format!("http://{address}");
        let mut assets = vec![
            json!({
                "id": 1, "name": name, "size": bytes.len(),
                "url": format!("{base}/assets/1"),
                "browser_download_url": format!("{base}/{name}"),
                "digest": format!("sha256:{digest}"),
                "content_type": "application/gzip"
            }),
            json!({
                "id": 2, "name": checksum_name, "size": checksum.len(),
                "url": format!("{base}/assets/2"),
                "browser_download_url": format!("{base}/{checksum_name}"),
                "content_type": "text/plain"
            }),
        ];
        // Wrong-platform candidates must not win asset selection. They have no
        // download route: a mistaken selection fails instead of testing a lie.
        for target in [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        ] {
            if !name.contains(target) {
                let other = format!("rowboat-{}-{target}.tar.gz", env!("CARGO_PKG_VERSION"));
                assets.push(json!({
                    "id": assets.len() + 1, "name": other, "size": 1,
                    "url": format!("{base}/unavailable/{other}"),
                    "browser_download_url": format!("{base}/{other}"),
                    "content_type": "application/gzip"
                }));
            }
        }
        let release = json!({
            "id": 1, "tag_name": format!("v{}", env!("CARGO_PKG_VERSION")),
            "created_at": "2026-01-01T00:00:00Z",
            "published_at": "2026-01-01T00:00:00Z",
            "draft": false, "prerelease": false, "assets": assets
        });
        let release_bytes = serde_json::to_vec(&release)?;
        let releases = serde_json::to_vec(&vec![release])?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            for incoming in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = incoming else { break };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut request = String::new();
                let mut reader = BufReader::new(&mut stream);
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                }
                let route = request.split_whitespace().nth(1).unwrap_or("");
                let route = route.split('?').next().unwrap_or("");
                let (status, body): (&str, &[u8]) = if route == format!("/{name}")
                    || route == "/assets/1"
                {
                    ("200 OK", &bytes)
                } else if route == format!("/{checksum_name}") || route == "/assets/2" {
                    ("200 OK", &checksum)
                } else if route.ends_with("/releases") {
                    ("200 OK", &releases)
                } else if route.contains("/releases/tags/") || route.ends_with("/releases/latest") {
                    ("200 OK", &release_bytes)
                } else {
                    ("404 Not Found", b"unknown fixture route")
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if !request.starts_with("HEAD ") {
                    let _ = stream.write_all(body);
                }
            }
        });
        Ok(Self {
            address,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for ReleaseServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn mise(directory: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("mise")
        .args(args)
        .current_dir(directory)
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").context("PATH is required")?,
        )
        .env("HOME", directory)
        .env("MISE_CONFIG_FILE", directory.join("mise.toml"))
        .env("MISE_GLOBAL_CONFIG_FILE", directory.join("global.toml"))
        .env("MISE_DATA_DIR", directory.join("mise-data"))
        .env("MISE_CACHE_DIR", directory.join("mise-cache"))
        .env("MISE_STATE_DIR", directory.join("mise-state"))
        .env("MISE_TRUSTED_CONFIG_PATHS", directory)
        .env("MISE_USE_VERSIONS_HOST", "0")
        .env("MISE_YES", "1")
        .env("NO_COLOR", "1")
        .env("ROWBOAT_DATA_DIR", directory.join("profiles"))
        .output()
        .context("Run installed mise for the disposable release fixture")?;
    ensure!(
        output.status.success(),
        "mise fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[test]
fn github_backend_installs_both_binaries_and_selects_the_native_asset() -> Result<()> {
    let archive = PathBuf::from(
        std::env::var_os("ROWBOAT_DISTRIBUTION_ARCHIVE")
            .context("Set ROWBOAT_DISTRIBUTION_ARCHIVE to the native release archive")?,
    );
    let server = ReleaseServer::start(&archive)?;
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("global.toml"), "")?;
    // Test autodetection, not a hardcoded asset name or executable rename. The
    // user-facing release's bin/ layout must work with the stock GitHub backend.
    std::fs::write(
        directory.path().join("mise.toml"),
        format!(
            "[tools.\"github:rowboat-fixture/rowboat\"]\nversion = \"{}\"\napi_url = \"http://{}\"\n",
            env!("CARGO_PKG_VERSION"),
            server.address
        ),
    )?;
    mise(directory.path(), &["install"])?;
    ensure!(
        mise(directory.path(), &["exec", "--", "rowboat", "--version"])?.trim()
            == concat!("Rowboat ", env!("CARGO_PKG_VERSION"))
    );
    ensure!(
        mise(directory.path(), &["exec", "--", "rowboatctl", "--version"])?.trim()
            == concat!("rowboatctl ", env!("CARGO_PKG_VERSION"))
    );
    let file = directory.path().join("fixture.sqlite");
    let path = file.to_str().context("Fixture path must be Unicode")?;
    let output = mise(
        directory.path(),
        &[
            "exec",
            "--",
            "rowboatctl",
            "--database-file",
            path,
            "--create",
            "query",
            "--sql",
            "SELECT 42 AS portable",
            "--format",
            "json-envelope",
        ],
    )?;
    let response: serde_json::Value = serde_json::from_str(&output)?;
    ensure!(response["ok"] == true && response["results"][0]["rows"][0][0] == "42");
    Ok(())
}
