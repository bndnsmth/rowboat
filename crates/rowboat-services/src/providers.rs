//! Optional services available on top of core engine capabilities.
use rowboat_core::{ConnectionKind, DialectFamily, Engine, ToolProvider, ToolSupport};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceOperation {
    Backup,
    Restore,
    SshTunnel,
    ConnectionUrl,
    FormatSql,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedServiceOperation {
    pub engine: Engine,
    pub operation: ServiceOperation,
    pub reason: &'static str,
}

impl fmt::Display for UnsupportedServiceOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {:?}: {}", self.engine, self.operation, self.reason)
    }
}

impl std::error::Error for UnsupportedServiceOperation {}

/// Existing native-client image for supported backup providers; never pulls it.
pub fn default_backup_image(engine: Engine) -> anyhow::Result<&'static str> {
    Ok(
        match NetworkProvider::for_engine(engine, ServiceOperation::Backup)? {
            NetworkProvider::Postgres => "docker.io/library/postgres:18",
            NetworkProvider::MySql => "docker.io/library/mysql:8.4",
        },
    )
}

pub fn service_support(
    engine: Engine,
    operation: ServiceOperation,
) -> Result<(), UnsupportedServiceOperation> {
    let descriptor = engine.descriptor();
    let reason = match operation {
        ServiceOperation::Backup | ServiceOperation::Restore => match descriptor.tools.backup {
            ToolSupport::Supported(ToolProvider::Postgres | ToolProvider::MySql) => return Ok(()),
            ToolSupport::Supported(ToolProvider::Sqlite) => return Ok(()),
            ToolSupport::Unsupported(reason) => reason,
        },
        ServiceOperation::SshTunnel if descriptor.connection_kind == ConnectionKind::File => {
            "File connections do not support SSH or network transport overrides"
        }
        ServiceOperation::ConnectionUrl if descriptor.connection_kind == ConnectionKind::File => {
            "SQLite connection URLs are unsupported; use a typed file profile or portable JSON with an explicit open mode"
        }
        _ => return Ok(()),
    };
    Err(UnsupportedServiceOperation {
        engine,
        operation,
        reason,
    })
}

pub(crate) fn text_allows_nul(engine: Engine) -> bool {
    match engine.descriptor().dialect.family {
        DialectFamily::PostgreSql => false,
        DialectFamily::MySql | DialectFamily::Sqlite => true,
    }
}

/// Concrete native/network protocols. File engines cannot reach these clients.
#[derive(Clone, Copy)]
pub(crate) enum NetworkProvider {
    Postgres,
    MySql,
}

impl NetworkProvider {
    pub(crate) fn for_engine(engine: Engine, operation: ServiceOperation) -> anyhow::Result<Self> {
        service_support(engine, operation)?;
        Ok(match engine.descriptor().dialect.family {
            DialectFamily::PostgreSql => Self::Postgres,
            DialectFamily::MySql => Self::MySql,
            DialectFamily::Sqlite => {
                return Err(UnsupportedServiceOperation {
                    engine,
                    operation,
                    reason: "This service requires a network database provider",
                }
                .into());
            }
        })
    }

    pub(crate) fn default_port(self) -> u16 {
        match self {
            Self::Postgres => 5432,
            Self::MySql => 3306,
        }
    }

    pub(crate) fn scheme(self) -> &'static str {
        match self {
            Self::Postgres => "postgresql",
            Self::MySql => "mysql",
        }
    }

    pub(crate) fn tls_keys(self) -> [&'static str; 3] {
        match self {
            Self::Postgres => ["sslrootcert", "sslcert", "sslkey"],
            Self::MySql => ["ssl-ca", "ssl-cert", "ssl-key"],
        }
    }

    pub(crate) fn socket_name(self, port: u16) -> String {
        match self {
            Self::Postgres => format!(".s.PGSQL.{port}"),
            Self::MySql => "database.sock".into(),
        }
    }
}
