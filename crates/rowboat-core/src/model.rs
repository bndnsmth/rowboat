use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Engine {
    #[default]
    Postgres,
    MySql,
    Sqlite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SqliteOpenMode {
    #[default]
    OpenExisting,
    CreateIfMissing,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SqliteFileOptions {
    pub path: std::path::PathBuf,
    #[serde(default)]
    pub mode: SqliteOpenMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub group: String,
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    /// disable, prefer, require, verify-ca, verify-full
    pub ssl_mode: String,
    pub ssl_ca: Option<String>,
    pub ssl_cert: Option<String>,
    pub ssl_key: Option<String>,
    pub color: String,
    pub read_only: bool,
    pub ssh: Option<SshConfig>,
    /// Ephemeral transport override. Host remains the TLS peer identity.
    /// Never persist a live tunnel endpoint or accept one from imported JSON.
    #[serde(skip)]
    pub transport: Option<ConnectionTransport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<SqliteFileOptions>,
}

#[derive(Debug, Clone)]
pub enum ConnectionTransport {
    /// PostgreSQL paths must end in `.s.PGSQL.<profile.port>`.
    UnixSocket { path: std::path::PathBuf },
}

impl Default for ConnectionProfile {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: "Local PostgreSQL".into(),
            group: "Development".into(),
            engine: Engine::Postgres,
            host: "127.0.0.1".into(),
            port: 5432,
            database: "postgres".into(),
            username: "postgres".into(),
            ssl_mode: "prefer".into(),
            ssl_ca: None,
            ssl_cert: None,
            ssl_key: None,
            color: "blue".into(),
            read_only: false,
            ssh: None,
            transport: None,
            file: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub identity_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cell {
    Null,
    Text(String),
    Binary(Vec<u8>),
}
impl Cell {
    pub fn display(&self) -> String {
        match self {
            Self::Null => "NULL".into(),
            Self::Text(s) => s.clone(),
            Self::Binary(b) => format!("0x{}", hex::encode(b)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    pub rows_affected: u64,
    pub elapsed_ms: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableInfo {
    pub table: TableRef,
    pub kind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Structure {
    pub columns: Vec<Column>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    pub definition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexInfo {
    pub name: String,
    pub columns: String,
    pub unique: bool,
    pub definition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: String,
    pub referenced_table: String,
    pub referenced_columns: String,
    pub on_update: String,
    pub on_delete: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FilterOp {
    Eq,
    NotEq,
    Less,
    Greater,
    LessEq,
    GreaterEq,
    Contains,
    StartsWith,
    IsNull,
    IsNotNull,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
    pub value: Cell,
}

/// Bounded, typed filter tree; values are always parameters, never SQL text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FilterExpr {
    Leaf(Filter),
    And(Vec<FilterExpr>),
    Or(Vec<FilterExpr>),
    In {
        column: String,
        values: Vec<Cell>,
        negated: bool,
    },
    Between {
        column: String,
        low: Cell,
        high: Cell,
        negated: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKeyColumn {
    pub source: String,
    pub referenced: String,
    pub nullable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKeyRelationship {
    pub name: String,
    pub source: TableRef,
    pub referenced: TableRef,
    /// Constraint ordinal order; identifiers are not comma-separated strings.
    pub columns: Vec<ForeignKeyColumn>,
    pub on_update: String,
    pub on_delete: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum RelationshipDirection {
    Referenced,
    Referencing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationshipTarget {
    pub table: TableRef,
    pub filter: FilterExpr,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrowseRequest {
    pub filters: Vec<Filter>,
    pub sort: Option<(String, bool)>,
    pub limit: u32,
    pub offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RowChange {
    Insert {
        values: Vec<(String, Cell)>,
    },
    Update {
        original: Vec<(String, Cell)>,
        values: Vec<(String, Cell)>,
    },
    Delete {
        original: Vec<(String, Cell)>,
    },
}

/// Stable database API implemented in database.rs:
/// Database::connect(&ConnectionProfile, &str) -> Result<Database> (async)
/// engine(&self) -> Engine; query(&self, &str, usize) -> Result<Vec<QueryResult>> (async)
/// catalog(&self) -> Result<Vec<TableInfo>>; structure(&self, &TableRef) -> Result<Structure>
/// browse(&self, &TableRef, &BrowseRequest) -> Result<QueryResult>
/// apply_changes(&self, &TableRef, &[RowChange]) -> Result<u64>
/// begin / commit / rollback / cancel(&self) -> Result<()> (all async)
/// Database is Clone + Send + Sync. Read-only is enforced on the server session.
const _: () = ();
