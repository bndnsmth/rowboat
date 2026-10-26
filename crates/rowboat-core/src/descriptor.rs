//! Implemented provider scope, not a statement about a connected user's grants.
use crate::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionKind {
    Network,
    File,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceBehavior {
    Schemas,
    Databases,
    AttachedDatabases,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialectFamily {
    PostgreSql,
    MySql,
    Sqlite,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifierCase {
    Lower,
    Preserve,
}
#[derive(Debug, Clone, Copy)]
pub struct DialectDescriptor {
    pub family: DialectFamily,
    pub identifier_quote: char,
    pub unquoted_case: IdentifierCase,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
    Function,
    Procedure,
    Sequence,
    Trigger,
    Event,
    Index,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolProvider {
    Postgres,
    MySql,
    Sqlite,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSupport {
    Supported(ToolProvider),
    Unsupported(&'static str),
}
#[derive(Debug, Clone, Copy)]
pub struct EngineTools {
    pub administration: ToolSupport,
    pub metrics: ToolSupport,
    pub explain: ToolSupport,
    pub schema: ToolSupport,
    pub backup: ToolSupport,
}
#[derive(Debug, Clone, Copy)]
pub struct EngineDescriptor {
    pub name: &'static str,
    pub connection_kind: ConnectionKind,
    pub namespaces: NamespaceBehavior,
    pub dialect: DialectDescriptor,
    pub object_kinds: &'static [ObjectKind],
    pub tools: EngineTools,
}
const fn tools(provider: ToolProvider) -> EngineTools {
    let support = ToolSupport::Supported(provider);
    EngineTools {
        administration: support,
        metrics: support,
        explain: support,
        schema: support,
        backup: support,
    }
}
const PG: EngineDescriptor = EngineDescriptor {
    name: "PostgreSQL",
    connection_kind: ConnectionKind::Network,
    namespaces: NamespaceBehavior::Schemas,
    dialect: DialectDescriptor {
        family: DialectFamily::PostgreSql,
        identifier_quote: '"',
        unquoted_case: IdentifierCase::Lower,
    },
    object_kinds: &[
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::ForeignTable,
        ObjectKind::Function,
        ObjectKind::Procedure,
        ObjectKind::Sequence,
        ObjectKind::Trigger,
        ObjectKind::Index,
    ],
    tools: tools(ToolProvider::Postgres),
};
const MY: EngineDescriptor = EngineDescriptor {
    name: "MySQL",
    connection_kind: ConnectionKind::Network,
    namespaces: NamespaceBehavior::Databases,
    dialect: DialectDescriptor {
        family: DialectFamily::MySql,
        identifier_quote: '`',
        unquoted_case: IdentifierCase::Preserve,
    },
    object_kinds: &[
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::Function,
        ObjectKind::Procedure,
        ObjectKind::Trigger,
        ObjectKind::Event,
        ObjectKind::Index,
    ],
    tools: tools(ToolProvider::MySql),
};
const SQLITE: EngineDescriptor = EngineDescriptor {
    name: "SQLite",
    connection_kind: ConnectionKind::File,
    namespaces: NamespaceBehavior::AttachedDatabases,
    dialect: DialectDescriptor {
        family: DialectFamily::Sqlite,
        identifier_quote: '"',
        unquoted_case: IdentifierCase::Preserve,
    },
    object_kinds: &[ObjectKind::Table, ObjectKind::View],
    tools: EngineTools {
        administration: ToolSupport::Unsupported("SQLite has no server administration provider"),
        metrics: ToolSupport::Unsupported("SQLite server metrics are not applicable"),
        explain: ToolSupport::Supported(ToolProvider::Sqlite),
        schema: ToolSupport::Supported(ToolProvider::Sqlite),
        backup: ToolSupport::Supported(ToolProvider::Sqlite),
    },
};
impl Engine {
    pub fn descriptor(self) -> &'static EngineDescriptor {
        match self {
            Self::Postgres => &PG,
            Self::MySql => &MY,
            Self::Sqlite => &SQLITE,
        }
    }
}
