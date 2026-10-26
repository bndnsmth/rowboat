//! Review-only management model and off-thread execution. Never persisted or logged.
use anyhow::{Result, bail, ensure};
use rowboat_admin::{
    access::*,
    ddl::*,
    objects::{self, NamespaceOperation},
};
use rowboat_core::{
    Cell, ConnectionProfile, Database, Engine, QueryResult, Structure, TableInfo, TableRef,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub session: u64,
    pub connection: String,
    pub database: String,
    pub engine: Engine,
    pub table: Option<TableRef>,
}
impl Target {
    pub fn matches(
        &self,
        session: u64,
        profile: &ConnectionProfile,
        table: Option<&TableRef>,
    ) -> bool {
        self.session == session
            && self.connection == profile.id
            && self.database == target_database(profile)
            && self.engine == profile.engine
            && (self.table.is_none() || self.table.as_ref() == table)
    }
    pub fn label(&self) -> String {
        match &self.table {
            Some(t) => format!("{} / {}.{}", self.database, t.schema, t.name),
            None => self.database.clone(),
        }
    }
}

pub fn target_database(profile: &ConnectionProfile) -> String {
    if profile.engine == Engine::Sqlite {
        profile
            .file
            .as_ref()
            .map(|f| f.path.display().to_string())
            .unwrap_or_default()
    } else {
        profile.database.clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    CreateTable,
    SetDefault,
    SetNullable,
    SetType,
    AddCheck,
    DropCheck,
    AddUnique,
    DropUnique,
    InspectConstraints,
    AccountLimits,
    AddColumn,
    AlterColumn,
    RenameColumn,
    DropColumn,
    CreateIndex,
    CreateUniqueIndex,
    RenameIndex,
    DropIndex,
    AddPrimaryKey,
    DropPrimaryKey,
    AddForeignKey,
    DropForeignKey,
    RenameTable,
    DropTable,
    CreateDatabase,
    DropDatabase,
    CreateSchema,
    RenameSchema,
    DropSchema,
    CreateAccount,
    AlterRole,
    SetPassword,
    ClearPassword,
    LockAccount,
    UnlockAccount,
    RenameAccount,
    DropAccount,
    Grant,
    Revoke,
    GrantMembership,
    RevokeMembership,
    InspectPrivileges,
}
impl Operation {
    pub fn supported(self, engine: Engine) -> bool {
        if rowboat_admin::providers::operation_support(engine, self.provider_operation()).is_err() {
            return false;
        }
        if engine != Engine::Sqlite || self.inspection() {
            return true;
        }
        use rowboat_admin::sqlite_schema::{Action, supported_action};
        let action = match self {
            Self::CreateTable => Action::CreateTable,
            Self::RenameTable => Action::RenameTable,
            Self::DropTable => Action::DropTable,
            Self::AddColumn => Action::AddColumn,
            Self::RenameColumn => Action::RenameColumn,
            Self::DropColumn => Action::DropColumn,
            Self::CreateIndex | Self::CreateUniqueIndex => Action::CreateIndex,
            Self::DropIndex => Action::DropIndex,
            Self::RenameIndex => Action::RenameIndex,
            _ => return false,
        };
        supported_action(action)
    }
    pub fn provider_operation(self) -> rowboat_admin::providers::Operation {
        use rowboat_admin::providers::Operation as Provider;
        if self.access() {
            Provider::AccessManagement
        } else if self.inspection() {
            Provider::SchemaInspection
        } else if matches!(self, Self::CreateDatabase | Self::DropDatabase) {
            Provider::NamespaceDdl
        } else {
            Provider::SchemaDdl
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::CreateTable => "Create table",
            Self::SetDefault => "Set column default",
            Self::SetNullable => "Set column nullability",
            Self::SetType => "Set column type / USING",
            Self::AddCheck => "Add CHECK constraint",
            Self::DropCheck => "Drop CHECK constraint",
            Self::AddUnique => "Add UNIQUE constraint",
            Self::DropUnique => "Drop UNIQUE constraint",
            Self::InspectConstraints => "Inspect constraints",
            Self::AccountLimits => "Account limits / expiry",
            Self::AddColumn => "Add column",
            Self::AlterColumn => "Alter column",
            Self::RenameColumn => "Rename column",
            Self::DropColumn => "Drop column",
            Self::CreateIndex => "Create index",
            Self::CreateUniqueIndex => "Create unique index",
            Self::RenameIndex => "Rename index",
            Self::DropIndex => "Drop index",
            Self::AddPrimaryKey => "Add primary key",
            Self::DropPrimaryKey => "Drop primary key",
            Self::AddForeignKey => "Add foreign key",
            Self::DropForeignKey => "Drop foreign key",
            Self::RenameTable => "Rename table",
            Self::DropTable => "Drop table",
            Self::CreateDatabase => "Create database",
            Self::DropDatabase => "Drop database",
            Self::CreateSchema => "Create schema",
            Self::RenameSchema => "Rename schema",
            Self::DropSchema => "Drop schema",
            Self::CreateAccount => "Create role / user",
            Self::AlterRole => "Alter role attributes",
            Self::SetPassword => "Set password",
            Self::ClearPassword => "Clear role password",
            Self::LockAccount => "Lock account",
            Self::UnlockAccount => "Unlock account",
            Self::RenameAccount => "Rename role / user",
            Self::DropAccount => "Drop role / user",
            Self::Grant => "Grant privileges",
            Self::Revoke => "Revoke privileges",
            Self::GrantMembership => "Grant role membership",
            Self::RevokeMembership => "Revoke role membership",
            Self::InspectPrivileges => "Inspect privileges",
        }
    }
    pub fn table_bound(self) -> bool {
        matches!(
            self,
            Self::SetDefault
                | Self::SetNullable
                | Self::SetType
                | Self::AddCheck
                | Self::DropCheck
                | Self::AddUnique
                | Self::DropUnique
                | Self::InspectConstraints
                | Self::AddColumn
                | Self::AlterColumn
                | Self::RenameColumn
                | Self::DropColumn
                | Self::CreateIndex
                | Self::CreateUniqueIndex
                | Self::RenameIndex
                | Self::DropIndex
                | Self::AddPrimaryKey
                | Self::DropPrimaryKey
                | Self::AddForeignKey
                | Self::DropForeignKey
                | Self::RenameTable
                | Self::DropTable
        )
    }
    pub fn access(self) -> bool {
        matches!(
            self,
            Self::AccountLimits
                | Self::CreateAccount
                | Self::AlterRole
                | Self::SetPassword
                | Self::ClearPassword
                | Self::LockAccount
                | Self::UnlockAccount
                | Self::RenameAccount
                | Self::DropAccount
                | Self::Grant
                | Self::Revoke
                | Self::GrantMembership
                | Self::RevokeMembership
                | Self::InspectPrivileges
        )
    }
    pub fn secret(self) -> bool {
        matches!(self, Self::CreateAccount | Self::SetPassword)
    }
    pub fn inspection(self) -> bool {
        matches!(self, Self::InspectPrivileges | Self::InspectConstraints)
    }
    #[cfg(test)]
    pub fn atomic(self, engine: Engine) -> bool {
        rowboat_admin::providers::operation_support(engine, self.provider_operation())
            .is_ok_and(|s| s.atomicity == rowboat_admin::providers::Atomicity::Transactional)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Field {
    Using,
    PgLogin,
    PgSuperuser,
    PgCreateDb,
    PgCreateRole,
    PgInherit,
    PgReplication,
    PgBypassRls,
    Expression,
    QueriesPerHour,
    UpdatesPerHour,
    ConnectionsPerHour,
    UserConnections,
    ValidUntil,
    Expiration,
    ExpiryDays,
    FailedAttempts,
    LockDuration,
    LockDays,
    Name,
    NewName,
    Type,
    Default,
    DefaultKind,
    Columns,
    RefSchema,
    RefTable,
    RefColumns,
    OnUpdate,
    OnDelete,
    Host,
    NewHost,
    Password,
    Limit,
    Scope,
    Schema,
    Object,
    Privileges,
    Member,
}
impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Self::Using => "USING expression (optional trusted SQL)",
            Self::PgLogin => "LOGIN",
            Self::PgSuperuser => "SUPERUSER",
            Self::PgCreateDb => "CREATEDB",
            Self::PgCreateRole => "CREATEROLE",
            Self::PgInherit => "INHERIT",
            Self::PgReplication => "REPLICATION",
            Self::PgBypassRls => "BYPASSRLS",
            Self::Expression => "CHECK expression (trusted SQL)",
            Self::QueriesPerHour => "Max queries / hour (blank = unchanged; 0 = unlimited)",
            Self::UpdatesPerHour => "Max updates / hour",
            Self::ConnectionsPerHour => "Max connections / hour",
            Self::UserConnections => "Max concurrent user connections",
            Self::ValidUntil => "Password valid until (blank = unchanged; infinity = no expiry)",
            Self::Expiration => "Password expiration",
            Self::ExpiryDays => "Expiration interval (days)",
            Self::FailedAttempts => "Failed-login attempts (blank = unchanged; 0 = disabled)",
            Self::LockDuration => "Temporary lock duration",
            Self::LockDays => "Temporary lock days (0 = disabled)",
            Self::Name => "Name",
            Self::NewName => "New name",
            Self::Type => "Data type",
            Self::Default => "Default value",
            Self::DefaultKind => "Default mode",
            Self::Columns => "Columns (JSON array of names)",
            Self::RefSchema => "Referenced schema / database",
            Self::RefTable => "Referenced table",
            Self::RefColumns => "Referenced columns (JSON array)",
            Self::OnUpdate => "On update",
            Self::OnDelete => "On delete",
            Self::Host => "Account host",
            Self::NewHost => "New account host",
            Self::Password => "Password (memory only)",
            Self::Limit => "Connection limit (-1 = unlimited)",
            Self::Scope => "Privilege scope",
            Self::Schema => "Schema / database",
            Self::Object => "Table / sequence name",
            Self::Privileges => "Privileges",
            Self::Member => "Member role",
        }
    }
    pub fn choices(self, engine: Engine) -> &'static [&'static str] {
        match self {
            Self::PgLogin
            | Self::PgSuperuser
            | Self::PgCreateDb
            | Self::PgCreateRole
            | Self::PgInherit
            | Self::PgReplication
            | Self::PgBypassRls => &["Unchanged", "Enabled", "Disabled"],
            Self::DefaultKind => &["Keep", "None", "Literal", "NULL", "Expression"],
            Self::Expiration => &["Unchanged", "Now", "Default", "Never", "Days"],
            Self::LockDuration => &["Unchanged", "Days", "Unbounded"],
            Self::OnUpdate | Self::OnDelete if engine == Engine::Postgres => &[
                "NO ACTION",
                "RESTRICT",
                "CASCADE",
                "SET NULL",
                "SET DEFAULT",
            ],
            Self::OnUpdate | Self::OnDelete => &["NO ACTION", "RESTRICT", "CASCADE", "SET NULL"],
            Self::Scope if engine == Engine::Postgres => {
                &["Table", "Schema", "Database", "Sequence"]
            }
            Self::Scope => &["Table", "Database", "Global"],
            _ => &[],
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Flag {
    Nullable,
    Cascade,
    Login,
    Superuser,
    CreateDb,
    CreateRole,
    Inherit,
    Replication,
    BypassRls,
    GrantOption,
}
impl Flag {
    pub fn label(self) -> &'static str {
        match self {
            Self::Nullable => "Allow NULL",
            Self::Cascade => "CASCADE (dependent objects)",
            Self::Login => "LOGIN",
            Self::Superuser => "SUPERUSER",
            Self::CreateDb => "CREATEDB",
            Self::CreateRole => "CREATEROLE",
            Self::Inherit => "INHERIT",
            Self::Replication => "REPLICATION",
            Self::BypassRls => "BYPASSRLS",
            Self::GrantOption => "Grant / admin option",
        }
    }
}

// No Debug or serialization: field values can contain credentials.
pub struct Draft {
    pub operation: Operation,
    pub values: BTreeMap<Field, String>,
    pub flags: BTreeSet<Flag>,
}
impl Draft {
    pub fn value(&self, field: Field) -> &str {
        self.values.get(&field).map(String::as_str).unwrap_or("")
    }
    fn text(&self, field: Field) -> String {
        self.value(field).to_owned()
    }
    fn flag(&self, flag: Flag) -> bool {
        self.flags.contains(&flag)
    }
    fn columns(&self, field: Field) -> Result<Vec<String>> {
        serde_json::from_str(self.value(field)).map_err(|_| {
            anyhow::anyhow!(
                "{} must be a JSON array, e.g. [\"id\", \"tenant_id\"]",
                field.label()
            )
        })
    }
    pub fn fields(operation: Operation, engine: Engine) -> Vec<Field> {
        use Field::*;
        use Operation::*;
        let mut fields = match operation {
            SetDefault => vec![Name, DefaultKind, Default],
            SetNullable => vec![Name],
            SetType => vec![Name, Type, Using],
            AddCheck => vec![Name, Expression],
            AddUnique => vec![Name, Columns],
            InspectConstraints => vec![],
            AccountLimits if engine == Engine::Postgres => vec![Name, Limit, ValidUntil],
            AccountLimits => vec![
                Name,
                QueriesPerHour,
                UpdatesPerHour,
                ConnectionsPerHour,
                UserConnections,
                Expiration,
                ExpiryDays,
                FailedAttempts,
                LockDuration,
                LockDays,
            ],
            AddColumn | AlterColumn => vec![Name, Type, DefaultKind, Default],
            RenameColumn | RenameIndex | RenameTable | RenameSchema => vec![Name, NewName],
            CreateIndex | CreateUniqueIndex | AddPrimaryKey => vec![Name, Columns],
            AddForeignKey => vec![
                Name, Columns, RefSchema, RefTable, RefColumns, OnUpdate, OnDelete,
            ],
            DropTable => vec![],
            CreateAccount if engine == Engine::Postgres => vec![Name, Password, Limit],
            CreateAccount | SetPassword => vec![Name, Password],
            AlterRole => vec![
                Name,
                PgLogin,
                PgSuperuser,
                PgCreateDb,
                PgCreateRole,
                PgInherit,
                PgReplication,
                PgBypassRls,
                Limit,
                ValidUntil,
            ],
            RenameAccount => vec![Name, NewName],
            Grant | Revoke => vec![Name, Scope, Schema, Object, Privileges],
            GrantMembership | RevokeMembership => vec![Name, Member],
            _ => vec![Name],
        };
        if engine == Engine::MySql && operation.access() {
            fields.insert(1, Host);
            if operation == RenameAccount {
                fields.push(NewHost);
            }
        }
        // The table is immutable and shown in the dialog heading.
        if operation == RenameTable {
            fields.remove(0);
        }
        fields
    }
    pub fn toggles(operation: Operation, engine: Engine) -> Vec<Flag> {
        use Flag::*;
        match operation {
            Operation::AddColumn | Operation::AlterColumn | Operation::SetNullable => {
                vec![Nullable]
            }
            Operation::DropTable | Operation::DropSchema if engine == Engine::Postgres => {
                vec![Cascade]
            }
            Operation::CreateAccount if engine == Engine::Postgres => vec![
                Login,
                Superuser,
                CreateDb,
                CreateRole,
                Inherit,
                Replication,
                BypassRls,
            ],
            Operation::Grant | Operation::GrantMembership => vec![GrantOption],
            _ => vec![],
        }
    }
    fn ddl_operation(
        &self,
        target: &Target,
        mysql_column: Option<&MySqlColumnDefinition>,
    ) -> Result<Option<DdlOperation>> {
        use Operation::*;
        let engine = target.engine;
        rowboat_admin::providers::operation_support(engine, self.operation.provider_operation())?;
        let name = self.text(Field::Name);
        let new_name = self.text(Field::NewName);
        let table = || {
            target
                .table
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Choose a table first"))
        };
        let column = || -> Result<ColumnDesign> {
            Ok(ColumnDesign { name: name.clone(), data_type: DataType::parse(engine, self.value(Field::Type))?, nullable: self.flag(Flag::Nullable), default: match self.value(Field::DefaultKind) {
                "Keep" => mysql_column.ok_or_else(||anyhow::anyhow!("Keep default is available for existing MySQL columns; select an explicit default mode here"))?.column.default.clone(),
                "None" => None, "NULL" => Some(DefaultValue::Literal(Cell::Null)),
                "Literal" => Some(DefaultValue::Literal(Cell::Text(self.text(Field::Default)))),
                "Expression" => Some(DefaultValue::RawExpression(self.text(Field::Default))),
                _ => bail!("Select a default mode"),
            } })
        };
        let ddl = match self.operation {
            SetDefault => Some(DdlOperation::SetColumnDefault {
                table: table()?,
                name: name.clone(),
                default: match self.value(Field::DefaultKind) {
                    "None" => None,
                    "NULL" => Some(DefaultValue::Literal(Cell::Null)),
                    "Literal" => Some(DefaultValue::Literal(Cell::Text(self.text(Field::Default)))),
                    "Expression" => Some(DefaultValue::RawExpression(self.text(Field::Default))),
                    _ => bail!("Choose an explicit default mode"),
                },
            }),
            SetNullable => Some(DdlOperation::SetColumnNullable {
                table: table()?,
                name: name.clone(),
                nullable: self.flag(Flag::Nullable),
            }),
            SetType => Some(DdlOperation::SetColumnType {
                table: table()?,
                name: name.clone(),
                data_type: DataType::parse(engine, self.value(Field::Type))?,
                using: (!self.value(Field::Using).is_empty()).then(|| self.text(Field::Using)),
            }),
            AddCheck => Some(DdlOperation::AddCheck {
                table: table()?,
                check: CheckDesign {
                    name: name.clone(),
                    expression: self.text(Field::Expression),
                },
            }),
            DropCheck => Some(DdlOperation::DropCheck {
                table: table()?,
                name: name.clone(),
            }),
            AddUnique => Some(DdlOperation::AddUnique {
                table: table()?,
                unique: UniqueDesign {
                    name: name.clone(),
                    columns: self.columns(Field::Columns)?,
                },
            }),
            DropUnique => Some(DdlOperation::DropUnique {
                table: table()?,
                name: name.clone(),
            }),
            AddColumn => Some(DdlOperation::AddColumn {
                table: table()?,
                column: column()?,
            }),
            AlterColumn if engine == Engine::MySql => {
                let mut definition = mysql_column.cloned().ok_or_else(|| {
                    anyhow::anyhow!("Read the complete MySQL column definition before altering it")
                })?;
                definition.column = column()?;
                Some(DdlOperation::ModifyMySqlColumn {
                    table: table()?,
                    definition,
                })
            }
            AlterColumn => Some(DdlOperation::AlterColumn {
                table: table()?,
                column: column()?,
            }),
            RenameColumn => Some(DdlOperation::RenameColumn {
                table: table()?,
                name: name.clone(),
                new_name: new_name.clone(),
            }),
            DropColumn => Some(DdlOperation::DropColumn {
                table: table()?,
                name: name.clone(),
            }),
            CreateIndex | CreateUniqueIndex => Some(DdlOperation::CreateIndex {
                table: table()?,
                index: IndexDesign {
                    name: name.clone(),
                    columns: self.columns(Field::Columns)?,
                    unique: self.operation == CreateUniqueIndex,
                },
            }),
            RenameIndex => Some(DdlOperation::RenameIndex {
                table: table()?,
                name: name.clone(),
                new_name: new_name.clone(),
            }),
            DropIndex => Some(DdlOperation::DropIndex {
                table: table()?,
                name: name.clone(),
            }),
            AddPrimaryKey => Some(DdlOperation::AddPrimaryKey {
                table: table()?,
                primary_key: PrimaryKeyDesign {
                    name: name.clone(),
                    columns: self.columns(Field::Columns)?,
                },
            }),
            DropPrimaryKey => Some(DdlOperation::DropPrimaryKey {
                table: table()?,
                name: name.clone(),
            }),
            AddForeignKey => Some(DdlOperation::AddForeignKey {
                table: table()?,
                foreign_key: ForeignKeyDesign {
                    name: name.clone(),
                    columns: self.columns(Field::Columns)?,
                    referenced_table: TableRef {
                        schema: self.text(Field::RefSchema),
                        name: self.text(Field::RefTable),
                    },
                    referenced_columns: self.columns(Field::RefColumns)?,
                    on_update: referential(self.value(Field::OnUpdate))?,
                    on_delete: referential(self.value(Field::OnDelete))?,
                },
            }),
            DropForeignKey => Some(DdlOperation::DropForeignKey {
                table: table()?,
                name: name.clone(),
            }),
            RenameTable => Some(DdlOperation::RenameTable {
                table: table()?,
                new_name: new_name.clone(),
            }),
            DropTable => Some(DdlOperation::DropTable {
                table: table()?,
                cascade: self.flag(Flag::Cascade),
            }),
            _ => None,
        };
        Ok(ddl)
    }
    fn plan(
        &self,
        target: &Target,
        mode: MySqlStringMode,
        redact: bool,
        mysql_column: Option<&MySqlColumnDefinition>,
    ) -> Result<DdlPlan> {
        use Operation::*;
        let engine = target.engine;
        if let Some(op) = self.ddl_operation(target, mysql_column)? {
            return preview(engine, &[op]);
        }
        let name = self.text(Field::Name);
        let new_name = self.text(Field::NewName);
        let account = || MySqlAccount {
            user: name.clone(),
            host: self.text(Field::Host),
        };
        let password = || {
            if redact {
                "[REDACTED]".into()
            } else {
                self.text(Field::Password)
            }
        };
        let namespace = match self.operation {
            CreateDatabase => Some(NamespaceOperation::CreateDatabase { name: name.clone() }),
            DropDatabase => Some(NamespaceOperation::DropDatabase { name: name.clone() }),
            CreateSchema => Some(NamespaceOperation::CreateSchema { name: name.clone() }),
            RenameSchema => Some(NamespaceOperation::RenameSchema {
                name: name.clone(),
                new_name: new_name.clone(),
            }),
            DropSchema => Some(NamespaceOperation::DropSchema {
                name: name.clone(),
                cascade: self.flag(Flag::Cascade),
            }),
            _ => None,
        };
        if let Some(op) = namespace {
            ensure!(
                !(self.operation == DropDatabase
                    || (engine == Engine::MySql && self.operation == DropSchema))
                    || name != target.database,
                "Connect to a different maintenance database before dropping the connected database"
            );
            return objects::namespace_sql(engine, &op);
        }
        if matches!(self.operation, Grant | Revoke) {
            let scope = match self.value(Field::Scope) {
                "Table" => GrantTarget::Table(TableRef {
                    schema: self.text(Field::Schema),
                    name: self.text(Field::Object),
                }),
                "Sequence" => GrantTarget::Sequence(TableRef {
                    schema: self.text(Field::Schema),
                    name: self.text(Field::Object),
                }),
                "Schema" => GrantTarget::Schema(self.text(Field::Schema)),
                "Database" => GrantTarget::Database(self.text(Field::Schema)),
                "Global" => GrantTarget::Global,
                _ => bail!("Select a privilege scope"),
            };
            return grant_sql(
                engine,
                &GrantOperation {
                    target: scope,
                    grantee: if engine == Engine::Postgres {
                        Grantee::PgRole(name)
                    } else {
                        Grantee::MySqlAccount(account())
                    },
                    privileges: privileges(self.value(Field::Privileges))?,
                    revoke: self.operation == Revoke,
                    grant_option: self.flag(Flag::GrantOption),
                },
            );
        }
        if engine == Engine::MySql {
            return mysql_user_sql(
                &match self.operation {
                    AccountLimits => MySqlUserOperation::AlterPolicy {
                        account: account(),
                        policy: MySqlAccountPolicy {
                            resources: MySqlResourceLimits {
                                max_queries_per_hour: self
                                    .optional_number(Field::QueriesPerHour)?,
                                max_updates_per_hour: self
                                    .optional_number(Field::UpdatesPerHour)?,
                                max_connections_per_hour: self
                                    .optional_number(Field::ConnectionsPerHour)?,
                                max_user_connections: self
                                    .optional_number(Field::UserConnections)?,
                            },
                            password_expiration: match self.value(Field::Expiration) {
                                "Unchanged" => None,
                                "Now" => Some(MySqlPasswordExpiration::ExpireNow),
                                "Default" => Some(MySqlPasswordExpiration::Default),
                                "Never" => Some(MySqlPasswordExpiration::Never),
                                "Days" => Some(MySqlPasswordExpiration::IntervalDays(
                                    self.value(Field::ExpiryDays).parse().map_err(|_| {
                                        anyhow::anyhow!("Enter an expiration interval in days")
                                    })?,
                                )),
                                _ => bail!("Select an expiration mode"),
                            },
                            failed_login_attempts: self
                                .optional_number(Field::FailedAttempts)?
                                .map(u16::try_from)
                                .transpose()?,
                            password_lock_time: match self.value(Field::LockDuration) {
                                "Unchanged" => None,
                                "Unbounded" => Some(MySqlPasswordLockTime::Unbounded),
                                "Days" => Some(MySqlPasswordLockTime::Days(
                                    self.value(Field::LockDays).parse().map_err(|_| {
                                        anyhow::anyhow!("Enter a lock duration in days")
                                    })?,
                                )),
                                _ => bail!("Select a temporary lock duration"),
                            },
                            locked: None,
                        },
                    },
                    CreateAccount => MySqlUserOperation::Create {
                        account: account(),
                        password: password(),
                        locked: false,
                    },
                    SetPassword => MySqlUserOperation::SetPassword {
                        account: account(),
                        password: password(),
                    },
                    LockAccount | UnlockAccount => MySqlUserOperation::SetLocked {
                        account: account(),
                        locked: self.operation == LockAccount,
                    },
                    RenameAccount => MySqlUserOperation::Rename {
                        account: account(),
                        new_account: MySqlAccount {
                            user: new_name,
                            host: self.text(Field::NewHost),
                        },
                    },
                    DropAccount => MySqlUserOperation::Drop { account: account() },
                    _ => bail!("This operation is not available for MySQL"),
                },
                mode,
            );
        }
        let options = || -> Result<PgRoleOptions> {
            Ok(PgRoleOptions {
                login: self.flag(Flag::Login),
                superuser: self.flag(Flag::Superuser),
                create_database: self.flag(Flag::CreateDb),
                create_role: self.flag(Flag::CreateRole),
                inherit: self.flag(Flag::Inherit),
                replication: self.flag(Flag::Replication),
                bypass_rls: self.flag(Flag::BypassRls),
                connection_limit: Some(
                    self.value(Field::Limit)
                        .parse()
                        .map_err(|_| anyhow::anyhow!("Connection limit must be an integer"))?,
                ),
            })
        };
        pg_role_sql(&match self.operation {
            SetPassword => PgRoleOperation::SetPassword {
                name,
                password: password(),
            },
            AccountLimits => {
                PgRoleOperation::Patch {
                    name,
                    options: PgRolePatch {
                        connection_limit: if self.value(Field::Limit).is_empty() {
                            None
                        } else {
                            Some(self.value(Field::Limit).parse().map_err(|_| {
                                anyhow::anyhow!("Connection limit must be an integer")
                            })?)
                        },
                        valid_until: (!self.value(Field::ValidUntil).is_empty())
                            .then(|| self.text(Field::ValidUntil)),
                        ..Default::default()
                    },
                }
            }
            CreateAccount => PgRoleOperation::Create {
                name,
                options: options()?,
                password: (!self.value(Field::Password).is_empty()).then(password),
            },
            AlterRole => {
                PgRoleOperation::Patch {
                    name,
                    options: PgRolePatch {
                        login: self.optional_bool(Field::PgLogin)?,
                        superuser: self.optional_bool(Field::PgSuperuser)?,
                        create_database: self.optional_bool(Field::PgCreateDb)?,
                        create_role: self.optional_bool(Field::PgCreateRole)?,
                        inherit: self.optional_bool(Field::PgInherit)?,
                        replication: self.optional_bool(Field::PgReplication)?,
                        bypass_rls: self.optional_bool(Field::PgBypassRls)?,
                        connection_limit: if self.value(Field::Limit).is_empty() {
                            None
                        } else {
                            Some(self.value(Field::Limit).parse().map_err(|_| {
                                anyhow::anyhow!("Connection limit must be an integer")
                            })?)
                        },
                        valid_until: (!self.value(Field::ValidUntil).is_empty())
                            .then(|| self.text(Field::ValidUntil)),
                    },
                }
            }
            ClearPassword => PgRoleOperation::ClearPassword { name },
            RenameAccount => PgRoleOperation::Rename { name, new_name },
            DropAccount => PgRoleOperation::Drop { name },
            GrantMembership => PgRoleOperation::GrantMembership {
                role: name,
                member: self.text(Field::Member),
                admin_option: self.flag(Flag::GrantOption),
            },
            RevokeMembership => PgRoleOperation::RevokeMembership {
                role: name,
                member: self.text(Field::Member),
            },
            _ => bail!("This operation is not available for PostgreSQL"),
        })
    }
    fn optional_number(&self, field: Field) -> Result<Option<u32>> {
        if self.value(field).trim().is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.value(field).parse().map_err(|_| {
                anyhow::anyhow!("{} must be a nonnegative integer", field.label())
            })?))
        }
    }
    fn optional_bool(&self, field: Field) -> Result<Option<bool>> {
        Ok(match self.value(field) {
            "Unchanged" => None,
            "Enabled" => Some(true),
            "Disabled" => Some(false),
            _ => bail!("Choose a role attribute setting"),
        })
    }
}
fn referential(value: &str) -> Result<ReferentialAction> {
    Ok(match value {
        "NO ACTION" => ReferentialAction::NoAction,
        "RESTRICT" => ReferentialAction::Restrict,
        "CASCADE" => ReferentialAction::Cascade,
        "SET NULL" => ReferentialAction::SetNull,
        "SET DEFAULT" => ReferentialAction::SetDefault,
        _ => bail!("Select a referential action"),
    })
}
pub fn privileges(value: &str) -> Result<Vec<Privilege>> {
    value
        .split(',')
        .map(|p| {
            Ok(match p.trim().to_ascii_uppercase().as_str() {
                "ALL" => Privilege::All,
                "SELECT" => Privilege::Select,
                "INSERT" => Privilege::Insert,
                "UPDATE" => Privilege::Update,
                "DELETE" => Privilege::Delete,
                "REFERENCES" => Privilege::References,
                "TRIGGER" => Privilege::Trigger,
                "USAGE" => Privilege::Usage,
                "CREATE" => Privilege::Create,
                "CONNECT" => Privilege::Connect,
                "TEMPORARY" => Privilege::Temporary,
                "EXECUTE" => Privilege::Execute,
                "ALTER" => Privilege::Alter,
                "DROP" => Privilege::Drop,
                "INDEX" => Privilege::Index,
                "CREATE VIEW" => Privilege::CreateView,
                "SHOW VIEW" => Privilege::ShowView,
                "CREATE ROUTINE" => Privilege::CreateRoutine,
                "ALTER ROUTINE" => Privilege::AlterRoutine,
                "CREATE USER" => Privilege::CreateUser,
                "PROCESS" => Privilege::Process,
                "RELOAD" => Privilege::Reload,
                _ => bail!("Select at least one supported privilege"),
            })
        })
        .collect()
}

pub struct Review {
    sqlite: Option<rowboat_admin::sqlite_schema::Review>,
    semantics: rowboat_admin::providers::ExecutionSemantics,
    pub target: Target,
    pub operation: Operation,
    pub display: DdlPlan,
    plan: DdlPlan,
    mode: MySqlStringMode,
    mysql_original: Option<MySqlColumnDefinition>,
    pub refresh_table: Option<TableRef>,
}
impl Review {
    pub fn build(target: Target, draft: Draft, mode: MySqlStringMode) -> Result<Self> {
        Self::build_with_column(target, draft, mode, None)
    }
    fn build_with_column(
        target: Target,
        draft: Draft,
        mode: MySqlStringMode,
        column: Option<&MySqlColumnDefinition>,
    ) -> Result<Self> {
        ensure!(
            target.engine != Engine::Sqlite,
            "SQLite requires live preparation and its opaque schema review"
        );
        let display = draft.plan(&target, mode, true, column)?;
        let plan = draft.plan(&target, mode, false, column)?;
        let semantics =
            plan.execution_semantics(target.engine, draft.operation.provider_operation())?;
        ensure!(!plan.is_empty(), "The operation produced no SQL");
        let refresh_table = match draft.operation {
            Operation::DropTable => None,
            Operation::RenameTable => target.table.as_ref().map(|t| TableRef {
                schema: t.schema.clone(),
                name: draft.text(Field::NewName),
            }),
            _ => target.table.clone(),
        };
        Ok(Self {
            sqlite: None,
            semantics,
            operation: draft.operation,
            target,
            display,
            plan,
            mode,
            mysql_original: column.cloned(),
            refresh_table,
        })
    }
    pub fn validate(
        &self,
        target: &Target,
        operation: Operation,
        read_only: bool,
        busy: bool,
        pending: bool,
        transaction: Option<bool>,
    ) -> Result<()> {
        ensure!(
            &self.target == target && self.operation == operation,
            "Review belongs to a different session, target, or operation; review again"
        );
        ensure!(
            !read_only,
            "Management changes are disabled for read-only connections"
        );
        ensure!(
            !busy && !pending,
            "Finish the current operation and pending row edits first"
        );
        ensure!(
            transaction == Some(false),
            "Commit or roll back and confirm transaction state before applying management changes"
        );
        Ok(())
    }
    pub fn execution_note(&self) -> &'static str {
        crate::provider_ui::execution_note(self.semantics)
    }
}

pub async fn prepare(db: &Database, target: Target, draft: Draft) -> Result<Review> {
    rowboat_admin::providers::operation_support(db.engine(), draft.operation.provider_operation())?;
    if db.engine() == Engine::Sqlite {
        let op = draft
            .ddl_operation(&target, None)?
            .ok_or_else(|| anyhow::anyhow!("Unsupported SQLite schema operation"))?;
        let refresh_table = match &op {
            DdlOperation::DropTable { .. } => None,
            DdlOperation::RenameTable { table, new_name } => Some(TableRef {
                schema: table.schema.clone(),
                name: new_name.clone(),
            }),
            _ => target.table.clone(),
        };
        return prepare_sqlite(db, target, draft.operation, op, refresh_table).await;
    }
    let mode = mysql_mode(db).await?;
    if draft.operation == Operation::AlterColumn && target.engine == Engine::MySql {
        let table = target
            .table
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Choose a table first"))?;
        let column = mysql_column_definition(db, table, draft.value(Field::Name)).await?;
        Review::build_with_column(target, draft, mode, Some(&column))
    } else {
        Review::build(target, draft, mode)
    }
}

pub async fn prepare_table(db: &Database, target: Target, design: TableDesign) -> Result<Review> {
    ensure!(
        db.engine() == Engine::Sqlite,
        "Bound table design is SQLite-only"
    );
    let table = design.table.clone();
    prepare_sqlite(
        db,
        target,
        Operation::CreateTable,
        DdlOperation::CreateTable(design),
        Some(table),
    )
    .await
}

async fn prepare_sqlite(
    db: &Database,
    target: Target,
    operation: Operation,
    op: DdlOperation,
    refresh_table: Option<TableRef>,
) -> Result<Review> {
    let sqlite = rowboat_admin::sqlite_schema::prepare(db, &[op]).await?;
    let plan = sqlite.plan().clone();
    Ok(Review {
        semantics: plan.execution_semantics(
            Engine::Sqlite,
            rowboat_admin::providers::Operation::SchemaDdl,
        )?,
        display: plan.clone(),
        plan,
        target,
        operation,
        sqlite: Some(sqlite),
        mode: MySqlStringMode::BackslashEscapes,
        mysql_original: None,
        refresh_table,
    })
}

pub async fn mysql_mode(db: &Database) -> Result<MySqlStringMode> {
    if db.engine() != Engine::MySql {
        return Ok(MySqlStringMode::BackslashEscapes);
    }
    let rows = mysql_sql_mode(db).await?;
    let value = rows
        .first()
        .and_then(|r| r.rows.first())
        .and_then(|r| r.first())
        .ok_or_else(|| anyhow::anyhow!("Server did not report sql_mode"))?
        .display();
    Ok(
        if value
            .split(',')
            .any(|v| v.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"))
        {
            MySqlStringMode::NoBackslashEscapes
        } else {
            MySqlStringMode::BackslashEscapes
        },
    )
}

pub struct Applied {
    pub target: Target,
    pub result: Result<(), String>,
    pub transaction: Result<bool, String>,
    pub catalog: Result<Vec<TableInfo>, String>,
    pub structure: Option<(TableRef, Result<Structure, String>)>,
}
pub async fn apply(db: Database, review: Review) -> Applied {
    let mut began = false;
    let result = async {
        if let Some(sqlite) = &review.sqlite {
            let outcome = rowboat_admin::sqlite_schema::apply(&db, sqlite).await?;
            ensure!(
                outcome.status == rowboat_core::AdministrativeStatus::Committed,
                "SQLite schema change {:?}: {}{}",
                outcome.status,
                outcome.error.as_deref().unwrap_or("change did not commit"),
                if outcome.reusable {
                    ""
                } else {
                    "; session unavailable; reconnect before continuing"
                }
            );
            return Ok(());
        }
        ensure!(
            !db.in_transaction().await?,
            "Server reports an active transaction; finish it first"
        );
        ensure!(
            mysql_mode(&db).await? == review.mode,
            "SQL string mode changed after review; generate a new preview"
        );
        if let Some(original) = &review.mysql_original {
            let current = mysql_column_definition(
                &db,
                review.target.table.as_ref().unwrap(),
                &original.column.name,
            )
            .await?;
            ensure!(
                &current == original,
                "Column definition changed after review; inspect and review again before applying"
            );
        }
        let semantics = review
            .plan
            .execution_semantics(db.engine(), review.operation.provider_operation())?;
        ensure!(
            semantics == review.semantics,
            "Provider execution semantics changed; review again"
        );
        if semantics.atomicity == rowboat_admin::providers::Atomicity::Transactional {
            db.begin().await?;
            began = true;
        }
        db.query(&review.plan.sql(), 0).await?;
        if began {
            db.commit().await?;
            began = false;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let mut result = result.map_err(|e| if review.operation.secret() { "Account operation failed. Server details are withheld because they may contain the password. Check account policy and privileges, then review again.".into() } else { format!("{e:#}") });
    if began && let Err(e) = db.rollback().await {
        result = Err(format!(
            "{}; automatic rollback failed: {e:#}",
            result.err().unwrap_or_default()
        ));
    }
    let transaction = db.in_transaction().await.map_err(|e| format!("{e:#}"));
    let catalog = db.catalog().await.map_err(|e| format!("{e:#}"));
    // On failure refresh the original target: MySQL may already have committed.
    let table = if result.is_ok() {
        review.refresh_table
    } else {
        review.target.table.clone()
    };
    let structure = if let Some(table) = table {
        Some((
            table.clone(),
            db.structure(&table).await.map_err(|e| format!("{e:#}")),
        ))
    } else {
        None
    };
    Applied {
        target: review.target,
        result,
        transaction,
        catalog,
        structure,
    }
}

pub struct Inspection {
    pub results: Vec<QueryResult>,
    pub notice: Option<String>,
}
pub async fn inspect(db: &Database, target: &Target, draft: Draft) -> Result<Inspection> {
    if draft.operation == Operation::InspectConstraints {
        if db.engine() == Engine::Sqlite {
            return Ok(Inspection {
                results: rowboat_admin::sqlite_schema::inspect(
                    db,
                    target
                        .table
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("Choose a table first"))?,
                )
                .await?,
                notice: None,
            });
        }
        return Ok(Inspection {
            results: constraints(
                db,
                target
                    .table
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Choose a table first"))?,
            )
            .await?,
            notice: None,
        });
    }
    if db.engine() == Engine::MySql {
        let account = MySqlAccount {
            user: draft.text(Field::Name),
            host: draft.text(Field::Host),
        };
        let mut results = mysql_grants(db, &account).await?;
        let notice = match mysql_account_info(db, &account).await {
            Ok(info) => {
                results.extend(info);
                None
            }
            Err(e) => Some(format!(
                "Grants loaded. Account policy inspection is unavailable: {e:#}"
            )),
        };
        Ok(Inspection { results, notice })
    } else {
        let role = draft.value(Field::Name);
        let mut results = pg_role_info(db, role).await?;
        results.extend(pg_role_memberships(db, role).await?);
        results.extend(pg_grants(db, role).await?);
        Ok(Inspection {
            results,
            notice: None,
        })
    }
}

#[cfg(test)]
#[path = "management_tests.rs"]
mod tests;
