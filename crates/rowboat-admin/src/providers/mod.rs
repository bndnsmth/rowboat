//! Optional administration providers. Consumers ask about operations, not engine
//! names. Provider support is independent of current-session authorization.
use crate::objects::ObjectKind;
use rowboat_core::{Engine, EngineDescriptor, ToolProvider, ToolSupport};
use serde::Serialize;
use std::fmt;

pub(crate) mod access;
pub(crate) mod access_sql;
mod catalog;
pub(crate) mod ddl;
pub(crate) mod explain;
pub(crate) mod lexical;
pub(crate) mod metrics;
pub(crate) mod mysql_columns;
pub(crate) mod namespaces;
pub(crate) mod objects;
pub(crate) mod schema;
pub(crate) use catalog::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Operation {
    ConnectionInfo,
    Namespaces,
    ObjectCatalog(ObjectKind),
    ObjectEdit,
    SchemaInspection,
    SchemaDdl,
    SchemaComparison,
    NamespaceDdl,
    AccessManagement,
    ServerActivity,
    ServerMetrics,
    Explain,
    Analyze,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Atomicity {
    ReadOnly,
    Transactional,
    NonTransactional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ExecutionSemantics {
    pub atomicity: Atomicity,
    pub implicit_commit: bool,
    pub confirmation_required: bool,
    /// Provider can preserve attributes when rebuilding an existing object.
    /// Individual plans can still reject unsupported catalog attributes.
    pub rebuild_supported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct UnsupportedOperation {
    pub engine: Engine,
    pub operation: Operation,
    pub reason: &'static str,
}
impl fmt::Display for UnsupportedOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is unavailable for {:?}: {}",
            self.operation, self.engine, self.reason
        )
    }
}
impl std::error::Error for UnsupportedOperation {}

/// Closed set of installed server providers. A newly registered core engine
/// never enters a PostgreSQL/MySQL implementation through an `else` fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerKind {
    Postgres,
    MySql,
}

const POSTGRES_OBJECTS: &[ObjectKind] = &[
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
    ObjectKind::View,
    ObjectKind::MaterializedView,
    ObjectKind::Role,
    ObjectKind::Sequence,
];
const MYSQL_OBJECTS: &[ObjectKind] = &[
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
    ObjectKind::View,
    ObjectKind::Role,
];

/// The authoritative core descriptor, not a second capability registry.
pub fn descriptor(engine: Engine) -> &'static EngineDescriptor {
    engine.descriptor()
}

/// Advanced admin catalog kinds (including roles), narrower than core's
/// table/view browser. Availability is gated by the authoritative core tool slot.
pub fn supported_object_kinds(engine: Engine) -> &'static [ObjectKind] {
    match engine.descriptor().tools.administration {
        ToolSupport::Supported(ToolProvider::Postgres) => POSTGRES_OBJECTS,
        ToolSupport::Supported(ToolProvider::MySql) => MYSQL_OBJECTS,
        _ => &[],
    }
}

pub fn operation_support(
    engine: Engine,
    operation: Operation,
) -> Result<ExecutionSemantics, UnsupportedOperation> {
    let unavailable = |reason| UnsupportedOperation {
        engine,
        operation,
        reason,
    };
    if tool_support(engine, operation) == ToolSupport::Supported(ToolProvider::Sqlite) {
        let mutation = match operation {
            Operation::SchemaDdl => true,
            Operation::SchemaInspection | Operation::Explain => false,
            Operation::Analyze => {
                return Err(unavailable(
                    "SQLite provides EXPLAIN QUERY PLAN, not executing EXPLAIN ANALYZE",
                ));
            }
            Operation::NamespaceDdl => {
                return Err(unavailable(
                    "SQLite schemas are attached databases; server namespace DDL is unavailable",
                ));
            }
            Operation::SchemaComparison => {
                return Err(unavailable(
                    "SQLite supports reviewed native DDL, but dependency-aware schema comparison is not implemented",
                ));
            }
            _ => return Err(unavailable("This operation is not provided by SQLite")),
        };
        return Ok(ExecutionSemantics {
            atomicity: if mutation {
                Atomicity::Transactional
            } else {
                Atomicity::ReadOnly
            },
            implicit_commit: false,
            confirmation_required: mutation,
            rebuild_supported: false,
        });
    }
    let server = server(engine, operation)?;
    if let Operation::ObjectCatalog(kind) = operation
        && !supported_object_kinds(engine).contains(&kind)
    {
        return Err(unavailable(
            "The provider does not support this object kind",
        ));
    }
    let mutation = matches!(
        operation,
        Operation::ObjectEdit
            | Operation::SchemaDdl
            | Operation::SchemaComparison
            | Operation::NamespaceDdl
            | Operation::AccessManagement
    );
    let implicit_commit = mutation && server == ServerKind::MySql;
    Ok(ExecutionSemantics {
        atomicity: if operation == Operation::Analyze
            || operation == Operation::NamespaceDdl
            || implicit_commit
        {
            Atomicity::NonTransactional
        } else if mutation {
            Atomicity::Transactional
        } else {
            Atomicity::ReadOnly
        },
        implicit_commit,
        confirmation_required: mutation || operation == Operation::Analyze,
        rebuild_supported: matches!(
            operation,
            Operation::ObjectEdit | Operation::SchemaDdl | Operation::SchemaComparison
        ),
    })
}

fn tool_support(engine: Engine, operation: Operation) -> ToolSupport {
    let tools = &engine.descriptor().tools;
    match operation {
        Operation::SchemaInspection
        | Operation::SchemaDdl
        | Operation::SchemaComparison
        | Operation::NamespaceDdl => tools.schema,
        Operation::ServerMetrics => tools.metrics,
        Operation::Explain | Operation::Analyze => tools.explain,
        _ => tools.administration,
    }
}

pub(crate) fn server(
    engine: Engine,
    operation: Operation,
) -> Result<ServerKind, UnsupportedOperation> {
    match tool_support(engine, operation) {
        ToolSupport::Supported(ToolProvider::Postgres) => Ok(ServerKind::Postgres),
        ToolSupport::Supported(ToolProvider::MySql) => Ok(ServerKind::MySql),
        ToolSupport::Supported(_) => Err(UnsupportedOperation {
            engine,
            operation,
            reason: "The advertised tool provider has no installed admin implementation",
        }),
        ToolSupport::Unsupported(reason) => Err(UnsupportedOperation {
            engine,
            operation,
            reason,
        }),
    }
}
