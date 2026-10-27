//! Portable profiles and password-free connection URLs. These APIs do no I/O.
use crate::providers::{NetworkProvider, ServiceOperation};
use anyhow::{Result, anyhow, bail, ensure};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use rowboat_core::{
    ConnectionKind, ConnectionProfile, Engine, SqliteFileOptions, SqliteOpenMode, SshConfig,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fmt, net::IpAddr, path::PathBuf};
use url::{Host, Url};

/// Password is memory-only: serialization omits it and Debug redacts it.
#[derive(Clone, Serialize)]
pub struct ParsedConnectionUrl {
    pub profile: ConnectionProfile,
    #[serde(skip_serializing)]
    pub password: Option<String>,
}

impl fmt::Debug for ParsedConnectionUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedConnectionUrl")
            .field("profile", &self.profile)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ExportOptions {
    pub include_local_paths: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImportIdPolicy {
    #[default]
    NewIds,
    PreserveIds,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ImportOptions {
    pub id_policy: ImportIdPolicy,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MergePolicy {
    #[default]
    Reject,
    Skip,
    Replace,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    pub added: usize,
    pub skipped: usize,
    pub replaced: usize,
}

// Dedicated wire types prevent core's deliberately permissive serde model from
// silently accepting credentials, duplicate fields, or unknown future fields.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format: String,
    version: u32,
    connections: Vec<PortableProfile>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PortableProfile {
    Network(Box<PortableNetworkProfile>),
    File(PortableFileProfile),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableNetworkProfile {
    id: String,
    name: String,
    #[serde(default)]
    group: String,
    engine: Engine,
    host: String,
    port: u16,
    database: String,
    username: String,
    ssl_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ssl_ca: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ssl_cert: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ssl_key: Option<String>,
    #[serde(default = "default_color")]
    color: String,
    #[serde(default)]
    read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ssh: Option<PortableSsh>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableFileProfile {
    id: String,
    name: String,
    #[serde(default)]
    group: String,
    engine: Engine,
    file: PortableFileOptions,
    #[serde(default = "default_color")]
    color: String,
    #[serde(default)]
    read_only: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableFileOptions {
    path: PathBuf,
    #[serde(default)]
    mode: SqliteOpenMode,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableSsh {
    host: String,
    port: u16,
    username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity_file: Option<String>,
}

fn default_color() -> String {
    "blue".into()
}

impl From<PortableProfile> for ConnectionProfile {
    fn from(p: PortableProfile) -> Self {
        match p {
            PortableProfile::Network(p) => (*p).into(),
            PortableProfile::File(p) => Self {
                id: p.id,
                name: p.name,
                group: p.group,
                engine: p.engine,
                file: Some(SqliteFileOptions {
                    path: p.file.path,
                    mode: p.file.mode,
                }),
                host: String::new(),
                port: 0,
                database: String::new(),
                username: String::new(),
                ssl_mode: "disable".into(),
                color: p.color,
                read_only: p.read_only,
                ..Self::default()
            },
        }
    }
}

impl From<PortableNetworkProfile> for ConnectionProfile {
    fn from(p: PortableNetworkProfile) -> Self {
        Self {
            id: p.id,
            name: p.name,
            group: p.group,
            engine: p.engine,
            file: None,
            host: p.host,
            port: p.port,
            database: p.database,
            username: p.username,
            ssl_mode: p.ssl_mode,
            ssl_ca: p.ssl_ca,
            ssl_cert: p.ssl_cert,
            ssl_key: p.ssl_key,
            color: p.color,
            read_only: p.read_only,
            ssh: p.ssh.map(|s| SshConfig {
                host: s.host,
                port: s.port,
                username: s.username,
                identity_file: s.identity_file,
            }),
            transport: None,
        }
    }
}

impl PortableProfile {
    fn from_profile(p: &ConnectionProfile, include_paths: bool) -> Self {
        if let Some(file) = &p.file {
            // The file path is the essential connection target, not an optional
            // TLS/SSH credential path. Export must not silently drop its identity.
            return Self::File(PortableFileProfile {
                id: p.id.clone(),
                name: p.name.clone(),
                group: p.group.clone(),
                engine: p.engine,
                file: PortableFileOptions {
                    path: file.path.clone(),
                    mode: file.mode,
                },
                color: p.color.clone(),
                read_only: p.read_only,
            });
        }
        let path = |value: &Option<String>| if include_paths { value.clone() } else { None };
        Self::Network(Box::new(PortableNetworkProfile {
            id: p.id.clone(),
            name: p.name.clone(),
            group: p.group.clone(),
            engine: p.engine,
            host: p.host.clone(),
            port: p.port,
            database: p.database.clone(),
            username: p.username.clone(),
            ssl_mode: p.ssl_mode.clone(),
            ssl_ca: path(&p.ssl_ca),
            ssl_cert: path(&p.ssl_cert),
            ssl_key: path(&p.ssl_key),
            color: p.color.clone(),
            read_only: p.read_only,
            ssh: p.ssh.as_ref().map(|s| PortableSsh {
                host: s.host.clone(),
                port: s.port,
                username: s.username.clone(),
                identity_file: path(&s.identity_file),
            }),
        }))
    }
}

pub fn validate_profile(profile: &ConnectionProfile) -> Result<()> {
    for (label, value) in [("ID", &profile.id), ("name", &profile.name)] {
        validate_text(value, label, true)?;
    }
    for (label, value) in [("group", &profile.group), ("color", &profile.color)] {
        validate_text(value, label, false)?;
    }
    if profile.engine.descriptor().connection_kind == ConnectionKind::File {
        let file = profile
            .file
            .as_ref()
            .ok_or_else(|| anyhow!("SQLite requires typed file options"))?;
        let path = file
            .path
            .to_str()
            .ok_or_else(|| anyhow!("SQLite profile path must be UTF-8"))?;
        validate_text(path, "file path", true)?;
        ensure!(path.len() <= 4096, "SQLite profile path exceeds 4096 bytes");
        let folded = path.to_ascii_lowercase();
        ensure!(
            folded != ":memory:" && !folded.starts_with("file:") && !folded.starts_with("sqlite:"),
            "SQLite profiles require a filesystem path, not a memory database or URI"
        );
        ensure!(
            !profile.read_only || file.mode == SqliteOpenMode::OpenExisting,
            "Read-only SQLite profiles cannot create missing files"
        );
        ensure!(
            profile.ssh.is_none() && profile.transport.is_none(),
            "SQLite file profiles do not support SSH or live transport overrides"
        );
        return Ok(());
    }
    ensure!(
        profile.file.is_none(),
        "Network profiles cannot contain file options"
    );
    validate_text(&profile.database, "database", true)?;
    validate_text(&profile.username, "username", true)?;
    canonical_host(&profile.host)?;
    ensure!(
        profile.port != 0,
        "Database port must be between 1 and 65535"
    );
    ensure!(
        ["disable", "prefer", "require", "verify-ca", "verify-full"]
            .contains(&profile.ssl_mode.as_str()),
        "Unsupported TLS mode"
    );
    for path in [&profile.ssl_ca, &profile.ssl_cert, &profile.ssl_key]
        .into_iter()
        .flatten()
    {
        validate_text(path, "TLS path", true)?;
    }
    if let Some(ssh) = &profile.ssh {
        canonical_host(&ssh.host)?;
        ensure!(ssh.port != 0, "SSH port must be between 1 and 65535");
        validate_text(&ssh.username, "SSH username", true)?;
        ensure!(
            !ssh.username.chars().any(char::is_whitespace),
            "SSH username must not contain whitespace"
        );
        if let Some(path) = &ssh.identity_file {
            validate_text(path, "SSH identity path", true)?;
        }
    }
    Ok(())
}

fn validate_text(value: &str, label: &str, required: bool) -> Result<()> {
    ensure!(
        !required || !value.trim().is_empty(),
        "Profile {label} must not be empty"
    );
    ensure!(
        !value.chars().any(char::is_control),
        "Profile {label} must not contain control characters"
    );
    Ok(())
}

pub fn validate_connection_host(host: &str) -> Result<()> {
    canonical_host(host).map(|_| ())
}

fn canonical_host(host: &str) -> Result<String> {
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(address.to_string());
    }
    ensure!(
        !host.is_empty()
            && !host.chars().any(char::is_whitespace)
            && !host.contains([':', '/', '\\', '[', ']', '%', '@', '?', '#']),
        "Invalid connection host; use a DNS name or unbracketed IP address"
    );
    let parsed = Host::parse(host).map_err(|_| anyhow!("Invalid connection host"))?;
    match parsed {
        Host::Domain(domain) => {
            let labels = domain.strip_suffix('.').unwrap_or(&domain);
            ensure!(
                !labels.is_empty()
                    && labels.len() <= 253
                    && labels.split('.').all(|label| {
                        !label.is_empty()
                            && label.len() <= 63
                            && !label.starts_with('-')
                            && !label.ends_with('-')
                            && label
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    }),
                "Invalid DNS host name"
            );
            Ok(domain)
        }
        Host::Ipv4(address) => Ok(address.to_string()),
        Host::Ipv6(address) => Ok(address.to_string()),
    }
}

fn validate_list(profiles: &[ConnectionProfile]) -> Result<()> {
    let mut ids = HashSet::new();
    for profile in profiles {
        validate_profile(profile)?;
        ensure!(
            ids.insert(&profile.id),
            "Duplicate connection IDs are not allowed"
        );
    }
    Ok(())
}

pub fn export_connections(profiles: &[ConnectionProfile]) -> Result<String> {
    export_connections_with_options(profiles, &ExportOptions::default())
}

pub fn export_connections_with_options(
    profiles: &[ConnectionProfile],
    options: &ExportOptions,
) -> Result<String> {
    validate_list(profiles)?;
    let document = Document {
        format: "rowboat-connections".into(),
        version: if profiles.iter().any(|profile| profile.file.is_some()) {
            2
        } else {
            1
        },
        connections: profiles
            .iter()
            .map(|p| PortableProfile::from_profile(p, options.include_local_paths))
            .collect(),
    };
    Ok(serde_json::to_string_pretty(&document)?)
}

pub fn import_connections(input: &str) -> Result<Vec<ConnectionProfile>> {
    import_connections_with_options(input, &ImportOptions::default())
}

pub fn import_connections_with_options(
    input: &str,
    options: &ImportOptions,
) -> Result<Vec<ConnectionProfile>> {
    // Do not attach serde's raw error: it can contain a credential value or key.
    let document: Document = serde_json::from_str(input).map_err(|_| anyhow!("Invalid connection document: malformed data, missing required fields, or unknown/duplicate fields (passwords and secrets are not permitted)"))?;
    ensure!(
        matches!(
            document.format.as_str(),
            "rowboat-connections" | "keel-connections"
        ),
        "Unsupported connection document format"
    );
    ensure!(
        matches!(document.version, 1 | 2),
        "Unsupported connection document version"
    );
    ensure!(
        document.version != 1
            || document
                .connections
                .iter()
                .all(|p| matches!(p, PortableProfile::Network(_))),
        "File profiles require connection document version 2"
    );
    ensure!(
        document.format != "keel-connections" || document.version == 1,
        "Legacy Keel connection documents support version 1 only"
    );
    let mut profiles: Vec<ConnectionProfile> =
        document.connections.into_iter().map(Into::into).collect();
    validate_list(&profiles)?;
    if options.id_policy == ImportIdPolicy::NewIds {
        let mut ids: HashSet<String> = profiles.iter().map(|p| p.id.clone()).collect();
        for profile in &mut profiles {
            loop {
                let id = uuid::Uuid::new_v4().to_string();
                if ids.insert(id.clone()) {
                    profile.id = id;
                    break;
                }
            }
        }
    }
    Ok(profiles)
}

pub fn merge_connections(
    existing: &mut Vec<ConnectionProfile>,
    incoming: Vec<ConnectionProfile>,
) -> Result<ImportReport> {
    merge_connections_with_policy(existing, incoming, MergePolicy::Reject)
}

pub fn merge_connections_with_policy(
    existing: &mut Vec<ConnectionProfile>,
    incoming: Vec<ConnectionProfile>,
    policy: MergePolicy,
) -> Result<ImportReport> {
    validate_list(existing)?;
    validate_list(&incoming)?;
    if policy == MergePolicy::Reject {
        let ids: HashSet<_> = existing.iter().map(|p| &p.id).collect();
        ensure!(
            !incoming.iter().any(|p| ids.contains(&p.id)),
            "Connection ID collision; import with new IDs or select an explicit merge policy"
        );
    }
    let mut report = ImportReport::default();
    for profile in incoming {
        match existing.iter().position(|p| p.id == profile.id) {
            None => {
                existing.push(profile);
                report.added += 1;
            }
            Some(index) if policy == MergePolicy::Replace => {
                existing[index] = profile;
                report.replaced += 1;
            }
            Some(_) => report.skipped += 1,
        }
    }
    Ok(report)
}

fn decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%' {
            ensure!(
                bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
                    && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit),
                "Malformed URL percent encoding"
            );
        }
    }
    percent_decode_str(value)
        .decode_utf8()
        .map(|s| s.into_owned())
        .map_err(|_| anyhow!("URL fields must be UTF-8"))
}

pub fn parse_connection_url(input: &str) -> Result<ParsedConnectionUrl> {
    if input.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("sqlite") || scheme.eq_ignore_ascii_case("file")
    }) {
        crate::service_support(Engine::Sqlite, ServiceOperation::ConnectionUrl)?;
    }
    ensure!(
        !input.chars().any(|c| c.is_whitespace() || c.is_control()),
        "URL whitespace/control characters must be percent encoded"
    );
    let (scheme, rest) = input
        .split_once("://")
        .ok_or_else(|| anyhow!("Expected a PostgreSQL or MySQL connection URL"))?;
    let engine = match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => Engine::Postgres,
        "mysql" => Engine::MySql,
        _ => bail!("Unsupported connection URL scheme"),
    };
    ensure!(
        !rest.contains('#'),
        "Connection URL fragments are not supported"
    );
    let (authority, suffix) = rest
        .split_once('/')
        .ok_or_else(|| anyhow!("Connection URL requires a database path"))?;
    ensure!(
        !authority.contains('?') && authority.matches('@').count() == 1,
        "Connection URL requires an explicit username and host; encode userinfo delimiters"
    );
    let (userinfo, hostport) = authority
        .split_once('@')
        .ok_or_else(|| anyhow!("Connection URL requires a username"))?;
    ensure!(
        !hostport.is_empty() && !hostport.ends_with(':'),
        "Connection URL has an empty host or port"
    );
    let (raw_username, raw_password) = match userinfo.split_once(':') {
        Some((u, p)) => (u, Some(p)),
        None => (userinfo, None),
    };
    let username = decode(raw_username)?;
    let password = raw_password.map(decode).transpose()?;
    ensure!(
        !password.as_ref().is_some_and(|p| p.contains('\0')),
        "URL password must not contain NUL"
    );
    let (database, query) = suffix
        .split_once('?')
        .map_or((suffix, None), |(p, q)| (p, Some(q)));
    ensure!(
        !database.is_empty() && !database.contains('/'),
        "Connection URL requires one database path component (encode embedded slashes)"
    );
    let database = decode(database)?;
    let url = Url::parse(input).map_err(|_| anyhow!("Malformed connection URL host or port"))?;
    let host = match url
        .host()
        .ok_or_else(|| anyhow!("Connection URL requires a host"))?
    {
        Host::Domain(name) => canonical_host(&decode(name)?)?,
        Host::Ipv4(address) => address.to_string(),
        Host::Ipv6(address) => address.to_string(),
    };
    let port = url.port().unwrap_or(
        NetworkProvider::for_engine(engine, ServiceOperation::ConnectionUrl)?.default_port(),
    );
    let mut profile = ConnectionProfile {
        name: format!("{database} @ {host}"),
        group: String::new(),
        engine,
        host,
        port,
        database,
        username,
        ..ConnectionProfile::default()
    };
    if let Some(query) = query {
        let mut seen = HashSet::new();
        for option in query.split('&') {
            let (key, value) = option
                .split_once('=')
                .ok_or_else(|| anyhow!("URL options require a key and value"))?;
            let key = decode(&key.replace('+', " "))?;
            let value = decode(&value.replace('+', " "))?;
            let field = match (engine, key.as_str()) {
                (_, "sslmode") | (Engine::MySql, "ssl-mode") => "mode",
                (Engine::Postgres, "sslrootcert") | (Engine::MySql, "ssl-ca") => "ca",
                (Engine::Postgres, "sslcert") | (Engine::MySql, "ssl-cert") => "cert",
                (Engine::Postgres, "sslkey") | (Engine::MySql, "ssl-key") => "key",
                _ => bail!(
                    "Unsupported connection URL option. Supported options: sslmode, sslrootcert/ssl-ca, sslcert/ssl-cert and sslkey/ssl-key (certificate option names depend on the engine). Remove other options or configure supported settings in the form."
                ),
            };
            ensure!(seen.insert(field), "Duplicate connection URL option");
            match field {
                "mode" => {
                    profile.ssl_mode = match (engine, value.as_str()) {
                        (Engine::MySql, "DISABLED") => "disable".into(),
                        (Engine::MySql, "PREFERRED") => "prefer".into(),
                        (Engine::MySql, "REQUIRED") => "require".into(),
                        (Engine::MySql, "VERIFY_CA") => "verify-ca".into(),
                        (Engine::MySql, "VERIFY_IDENTITY") => "verify-full".into(),
                        _ => value,
                    }
                }
                "ca" => profile.ssl_ca = Some(value),
                "cert" => profile.ssl_cert = Some(value),
                "key" => profile.ssl_key = Some(value),
                _ => unreachable!(),
            }
        }
    }
    validate_profile(&profile)?;
    Ok(ParsedConnectionUrl { profile, password })
}

/// Password-free URL. Refuses SSH/read-only profiles rather than losing policy.
pub fn connection_url(profile: &ConnectionProfile) -> Result<String> {
    let provider = NetworkProvider::for_engine(profile.engine, ServiceOperation::ConnectionUrl)?;
    validate_profile(profile)?;
    ensure!(
        profile.ssh.is_none(),
        "A connection URL cannot preserve SSH settings; export connections as JSON instead"
    );
    ensure!(
        !profile.read_only,
        "A connection URL cannot preserve read-only policy; export connections as JSON instead"
    );
    let encode = |value: &str| utf8_percent_encode(value, NON_ALPHANUMERIC).to_string();
    let host = canonical_host(&profile.host)?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    let scheme = provider.scheme();
    let mut url = format!(
        "{scheme}://{}@{host}:{}/{}?sslmode={}",
        encode(&profile.username),
        profile.port,
        encode(&profile.database),
        encode(&profile.ssl_mode)
    );
    let keys = provider.tls_keys();
    for (key, path) in keys
        .into_iter()
        .zip([&profile.ssl_ca, &profile.ssl_cert, &profile.ssl_key])
    {
        if let Some(path) = path {
            url.push_str(&format!("&{key}={}", encode(path)));
        }
    }
    Ok(url)
}
