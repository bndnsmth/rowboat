//! Account, role, membership and privilege SQL builders. Never execute these
//! plans automatically. Plans containing passwords must not be logged/stored.
use super::{Operation, operation_support};
use crate::ddl::{DdlPlan, validate_name, validate_table};
use anyhow::{Result, ensure};
use rowboat_core::{
    Cell, Engine, QueryResult, QuerySession, TableRef,
    sql::{literal, qualified_table, quote_ident},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MySqlAccount {
    pub user: String,
    pub host: String,
}
impl MySqlAccount {
    pub fn sql(&self) -> Result<String> {
        validate_name(&self.user)?;
        validate_name(&self.host)?;
        Ok(format!(
            "{}@{}",
            quote_ident(Engine::MySql, &self.user),
            quote_ident(Engine::MySql, &self.host)
        ))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MySqlStringMode {
    BackslashEscapes,
    NoBackslashEscapes,
}
fn mysql_string(value: &str, mode: MySqlStringMode) -> Result<String> {
    ensure!(!value.contains('\0'), "Password cannot contain NUL");
    let value = if mode == MySqlStringMode::BackslashEscapes {
        value.replace('\\', "\\\\")
    } else {
        value.to_owned()
    };
    Ok(format!("'{}'", value.replace('\'', "''")))
}
/// Deliberately not Debug/Serialize: contains plaintext passwords.
#[derive(Deserialize)]
pub enum MySqlUserOperation {
    AlterPolicy {
        account: MySqlAccount,
        policy: MySqlAccountPolicy,
    },
    Create {
        account: MySqlAccount,
        password: String,
        locked: bool,
    },
    SetPassword {
        account: MySqlAccount,
        password: String,
    },
    SetLocked {
        account: MySqlAccount,
        locked: bool,
    },
    Rename {
        account: MySqlAccount,
        new_account: MySqlAccount,
    },
    Drop {
        account: MySqlAccount,
    },
}
pub fn mysql_user_sql(op: &MySqlUserOperation, mode: MySqlStringMode) -> Result<DdlPlan> {
    use MySqlUserOperation::*;
    let mut plan = DdlPlan::default();
    match op {
        AlterPolicy { account, policy } => plan.push(
            format!("ALTER USER {} {}", account.sql()?, policy.sql()?),
            true,
            "Change account policy/resource limits; can prevent login",
        ),
        Create {
            account,
            password,
            locked,
        } => plan.push(
            format!(
                "CREATE USER {} IDENTIFIED BY {} ACCOUNT {}",
                account.sql()?,
                mysql_string(password, mode)?,
                if *locked { "LOCK" } else { "UNLOCK" }
            ),
            false,
            "Create MySQL user (contains password)",
        ),
        SetPassword { account, password } => plan.push(
            format!(
                "ALTER USER {} IDENTIFIED BY {}",
                account.sql()?,
                mysql_string(password, mode)?
            ),
            true,
            "Replace MySQL password (contains password)",
        ),
        SetLocked { account, locked } => plan.push(
            format!(
                "ALTER USER {} ACCOUNT {}",
                account.sql()?,
                if *locked { "LOCK" } else { "UNLOCK" }
            ),
            *locked,
            "Change account lock",
        ),
        Rename {
            account,
            new_account,
        } => plan.push(
            format!("RENAME USER {} TO {}", account.sql()?, new_account.sql()?),
            false,
            "Rename MySQL account",
        ),
        Drop { account } => plan.push(
            format!("DROP USER {}", account.sql()?),
            true,
            "Drop MySQL account",
        ),
    }
    plan.warnings.push("Account DDL implicitly commits. Password quoting must match the execution connection's NO_BACKSLASH_ESCAPES sql_mode. Do not persist or log password SQL.".into());
    Ok(plan)
}
pub async fn mysql_grants(
    db: &impl QuerySession,
    account: &MySqlAccount,
) -> Result<Vec<QueryResult>> {
    operation_support(db.engine(), Operation::AccessManagement)?;
    ensure!(db.engine() == Engine::MySql, "SHOW GRANTS requires MySQL");
    crate::objects::text_results(
        db.query(&format!("SHOW GRANTS FOR {}", account.sql()?), 10_000)
            .await?,
    )
}
pub async fn mysql_sql_mode(db: &impl QuerySession) -> Result<Vec<QueryResult>> {
    operation_support(db.engine(), Operation::AccessManagement)?;
    ensure!(db.engine() == Engine::MySql, "sql_mode requires MySQL");
    db.query("SELECT @@session.sql_mode AS sql_mode", 1).await
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgRoleOptions {
    pub login: bool,
    pub superuser: bool,
    pub create_database: bool,
    pub create_role: bool,
    pub inherit: bool,
    pub replication: bool,
    pub bypass_rls: bool,
    pub connection_limit: Option<i32>,
}
impl Default for PgRoleOptions {
    fn default() -> Self {
        Self {
            login: false,
            superuser: false,
            create_database: false,
            create_role: false,
            inherit: true,
            replication: false,
            bypass_rls: false,
            connection_limit: None,
        }
    }
}
impl PgRoleOptions {
    fn sql(&self) -> Result<String> {
        let pairs = [
            (self.login, "LOGIN"),
            (self.superuser, "SUPERUSER"),
            (self.create_database, "CREATEDB"),
            (self.create_role, "CREATEROLE"),
            (self.inherit, "INHERIT"),
            (self.replication, "REPLICATION"),
            (self.bypass_rls, "BYPASSRLS"),
        ];
        let mut sql = pairs
            .into_iter()
            .map(|(yes, word)| format!("{}{word}", if yes { "" } else { "NO" }))
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(n) = self.connection_limit {
            ensure!(n >= -1, "Connection limit must be -1 or nonnegative");
            sql.push_str(&format!(" CONNECTION LIMIT {n}"));
        }
        Ok(sql)
    }
}
/// Deliberately not Debug/Serialize: may contain plaintext passwords.
#[derive(Deserialize)]
pub enum PgRoleOperation {
    Patch {
        name: String,
        options: PgRolePatch,
    },
    SetPassword {
        name: String,
        password: String,
    },
    Create {
        name: String,
        options: PgRoleOptions,
        password: Option<String>,
    },
    Alter {
        name: String,
        options: PgRoleOptions,
        password: Option<String>,
    },
    ClearPassword {
        name: String,
    },
    Rename {
        name: String,
        new_name: String,
    },
    Drop {
        name: String,
    },
    GrantMembership {
        role: String,
        member: String,
        admin_option: bool,
    },
    RevokeMembership {
        role: String,
        member: String,
    },
}
pub fn pg_role_sql(op: &PgRoleOperation) -> Result<DdlPlan> {
    use PgRoleOperation::*;
    let ident = |n: &str| -> Result<String> {
        validate_name(n)?;
        Ok(quote_ident(Engine::Postgres, n))
    };
    let mut plan = DdlPlan::default();
    match op {
        Patch { name, options } => plan.push(
            format!("ALTER ROLE {} WITH {}", ident(name)?, options.sql()?),
            true,
            "Change only selected PostgreSQL role attributes",
        ),
        SetPassword { name, password } => {
            ensure!(!password.contains('\0'), "Password cannot contain NUL");
            plan.push(
                format!(
                    "ALTER ROLE {} PASSWORD {}",
                    ident(name)?,
                    literal(Engine::Postgres, &Cell::Text(password.clone()))
                ),
                true,
                "Replace PostgreSQL password (contains password)",
            );
        }
        Create {
            name,
            options,
            password,
        }
        | Alter {
            name,
            options,
            password,
        } => {
            if let Some(p) = password {
                ensure!(!p.contains('\0'), "Password cannot contain NUL");
            }
            let password = password
                .as_ref()
                .map(|p| {
                    format!(
                        " PASSWORD {}",
                        literal(Engine::Postgres, &Cell::Text(p.clone()))
                    )
                })
                .unwrap_or_default();
            plan.push(
                format!(
                    "{} ROLE {} WITH {}{}",
                    if matches!(op, Create { .. }) {
                        "CREATE"
                    } else {
                        "ALTER"
                    },
                    ident(name)?,
                    options.sql()?,
                    password
                ),
                matches!(op, Alter { .. }),
                "Set PostgreSQL role attributes (may contain password)",
            );
        }
        ClearPassword { name } => plan.push(
            format!("ALTER ROLE {} PASSWORD NULL", ident(name)?),
            true,
            "Remove role password",
        ),
        Rename { name, new_name } => plan.push(
            format!("ALTER ROLE {} RENAME TO {}", ident(name)?, ident(new_name)?),
            false,
            "Rename role",
        ),
        Drop { name } => plan.push(
            format!("DROP ROLE {}", ident(name)?),
            true,
            "Drop role; owned objects/dependencies must be handled explicitly",
        ),
        GrantMembership {
            role,
            member,
            admin_option,
        } => plan.push(
            format!(
                "GRANT {} TO {}{}",
                ident(role)?,
                ident(member)?,
                if *admin_option {
                    " WITH ADMIN OPTION"
                } else {
                    ""
                }
            ),
            false,
            "Grant role membership",
        ),
        RevokeMembership { role, member } => plan.push(
            format!("REVOKE {} FROM {}", ident(role)?, ident(member)?),
            true,
            "Revoke role membership",
        ),
    }
    if matches!(
        op,
        SetPassword { .. }
            | Create {
                password: Some(_),
                ..
            }
            | Alter {
                password: Some(_),
                ..
            }
    ) {
        plan.warnings.push(
            "Contains a plaintext password SQL literal: do not persist or log this preview.".into(),
        );
    }
    Ok(plan)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Privilege {
    All,
    Select,
    Insert,
    Update,
    Delete,
    Truncate,
    Maintain,
    References,
    Trigger,
    Usage,
    Create,
    Connect,
    Temporary,
    Execute,
    Alter,
    Drop,
    Index,
    CreateView,
    ShowView,
    CreateRoutine,
    AlterRoutine,
    CreateUser,
    Process,
    Reload,
    Event,
    LockTables,
    CreateTemporaryTables,
}
impl Privilege {
    pub(crate) fn sql(self) -> &'static str {
        match self {
            Self::All => "ALL PRIVILEGES",
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::Truncate => "TRUNCATE",
            Self::Maintain => "MAINTAIN",
            Self::References => "REFERENCES",
            Self::Trigger => "TRIGGER",
            Self::Usage => "USAGE",
            Self::Create => "CREATE",
            Self::Connect => "CONNECT",
            Self::Temporary => "TEMPORARY",
            Self::Execute => "EXECUTE",
            Self::Alter => "ALTER",
            Self::Drop => "DROP",
            Self::Index => "INDEX",
            Self::CreateView => "CREATE VIEW",
            Self::ShowView => "SHOW VIEW",
            Self::CreateRoutine => "CREATE ROUTINE",
            Self::AlterRoutine => "ALTER ROUTINE",
            Self::CreateUser => "CREATE USER",
            Self::Process => "PROCESS",
            Self::Reload => "RELOAD",
            Self::Event => "EVENT",
            Self::LockTables => "LOCK TABLES",
            Self::CreateTemporaryTables => "CREATE TEMPORARY TABLES",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GrantTarget {
    Global,
    Database(String),
    Schema(String),
    Table(TableRef),
    Sequence(TableRef),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Grantee {
    PgRole(String),
    MySqlAccount(MySqlAccount),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantOperation {
    pub target: GrantTarget,
    pub grantee: Grantee,
    pub privileges: Vec<Privilege>,
    pub revoke: bool,
    pub grant_option: bool,
}

pub fn grant_sql(engine: Engine, op: &GrantOperation) -> Result<DdlPlan> {
    operation_support(engine, Operation::AccessManagement)?;
    grant_sql_with_mysql_mode(engine, op, MySqlDatabaseGrantMode::WildcardPatterns)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MySqlDatabaseGrantMode {
    /// partial_revokes=OFF: escape wildcard bytes in literal database names.
    WildcardPatterns,
    /// partial_revokes=ON: database names are already literal.
    LiteralNames,
}
pub fn grant_sql_with_mysql_mode(
    engine: Engine,
    op: &GrantOperation,
    mode: MySqlDatabaseGrantMode,
) -> Result<DdlPlan> {
    operation_support(engine, Operation::AccessManagement)?;
    use GrantTarget::*;
    use Privilege as P;
    ensure!(
        !op.privileges.is_empty(),
        "At least one privilege is required"
    );
    ensure!(
        !(op.revoke && op.grant_option),
        "Use a separate explicit grant-option revocation; grant_option is only valid for GRANT"
    );
    ensure!(
        op.privileges.len() == 1 || !op.privileges.contains(&P::All),
        "ALL cannot be combined with individual privileges"
    );
    let ident = |n: &str| -> Result<String> {
        validate_name(n)?;
        Ok(quote_ident(engine, n))
    };
    let target = match (&op.target, engine) {
        (Global, Engine::MySql) => "*.*".into(),
        (Database(n) | Schema(n), Engine::MySql) => {
            validate_name(n)?;
            let name = if mode == MySqlDatabaseGrantMode::WildcardPatterns {
                n.replace('\\', "\\\\")
                    .replace('_', "\\_")
                    .replace('%', "\\%")
            } else {
                n.clone()
            };
            format!("{}.*", ident(&name)?)
        }
        (Table(t), Engine::MySql) => {
            validate_table(t)?;
            ensure!(
                !t.schema.is_empty(),
                "MySQL grants require an explicit database"
            );
            qualified_table(engine, t)
        }
        (Database(n), Engine::Postgres) => format!("DATABASE {}", ident(n)?),
        (Schema(n), Engine::Postgres) => format!("SCHEMA {}", ident(n)?),
        (Table(t), Engine::Postgres) => {
            validate_table(t)?;
            format!("TABLE {}", qualified_table(engine, t))
        }
        (Sequence(t), Engine::Postgres) => {
            validate_table(t)?;
            format!("SEQUENCE {}", qualified_table(engine, t))
        }
        _ => anyhow::bail!("Grant target unsupported by this engine"),
    };
    for p in &op.privileges {
        let valid = *p == P::All
            || match (engine, &op.target) {
                (Engine::Postgres, Database(_)) => {
                    matches!(p, P::Create | P::Connect | P::Temporary)
                }
                (Engine::Postgres, Schema(_)) => matches!(p, P::Create | P::Usage),
                (Engine::Postgres, Table(_)) => matches!(
                    p,
                    P::Select
                        | P::Insert
                        | P::Update
                        | P::Delete
                        | P::Truncate
                        | P::Maintain
                        | P::References
                        | P::Trigger
                ),
                (Engine::Postgres, Sequence(_)) => matches!(p, P::Usage | P::Select | P::Update),
                (Engine::MySql, Global) => {
                    !matches!(p, P::Connect | P::Temporary | P::Truncate | P::Maintain)
                }
                (Engine::MySql, Database(_) | Schema(_)) => matches!(
                    p,
                    P::Select
                        | P::Insert
                        | P::Update
                        | P::Delete
                        | P::References
                        | P::Trigger
                        | P::Create
                        | P::Execute
                        | P::Alter
                        | P::Drop
                        | P::Index
                        | P::CreateView
                        | P::ShowView
                        | P::CreateRoutine
                        | P::AlterRoutine
                        | P::Event
                        | P::LockTables
                        | P::CreateTemporaryTables
                ),
                (Engine::MySql, Table(_)) => matches!(
                    p,
                    P::Select
                        | P::Insert
                        | P::Update
                        | P::Delete
                        | P::References
                        | P::Trigger
                        | P::Create
                        | P::Alter
                        | P::Drop
                        | P::Index
                        | P::CreateView
                        | P::ShowView
                ),
                _ => false,
            };
        ensure!(
            valid,
            "Privilege {} is invalid for this grant target",
            p.sql()
        );
    }
    let grantee = match (&op.grantee, engine) {
        (Grantee::PgRole(n), Engine::Postgres) => ident(n)?,
        (Grantee::MySqlAccount(a), Engine::MySql) => a.sql()?,
        _ => anyhow::bail!("Grantee does not match engine"),
    };
    let privileges = op
        .privileges
        .iter()
        .map(|p| p.sql())
        .collect::<Vec<_>>()
        .join(", ");
    let mut plan = DdlPlan::default();
    plan.push(
        format!(
            "{} {privileges} ON {target} {} {grantee}{}",
            if op.revoke { "REVOKE" } else { "GRANT" },
            if op.revoke { "FROM" } else { "TO" },
            if op.grant_option {
                " WITH GRANT OPTION"
            } else {
                ""
            }
        ),
        op.revoke,
        "Change object privileges",
    );
    if engine == Engine::MySql {
        plan.warnings.push(
            "MySQL GRANT/REVOKE implicitly commits and is not transactionally rollbackable.".into(),
        );
        if matches!(op.target, Database(_) | Schema(_)) {
            plan.warnings.push(format!("Database-name quoting assumes partial_revokes={}; choose grant_sql_with_mysql_mode to match the server.",if mode==MySqlDatabaseGrantMode::WildcardPatterns{"OFF"}else{"ON"}));
        }
    }
    Ok(plan)
}

/// Column privileges use the same validated exact table/grantee builder.
/// GRANT OPTION remains a table-level authority in MySQL, never a column grant.
pub fn column_grant_sql(
    engine: Engine,
    op: &GrantOperation,
    columns: &[String],
) -> Result<DdlPlan> {
    operation_support(engine, Operation::AccessManagement)?;
    ensure!(
        matches!(op.target, GrantTarget::Table(_)),
        "Column grants require a table target"
    );
    ensure!(
        !columns.is_empty() && columns.len() <= 256,
        "Select 1–256 columns"
    );
    for column in columns {
        validate_name(column)?;
    }
    ensure!(
        op.privileges.iter().all(|p| matches!(
            p,
            Privilege::Select | Privilege::Insert | Privilege::Update | Privilege::References
        )),
        "Invalid column privilege"
    );
    let mut plan = grant_sql(engine, op)?;
    let columns = columns
        .iter()
        .map(|c| quote_ident(engine, c))
        .collect::<Vec<_>>()
        .join(", ");
    let privileges = op
        .privileges
        .iter()
        .map(|p| format!("{} ({columns})", p.sql()))
        .collect::<Vec<_>>()
        .join(", ");
    let statement = &mut plan.statements[0];
    let (_, suffix) = statement
        .sql
        .split_once(" ON ")
        .expect("validated grant builder ON clause");
    statement.sql = format!(
        "{} {privileges} ON {suffix}",
        if op.revoke { "REVOKE" } else { "GRANT" }
    );
    if engine == Engine::MySql && op.grant_option {
        plan.warnings.push(
            "MySQL WITH GRANT OPTION applies at table scope, not just to the selected columns."
                .into(),
        );
    }
    Ok(plan)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MySqlResourceLimits {
    pub max_queries_per_hour: Option<u32>,
    pub max_updates_per_hour: Option<u32>,
    pub max_connections_per_hour: Option<u32>,
    pub max_user_connections: Option<u32>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MySqlPasswordExpiration {
    ExpireNow,
    Default,
    Never,
    IntervalDays(u16),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MySqlPasswordLockTime {
    Days(u16),
    Unbounded,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MySqlAccountPolicy {
    pub resources: MySqlResourceLimits,
    pub password_expiration: Option<MySqlPasswordExpiration>,
    pub failed_login_attempts: Option<u16>,
    pub password_lock_time: Option<MySqlPasswordLockTime>,
    pub locked: Option<bool>,
}
impl MySqlAccountPolicy {
    fn sql(&self) -> Result<String> {
        let mut parts = Vec::new();
        let resources = [
            ("MAX_QUERIES_PER_HOUR", self.resources.max_queries_per_hour),
            ("MAX_UPDATES_PER_HOUR", self.resources.max_updates_per_hour),
            (
                "MAX_CONNECTIONS_PER_HOUR",
                self.resources.max_connections_per_hour,
            ),
            ("MAX_USER_CONNECTIONS", self.resources.max_user_connections),
        ]
        .into_iter()
        .filter_map(|(name, v)| v.map(|v| format!("{name} {v}")))
        .collect::<Vec<_>>();
        if !resources.is_empty() {
            parts.push(format!("WITH {}", resources.join(" ")));
        }
        if let Some(expiry) = &self.password_expiration {
            parts.push(match expiry {
                MySqlPasswordExpiration::ExpireNow => "PASSWORD EXPIRE".into(),
                MySqlPasswordExpiration::Default => "PASSWORD EXPIRE DEFAULT".into(),
                MySqlPasswordExpiration::Never => "PASSWORD EXPIRE NEVER".into(),
                MySqlPasswordExpiration::IntervalDays(n) => {
                    ensure!(*n > 0, "Password expiry interval must be positive");
                    format!("PASSWORD EXPIRE INTERVAL {n} DAY")
                }
            });
        }
        if let Some(n) = self.failed_login_attempts {
            ensure!(n <= 32767, "Failed-login attempts exceeds MySQL limit");
            parts.push(format!("FAILED_LOGIN_ATTEMPTS {n}"));
        }
        if let Some(time) = &self.password_lock_time {
            parts.push(format!(
                "PASSWORD_LOCK_TIME {}",
                match time {
                    MySqlPasswordLockTime::Unbounded => "UNBOUNDED".into(),
                    MySqlPasswordLockTime::Days(n) => {
                        ensure!(*n <= 32767, "Password lock time exceeds MySQL limit");
                        n.to_string()
                    }
                }
            ));
        }
        if let Some(locked) = self.locked {
            parts.push(format!(
                "ACCOUNT {}",
                if locked { "LOCK" } else { "UNLOCK" }
            ));
        }
        ensure!(!parts.is_empty(), "Account policy has no selected changes");
        Ok(parts.join(" "))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgRolePatch {
    pub login: Option<bool>,
    pub superuser: Option<bool>,
    pub create_database: Option<bool>,
    pub create_role: Option<bool>,
    pub inherit: Option<bool>,
    pub replication: Option<bool>,
    pub bypass_rls: Option<bool>,
    pub connection_limit: Option<i32>,
    /// PostgreSQL timestamp input, or "infinity"; always rendered as a literal.
    pub valid_until: Option<String>,
}
impl PgRolePatch {
    fn sql(&self) -> Result<String> {
        let pairs = [
            (self.login, "LOGIN"),
            (self.superuser, "SUPERUSER"),
            (self.create_database, "CREATEDB"),
            (self.create_role, "CREATEROLE"),
            (self.inherit, "INHERIT"),
            (self.replication, "REPLICATION"),
            (self.bypass_rls, "BYPASSRLS"),
        ];
        let mut parts = pairs
            .into_iter()
            .filter_map(|(v, name)| v.map(|yes| format!("{}{name}", if yes { "" } else { "NO" })))
            .collect::<Vec<_>>();
        if let Some(n) = self.connection_limit {
            ensure!(n >= -1, "Connection limit must be -1 or nonnegative");
            parts.push(format!("CONNECTION LIMIT {n}"));
        }
        if let Some(value) = &self.valid_until {
            ensure!(
                !value.is_empty() && !value.contains('\0'),
                "Invalid validity timestamp"
            );
            parts.push(format!(
                "VALID UNTIL {}",
                literal(Engine::Postgres, &Cell::Text(value.clone()))
            ));
        }
        ensure!(!parts.is_empty(), "Role patch has no selected changes");
        Ok(parts.join(" "))
    }
}

pub async fn mysql_account_info(
    db: &impl QuerySession,
    account: &MySqlAccount,
) -> Result<Vec<QueryResult>> {
    operation_support(db.engine(), Operation::AccessManagement)?;
    ensure!(
        db.engine() == Engine::MySql,
        "Account inspection requires MySQL"
    );
    account.sql()?;
    crate::objects::text_results(db.query(&format!("SELECT User AS user, Host AS host, plugin, account_locked, password_expired, password_lifetime, max_questions, max_updates, max_connections, max_user_connections, JSON_UNQUOTE(JSON_EXTRACT(User_attributes,'$.Password_locking.failed_login_attempts')) AS failed_login_attempts, JSON_UNQUOTE(JSON_EXTRACT(User_attributes,'$.Password_locking.password_lock_time_days')) AS password_lock_time_days FROM mysql.user WHERE User={} AND Host={}",literal(Engine::MySql,&Cell::Text(account.user.clone())),literal(Engine::MySql,&Cell::Text(account.host.clone()))),10).await?)
}
pub async fn pg_role_info(db: &impl QuerySession, role: &str) -> Result<Vec<QueryResult>> {
    operation_support(db.engine(), Operation::AccessManagement)?;
    ensure!(
        db.engine() == Engine::Postgres,
        "Role inspection requires PostgreSQL"
    );
    validate_name(role)?;
    db.query(&format!("SELECT rolname, rolcanlogin, rolsuper, rolcreatedb, rolcreaterole, rolinherit, rolreplication, rolbypassrls, rolconnlimit, rolvaliduntil FROM pg_roles WHERE rolname={}",literal(Engine::Postgres,&Cell::Text(role.into()))),10).await
}
pub async fn pg_role_memberships(db: &impl QuerySession, role: &str) -> Result<Vec<QueryResult>> {
    operation_support(db.engine(), Operation::AccessManagement)?;
    ensure!(
        db.engine() == Engine::Postgres,
        "Membership inspection requires PostgreSQL"
    );
    validate_name(role)?;
    db.query(&format!("SELECT r.rolname AS granted_role, m.rolname AS member, g.rolname AS grantor, a.admin_option FROM pg_auth_members a JOIN pg_roles r ON r.oid=a.roleid JOIN pg_roles m ON m.oid=a.member JOIN pg_roles g ON g.oid=a.grantor WHERE m.rolname={} ORDER BY r.rolname",literal(Engine::Postgres,&Cell::Text(role.into()))),10_000).await
}
/// Direct ACL grants plus PUBLIC grants; no claim to compute effective inherited
/// access, ownership, RLS, column ACLs or default privileges for future objects.
pub async fn pg_grants(db: &impl QuerySession, role: &str) -> Result<Vec<QueryResult>> {
    operation_support(db.engine(), Operation::AccessManagement)?;
    ensure!(
        db.engine() == Engine::Postgres,
        "Grant inspection requires PostgreSQL"
    );
    validate_name(role)?;
    // Administrative leases accept SELECT/SHOW inspection. Keep the fixed ACL
    // CTE inside a SELECT, preserving columns and explicit outer ordering.
    let sql = format!(
        r#"SELECT * FROM (WITH objects AS (
      SELECT 'DATABASE'::text AS kind, NULL::text AS schema_name, datname::text AS object_name, datdba AS owner, COALESCE(datacl,acldefault('d',datdba)) AS acl FROM pg_database
      UNION ALL SELECT 'SCHEMA', NULL, nspname, nspowner, COALESCE(nspacl,acldefault('n',nspowner)) FROM pg_namespace
      UNION ALL SELECT CASE WHEN c.relkind='S' THEN 'SEQUENCE' ELSE 'TABLE' END, n.nspname, c.relname, c.relowner, COALESCE(c.relacl,acldefault(CASE WHEN c.relkind='S' THEN 'S'::"char" ELSE 'r'::"char" END,c.relowner)) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','p','v','m','f','S')
      UNION ALL SELECT 'ROUTINE', n.nspname, p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')', p.proowner, COALESCE(p.proacl,acldefault('f',p.proowner)) FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
    ) SELECT o.kind, o.schema_name, o.object_name, pg_get_userbyid(a.grantor) AS grantor, CASE WHEN a.grantee=0 THEN 'PUBLIC' ELSE pg_get_userbyid(a.grantee) END AS grantee, a.privilege_type, a.is_grantable FROM objects o CROSS JOIN LATERAL aclexplode(o.acl) a WHERE a.grantee=0 OR a.grantee=(SELECT oid FROM pg_roles WHERE rolname={})) AS rowboat_grants ORDER BY 1,2,3,6"#,
        literal(Engine::Postgres, &Cell::Text(role.into()))
    );
    db.query(&sql, 100_000).await
}
