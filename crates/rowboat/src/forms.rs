use gpui_kit::{
    base::input::{InputState, TextareaState},
    *,
};
use rowboat_core::{
    Cell, ConnectionKind, ConnectionProfile, Engine, RowChange, SqliteFileOptions, SqliteOpenMode,
    SshConfig, Structure, TableRef,
};
use std::collections::BTreeMap;

pub fn parameters(text: &str) -> anyhow::Result<BTreeMap<String, Cell>> {
    if text.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let values: serde_json::Map<String, serde_json::Value> = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("Parameters must be a JSON object: {e}"))?;
    values
        .into_iter()
        .map(|(name, value)| {
            let cell = match value {
                serde_json::Value::Null => Cell::Null,
                serde_json::Value::String(v) => Cell::Text(v),
                serde_json::Value::Number(v) => Cell::Text(v.to_string()),
                serde_json::Value::Bool(v) => Cell::Text(v.to_string()),
                _ => anyhow::bail!("Parameter :{name} must be text, number, boolean, or null"),
            };
            Ok((name, cell))
        })
        .collect()
}

pub fn input(
    value: impl Into<SharedString>,
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(value)
            .placeholder(placeholder)
    })
}

pub fn textarea(
    value: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<TextareaState> {
    cx.new(|cx| TextareaState::new(window, cx).default_value(value).rows(9))
}

pub struct ConnectionForm {
    pub instance: uuid::Uuid,
    pub credential_request: Option<uuid::Uuid>,
    pub test_request: Option<uuid::Uuid>,
    pub test_task: Option<tokio::task::AbortHandle>,
    pub test_result: Option<(String, String)>,
    pub confirm_replace: bool,
    drafts: Vec<(Engine, EngineDraft)>,
    pub file_mode: SqliteOpenMode,
    pub file_request: Option<uuid::Uuid>,
    pub remember: bool,
    pub credential_exists: bool,
    pub password_dirty: bool,
    pub remember_touched: bool,
    pub keyring_loading: bool,
    pub id: String,
    pub engine: Engine,
    pub fields: BTreeMap<&'static str, Entity<InputState>>,
    pub password: Entity<InputState>,
    pub read_only: bool,
    pub ssh: bool,
    pub advanced: bool,
}

struct EngineDraft {
    fields: BTreeMap<&'static str, String>,
    password: String,
    remember: bool,
    credential_exists: bool,
    read_only: bool,
    ssh: bool,
    advanced: bool,
    file_mode: SqliteOpenMode,
}

impl Drop for ConnectionForm {
    fn drop(&mut self) {
        if let Some(task) = &self.test_task {
            task.abort();
        }
    }
}

impl ConnectionForm {
    pub fn cancel_test(&mut self) {
        if let Some(task) = self.test_task.take() {
            task.abort();
        }
        self.test_request = None;
        self.test_result = None;
    }
    pub fn is_file(&self) -> bool {
        self.engine.descriptor().connection_kind == ConnectionKind::File
    }

    /// Connection-presentation defaults live here rather than in button handlers.
    pub fn select_engine(&mut self, engine: Engine, window: &mut Window, cx: &mut App) {
        if self.engine == engine {
            return;
        }
        let draft = EngineDraft {
            fields: self
                .fields
                .iter()
                .filter(|(k, _)| !matches!(**k, "name" | "group"))
                .map(|(k, v)| (*k, v.read(cx).value().to_string()))
                .collect(),
            password: self.password.read(cx).value().to_string(),
            remember: self.remember,
            credential_exists: self.credential_exists,
            read_only: self.read_only,
            ssh: self.ssh,
            advanced: self.advanced,
            file_mode: self.file_mode,
        };
        self.drafts.retain(|(e, _)| *e != self.engine);
        self.drafts.push((self.engine, draft));
        self.file_request = None;
        self.credential_request = None;
        self.keyring_loading = false;
        self.cancel_test();
        self.confirm_replace = false;
        self.engine = engine;
        if let Some(index) = self.drafts.iter().position(|(e, _)| *e == engine) {
            let (_, draft) = self.drafts.remove(index);
            for (key, value) in draft.fields {
                self.fields[key].update(cx, |input, cx| input.set_value(value, window, cx));
            }
            self.password
                .update(cx, |p, cx| p.set_value(draft.password, window, cx));
            self.remember = draft.remember;
            self.credential_exists = draft.credential_exists;
            self.read_only = draft.read_only;
            self.ssh = draft.ssh;
            self.advanced = draft.advanced;
            self.file_mode = draft.file_mode;
            self.password_dirty = true;
            self.remember_touched = true;
            return;
        }
        self.password
            .update(cx, |p, cx| p.set_value("", window, cx));
        self.remember = false;
        self.credential_exists = false;
        self.password_dirty = true;
        self.remember_touched = true;
        self.ssh = false;
        self.advanced = false;
        self.read_only = false;
        let (port, database, username) = match engine {
            Engine::Postgres => ("5432", "postgres", "postgres"),
            Engine::MySql => ("3306", "", "root"),
            Engine::Sqlite => return,
        };
        for (key, value) in [
            ("port", port),
            ("database", database),
            ("username", username),
        ] {
            self.fields[key].update(cx, |input, cx| input.set_value(value, window, cx));
        }
    }

    pub fn new(
        profile: ConnectionProfile,
        password: String,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let is_file = profile.engine.descriptor().connection_kind == ConnectionKind::File;
        let ssh = profile.ssh.clone();
        let file_mode = profile.file.as_ref().map(|f| f.mode).unwrap_or_default();
        let values = [
            (
                "file_path",
                profile
                    .file
                    .as_ref()
                    .map(|f| f.path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            ("name", profile.name),
            ("group", profile.group),
            ("host", profile.host),
            ("port", profile.port.to_string()),
            ("database", profile.database),
            ("username", profile.username),
            ("ssl_mode", profile.ssl_mode),
            ("ssl_ca", profile.ssl_ca.unwrap_or_default()),
            ("ssl_cert", profile.ssl_cert.unwrap_or_default()),
            ("ssl_key", profile.ssl_key.unwrap_or_default()),
            (
                "ssh_host",
                ssh.as_ref().map(|s| s.host.clone()).unwrap_or_default(),
            ),
            (
                "ssh_port",
                ssh.as_ref().map(|s| s.port).unwrap_or(22).to_string(),
            ),
            (
                "ssh_username",
                ssh.as_ref().map(|s| s.username.clone()).unwrap_or_default(),
            ),
            (
                "ssh_identity",
                ssh.as_ref()
                    .and_then(|s| s.identity_file.clone())
                    .unwrap_or_default(),
            ),
        ];
        Self {
            instance: uuid::Uuid::new_v4(),
            credential_request: None,
            test_request: None,
            test_task: None,
            test_result: None,
            confirm_replace: false,
            drafts: Vec::new(),
            file_mode,
            file_request: None,
            remember: false,
            credential_exists: false,
            password_dirty: false,
            remember_touched: false,
            keyring_loading: false,
            id: profile.id,
            engine: profile.engine,
            fields: values
                .into_iter()
                .map(|(k, v)| (k, input(v, "", window, cx)))
                .collect(),
            password: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(if is_file { String::new() } else { password })
                    .masked(true)
                    .placeholder("Kept in memory for this session")
            }),
            read_only: profile.read_only,
            ssh: !is_file && ssh.is_some(),
            advanced: !is_file && ssh.is_some(),
        }
    }

    /// In-memory only; includes the password so a result cannot validate a changed draft.
    /// Never log or persist this value.
    pub fn fingerprint(&self, cx: &App) -> String {
        let fields: Vec<_> = self
            .fields
            .iter()
            .map(|(k, v)| (*k, v.read(cx).value().to_string()))
            .collect();
        serde_json::to_string(&(
            fields,
            self.engine,
            self.file_mode,
            self.read_only,
            self.ssh,
            self.remember,
            self.password.read(cx).value().to_string(),
        ))
        .unwrap()
    }

    pub fn profile(&self, cx: &App) -> anyhow::Result<ConnectionProfile> {
        let v = |key| self.fields[key].read(cx).value().trim().to_string();
        let optional = |key| {
            let s: String = v(key);
            (!s.is_empty()).then_some(s)
        };
        anyhow::ensure!(!v("name").is_empty(), "Give this connection a name.");
        if self.is_file() {
            anyhow::ensure!(
                !v("file_path").is_empty(),
                "Database file path is required."
            );
            anyhow::ensure!(
                !(self.read_only && self.file_mode == SqliteOpenMode::CreateIfMissing),
                "Read-only requires Open existing mode."
            );
            return Ok(ConnectionProfile {
                id: self.id.clone(),
                name: v("name"),
                group: v("group"),
                engine: self.engine,
                read_only: self.read_only,
                file: Some(SqliteFileOptions {
                    path: v("file_path").into(),
                    mode: self.file_mode,
                }),
                ..Default::default()
            });
        }
        anyhow::ensure!(!v("host").is_empty(), "Host is required.");
        anyhow::ensure!(!v("username").is_empty(), "Username is required.");
        anyhow::ensure!(
            ["disable", "prefer", "require", "verify-ca", "verify-full"]
                .contains(&v("ssl_mode").as_str()),
            "TLS mode must be disable, prefer, require, verify-ca, or verify-full."
        );
        let port: u16 = v("port")
            .parse()
            .map_err(|_| anyhow::anyhow!("Port must be between 1 and 65535."))?;
        anyhow::ensure!(port > 0, "Port must be between 1 and 65535.");
        let ssh = if self.ssh {
            anyhow::ensure!(
                !v("ssh_host").is_empty() && !v("ssh_username").is_empty(),
                "SSH host and username are required."
            );
            Some(SshConfig {
                host: v("ssh_host"),
                username: v("ssh_username"),
                port: v("ssh_port").parse()?,
                identity_file: optional("ssh_identity"),
            })
        } else {
            None
        };
        Ok(ConnectionProfile {
            file: None,
            transport: None,
            id: self.id.clone(),
            name: v("name"),
            group: v("group"),
            engine: self.engine,
            host: v("host"),
            port,
            database: v("database"),
            username: v("username"),
            ssl_mode: v("ssl_mode"),
            ssl_ca: optional("ssl_ca"),
            ssl_cert: optional("ssl_cert"),
            ssl_key: optional("ssl_key"),
            color: "blue".into(),
            read_only: self.read_only,
            ssh,
        })
    }
}

pub struct RowField {
    pub name: String,
    pub data_type: String,
    pub value: Entity<TextareaState>,
    pub null: bool,
    pub include: bool,
    pub binary: bool,
}

pub struct RowEditor {
    pub target: RowTarget,
    pub original: Option<Vec<(String, Cell)>>,
    pub fields: Vec<RowField>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowTarget {
    pub connection_id: String,
    pub session_generation: u64,
    pub table: TableRef,
}

impl RowEditor {
    pub fn new(
        target: RowTarget,
        structure: &Structure,
        row: Option<Vec<(String, Cell)>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let fields = structure
            .columns
            .iter()
            .map(|col| {
                let cell = row
                    .as_ref()
                    .and_then(|r| r.iter().find(|(name, _)| name == &col.name))
                    .map(|(_, v)| v.clone());
                let ty = col.data_type.to_ascii_lowercase();
                let binary = matches!(cell, Some(Cell::Binary(_)))
                    || ty == "bytea"
                    || ty.contains("blob")
                    || ty.contains("binary");
                let null = matches!(cell, Some(Cell::Null));
                let text = match cell {
                    Some(Cell::Text(s)) => s,
                    Some(Cell::Binary(b)) => Cell::Binary(b).display(),
                    _ => String::new(),
                };
                RowField {
                    name: col.name.clone(),
                    data_type: col.data_type.clone(),
                    value: cx.new(|cx| TextareaState::new(window, cx).default_value(text).rows(2)),
                    null,
                    include: row.is_some(),
                    binary,
                }
            })
            .collect();
        Self {
            target,
            original: row,
            fields,
        }
    }

    pub fn change(&self, cx: &App) -> anyhow::Result<RowChange> {
        let values: Vec<_> = self
            .fields
            .iter()
            .filter(|f| f.include)
            .map(|f| {
                Ok((
                    f.name.clone(),
                    if f.null {
                        Cell::Null
                    } else if f.binary {
                        parse_binary(&f.value.read(cx).value())
                            .map_err(|e| anyhow::anyhow!("{}: {e}", f.name))?
                    } else {
                        Cell::Text(f.value.read(cx).value().to_string())
                    },
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(name, cell)| {
                self.original.as_ref().is_none_or(|r| {
                    r.iter()
                        .find(|(n, _)| n == name)
                        .is_none_or(|(_, old)| old != cell)
                })
            })
            .collect();
        match &self.original {
            Some(original) => Ok(RowChange::Update {
                original: original.clone(),
                values,
            }),
            None => Ok(RowChange::Insert { values }),
        }
    }
}

pub(crate) fn parse_binary(input: &str) -> anyhow::Result<Cell> {
    let input = input.trim();
    let hex = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("\\x"))
        .unwrap_or(input);
    anyhow::ensure!(
        hex.len().is_multiple_of(2) && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "Binary values must be an even number of hexadecimal digits (optional 0x prefix)"
    );
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Cell::Binary(bytes))
}

#[cfg(test)]
mod tests {
    use super::{parameters, parse_binary};
    use rowboat_core::Cell;
    #[test]
    fn binary_input_is_lossless_and_rejects_malformed_hex() {
        assert_eq!(
            parse_binary("0x00ff7a").unwrap(),
            Cell::Binary(vec![0, 255, 122])
        );
        assert_eq!(parse_binary("\\x").unwrap(), Cell::Binary(vec![]));
        assert!(parse_binary("abc").is_err());
        assert!(parse_binary("0xzz").is_err());
        assert!(parse_binary("éé").is_err());
    }
    #[test]
    fn parameters_distinguish_null_empty_and_literal_null() {
        let values =
            parameters(r#"{"null":null,"empty":"","text":"NULL","number":12,"bool":false}"#)
                .unwrap();
        assert_eq!(values["null"], Cell::Null);
        assert_eq!(values["empty"], Cell::Text("".into()));
        assert_eq!(values["text"], Cell::Text("NULL".into()));
        assert_eq!(values["number"], Cell::Text("12".into()));
        assert!(parameters(r#"{"bad":[]}"#).is_err());
        assert!(parameters("[]").is_err());
    }
}
