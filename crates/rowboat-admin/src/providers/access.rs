//! Bounded, selected-principal ACL inspection. This is not an SQL execution oracle.
use super::{Operation, ServerKind, server};
use crate::{
    access::{self, GrantOperation, GrantTarget, Grantee, MySqlAccount, Privilege},
    ddl::{DdlPlan, validate_name, validate_table},
    objects::cell_text,
};
use anyhow::{Context, Result, bail, ensure};
use rowboat_core::{Cell, Engine, QuerySession, TableRef, sql::literal};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const ROLE_LIMIT: usize = 256;
const EVIDENCE_LIMIT: usize = 4096;

pub(crate) fn build_principal(
    engine: Engine,
    account: String,
    host: Option<String>,
) -> Result<Principal> {
    match server(engine, Operation::AccessManagement)? {
        ServerKind::Postgres => {
            ensure!(
                host.is_none(),
                "PostgreSQL principals do not accept an account host"
            );
            validate_name(&account)?;
            Ok(Principal::Postgres(account))
        }
        ServerKind::MySql => {
            let account = MySqlAccount {
                user: account,
                host: host.context("MySQL principals require an explicit account host")?,
            };
            account.sql()?;
            Ok(Principal::MySql(account))
        }
    }
}

pub(crate) fn validate_principal(engine: Engine, principal: &Principal) -> Result<()> {
    match (server(engine, Operation::AccessManagement)?, principal) {
        (ServerKind::Postgres, Principal::Postgres(name)) => validate_name(name),
        (ServerKind::MySql, Principal::MySql(account)) => account.sql().map(|_| ()),
        _ => bail!("Principal does not match connection provider"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Principal {
    Postgres(String),
    MySql(MySqlAccount),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessObject {
    Namespace(String),
    Table(TableRef),
    Column { table: TableRef, column: String },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessContext {
    Principal,
    DefaultRoles,
    CurrentSession,
    AvailableRoles,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessRequest {
    pub principal: Principal,
    pub objects: Vec<AccessObject>,
    pub context: AccessContext,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessState {
    Granted,
    Denied,
    Unknown,
}
impl AccessState {
    fn known(yes: bool) -> Self {
        if yes { Self::Granted } else { Self::Denied }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrantSource {
    Direct,
    Inherited,
    Public,
    Owner,
    Superuser,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantEvidence {
    pub kind: GrantSource,
    pub principal: Option<Principal>,
    pub scope: String,
    pub grantable: bool,
    pub active: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessRow {
    pub object: AccessObject,
    pub privilege: String,
    pub effective: AccessState,
    pub grantable: AccessState,
    pub provenance: Vec<GrantEvidence>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleAccess {
    pub principal: Principal,
    pub inherited: AccessState,
    pub can_set_role: AccessState,
    pub default_role: AccessState,
    pub active: AccessState,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessReport {
    pub principal: Principal,
    pub context: AccessContext,
    pub rows: Vec<AccessRow>,
    pub roles: Vec<RoleAccess>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PageRequest {
    pub offset: u32,
    pub limit: u16,
}
impl Default for PageRequest {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 100,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrincipalPage {
    pub principals: Vec<Principal>,
    pub next_offset: Option<u32>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessChange {
    pub object: AccessObject,
    pub privileges: Vec<Privilege>,
    pub revoke: bool,
    pub grant_option: bool,
}

fn text(row: &[Cell], n: usize) -> Result<String> {
    cell_text(row, n)?.context("Unexpected NULL in access catalog")
}
fn yes(row: &[Cell], n: usize) -> Result<bool> {
    match text(row, n)?.as_str() {
        "true" | "t" | "1" | "Y" => Ok(true),
        "false" | "f" | "0" | "N" => Ok(false),
        _ => bail!("Unexpected access catalog boolean"),
    }
}
fn lit(engine: Engine, value: &str) -> String {
    literal(engine, &Cell::Text(value.to_owned()))
}
async fn rows(db: &impl QuerySession, sql: &str, limit: usize) -> Result<Vec<Vec<Cell>>> {
    let mut result = db.query(sql, limit + 1).await?;
    ensure!(result.len() == 1, "Expected one access catalog result");
    let result = result.remove(0);
    ensure!(
        !result.truncated && result.rows.len() <= limit,
        "Access catalog exceeded bounded inspection ({limit} rows); narrow the selection"
    );
    Ok(result.rows)
}
fn validate_object(object: &AccessObject) -> Result<()> {
    match object {
        AccessObject::Namespace(n) => validate_name(n),
        AccessObject::Table(t) | AccessObject::Column { table: t, .. } => {
            validate_table(t)?;
            validate_name(&t.schema)?;
            if let AccessObject::Column { column, .. } = object {
                validate_name(column)?;
            }
            Ok(())
        }
    }
}
fn validate_request(engine: Engine, request: &AccessRequest) -> Result<()> {
    server(engine, Operation::AccessManagement)?;
    match (&request.principal, engine) {
        (Principal::Postgres(name), Engine::Postgres) => validate_name(name)?,
        (Principal::MySql(account), Engine::MySql) => {
            account.sql()?;
        }
        _ => bail!("Principal does not match connection engine"),
    }
    ensure!(
        !request.objects.is_empty() && request.objects.len() <= 32,
        "Select 1–32 exact access objects"
    );
    ensure!(
        engine == Engine::MySql || request.context != AccessContext::AvailableRoles,
        "PostgreSQL SET ROLE availability is reported separately; it is not effective access"
    );
    for object in &request.objects {
        validate_object(object)?;
    }
    Ok(())
}

/// Paged complete native principal catalog. Permission failures are errors, not empty pages.
pub async fn list_principals(db: &impl QuerySession, page: PageRequest) -> Result<PrincipalPage> {
    let provider = server(db.engine(), Operation::AccessManagement)?;
    ensure!(
        (1..=500).contains(&page.limit),
        "Principal page limit must be 1–500"
    );
    let limit = usize::from(page.limit);
    let sql = match provider {
        ServerKind::Postgres => format!(
            "SELECT rolname FROM pg_catalog.pg_roles ORDER BY rolname LIMIT {} OFFSET {}",
            limit + 1,
            page.offset
        ),
        ServerKind::MySql => format!(
            "SELECT User,Host FROM mysql.user ORDER BY BINARY User,BINARY Host LIMIT {} OFFSET {}",
            limit + 1,
            page.offset
        ),
    };
    let mut data = rows(db, &sql, limit + 1).await?;
    let more = data.len() > limit;
    data.truncate(limit);
    let principals = data
        .iter()
        .map(|r| match provider {
            ServerKind::Postgres => Ok(Principal::Postgres(text(r, 0)?)),
            ServerKind::MySql => Ok(Principal::MySql(MySqlAccount {
                user: text(r, 0)?,
                host: text(r, 1)?,
            })),
        })
        .collect::<Result<_>>()?;
    Ok(PrincipalPage {
        principals,
        next_offset: if more {
            Some(
                page.offset
                    .checked_add(u32::from(page.limit))
                    .context("Page offset overflow")?,
            )
        } else {
            None
        },
        warnings: vec![
            "Catalog pages are not a snapshot; concurrent account changes can shift offsets."
                .into(),
        ],
    })
}

fn privileges(engine: ServerKind, object: &AccessObject, pg_version: u32) -> Vec<&'static str> {
    match (engine, object) {
        (_, AccessObject::Column { .. }) => vec!["SELECT", "INSERT", "UPDATE", "REFERENCES"],
        (ServerKind::Postgres, AccessObject::Namespace(_)) => vec!["USAGE", "CREATE"],
        (ServerKind::Postgres, _) => {
            let mut p = vec![
                "SELECT",
                "INSERT",
                "UPDATE",
                "DELETE",
                "TRUNCATE",
                "REFERENCES",
                "TRIGGER",
            ];
            if pg_version >= 170000 {
                p.push("MAINTAIN");
            }
            p
        }
        (ServerKind::MySql, AccessObject::Namespace(_)) => vec![
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "REFERENCES",
            "TRIGGER",
            "CREATE",
            "ALTER",
            "DROP",
            "INDEX",
            "CREATE VIEW",
            "SHOW VIEW",
            "EXECUTE",
            "CREATE ROUTINE",
            "ALTER ROUTINE",
            "EVENT",
            "LOCK TABLES",
            "CREATE TEMPORARY TABLES",
        ],
        (ServerKind::MySql, _) => vec![
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "REFERENCES",
            "TRIGGER",
            "CREATE",
            "ALTER",
            "DROP",
            "INDEX",
            "CREATE VIEW",
            "SHOW VIEW",
        ],
    }
}

/// Inspect ACL access only. Failed/incomplete catalog proofs produce Unknown rows.
pub async fn inspect_access(
    db: &impl QuerySession,
    request: &AccessRequest,
) -> Result<AccessReport> {
    let provider = server(db.engine(), Operation::AccessManagement)?;
    validate_request(db.engine(), request)?;
    let mut report = AccessReport {
        principal: request.principal.clone(), context: request.context,
        rows: vec![], roles: vec![], warnings: vec![
            "Object privileges do not prove successful SQL execution: namespace prerequisites, RLS, security-definer behavior, login restrictions and runtime conditions are separate. Collection is not an atomic catalog snapshot; do not interleave role changes.".into(),
        ],
    };
    let result = match provider {
        ServerKind::Postgres => inspect_pg(db, request, &mut report).await,
        ServerKind::MySql => inspect_mysql(db, request, &mut report).await,
    };
    if let Err(error) = result {
        report.warnings.push(format!(
            "Inspection could not prove access: {error:#}. Unknown is not denied."
        ));
        report.rows = request
            .objects
            .iter()
            .flat_map(|o| {
                privileges(provider, o, 0).into_iter().map(|p| AccessRow {
                    object: o.clone(),
                    privilege: p.into(),
                    effective: AccessState::Unknown,
                    grantable: AccessState::Unknown,
                    provenance: vec![],
                })
            })
            .collect();
    }
    Ok(report)
}

async fn inspect_pg(
    db: &impl QuerySession,
    request: &AccessRequest,
    report: &mut AccessReport,
) -> Result<()> {
    let Principal::Postgres(name) = &request.principal else {
        unreachable!()
    };
    let p = lit(Engine::Postgres, name);
    let identity = rows(db, &format!("SELECT oid::text,rolsuper,current_setting('server_version_num'),current_user,(SELECT oid::text FROM pg_catalog.pg_roles WHERE rolname=session_user) FROM pg_catalog.pg_roles WHERE rolname={p}"), 1).await?;
    let identity = identity
        .first()
        .context("Selected PostgreSQL role does not exist")?;
    let oid: u32 = text(identity, 0)?.parse()?;
    let superuser = yes(identity, 1)?;
    let version: u32 = text(identity, 2)?.parse()?;
    if request.context == AccessContext::CurrentSession {
        ensure!(
            text(identity, 3)? == *name,
            "CurrentSession requires selected principal to equal current_user"
        );
    }
    // SET ROLE is authorized against the session user, even after a previous
    // SET ROLE changes current_user. Privilege inheritance still uses `oid`.
    let set_oid: u32 = if request.context == AccessContext::CurrentSession {
        text(identity, 4)?.parse()?
    } else {
        oid
    };
    let set_mode = if version >= 160000 { "SET" } else { "MEMBER" };
    let memberships = rows(db, &format!("SELECT rolname,pg_catalog.pg_has_role({oid},oid,'USAGE'),pg_catalog.pg_has_role({set_oid},oid,'{set_mode}') FROM pg_catalog.pg_roles WHERE oid<>{oid} AND (pg_catalog.pg_has_role({oid},oid,'MEMBER') OR pg_catalog.pg_has_role({set_oid},oid,'{set_mode}')) ORDER BY rolname LIMIT {}", ROLE_LIMIT + 1), ROLE_LIMIT).await;
    match memberships {
        Ok(memberships) => for r in memberships {
            report.roles.push(RoleAccess { principal: Principal::Postgres(text(&r, 0)?), inherited: AccessState::known(yes(&r, 1)?), can_set_role: AccessState::known(yes(&r, 2)?), default_role: AccessState::Unknown, active: AccessState::known(yes(&r, 1)?) });
        },
        Err(e) => report.warnings.push(format!("Role provenance list incomplete: {e:#}; native effective privilege checks remain authoritative.")),
    }
    report.warnings.push("PostgreSQL inheritance uses pg_has_role(USAGE); SET ROLE availability is separate and respects membership SET options on PostgreSQL 16+. Ownership always supplies grant options, but owners can revoke their own ordinary ACL privileges. PUBLIC means the special ACL grantee, not a role named PUBLIC.".into());
    if request.context == AccessContext::CurrentSession {
        report.warnings.push("CurrentSession SET ROLE availability uses session_user authority; effective/inherited object privileges use the selected current_user.".into());
    }
    for object in &request.objects {
        let (catalog, predicate, acl) = match object {
            AccessObject::Namespace(n) => (
                format!(
                    "SELECT oid,nspowner AS owner,COALESCE(nspacl,pg_catalog.acldefault('n',nspowner)) AS acl FROM pg_catalog.pg_namespace WHERE nspname={}",
                    lit(Engine::Postgres, n)
                ),
                format!("pg_catalog.has_schema_privilege({oid},o.oid,p.privilege)"),
                "SELECT a.*, 'SCHEMA'::text AS scope FROM pg_catalog.aclexplode(o.acl) a"
                    .to_owned(),
            ),
            AccessObject::Table(t) | AccessObject::Column { table: t, .. } => {
                let mut catalog = format!(
                    "SELECT c.oid,c.relowner AS owner,COALESCE(c.relacl,pg_catalog.acldefault('r',c.relowner)) AS acl{} FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace {} WHERE n.nspname={} AND c.relname={} AND c.relkind IN ('r','p','v','m','f')",
                    if matches!(object, AccessObject::Column { .. }) {
                        ",att.attnum,att.attacl"
                    } else {
                        ""
                    },
                    if let AccessObject::Column { column, .. } = object {
                        format!(
                            "JOIN pg_catalog.pg_attribute att ON att.attrelid=c.oid AND att.attnum>0 AND NOT att.attisdropped AND att.attname={}",
                            lit(Engine::Postgres, column)
                        )
                    } else {
                        String::new()
                    },
                    lit(Engine::Postgres, &t.schema),
                    lit(Engine::Postgres, &t.name)
                );
                let (predicate, acl) = if matches!(object, AccessObject::Column { .. }) {
                    (format!("pg_catalog.has_column_privilege({oid},o.oid,o.attnum,p.privilege)"), "SELECT a.*, 'TABLE'::text AS scope FROM pg_catalog.aclexplode(o.acl) a UNION ALL SELECT a.*, 'COLUMN' FROM pg_catalog.aclexplode(o.attacl) a".into())
                } else {
                    (
                        format!("pg_catalog.has_table_privilege({oid},o.oid,p.privilege)"),
                        "SELECT a.*, 'TABLE'::text AS scope FROM pg_catalog.aclexplode(o.acl) a"
                            .into(),
                    )
                };
                catalog.shrink_to_fit();
                (catalog, predicate, acl)
            }
        };
        let values = privileges(ServerKind::Postgres, object, version)
            .iter()
            .map(|p| format!("('{p}')"))
            .collect::<Vec<_>>()
            .join(",");
        let grant_predicate =
            predicate.replace("p.privilege)", "p.privilege || ' WITH GRANT OPTION')");
        let sql = format!(
            "WITH o AS ({catalog}),p(privilege) AS (VALUES {values}) SELECT p.privilege,{predicate},{grant_predicate},pg_catalog.pg_get_userbyid(o.owner),pg_catalog.pg_has_role({oid},o.owner,'USAGE'),COALESCE((SELECT json_agg(x)::text FROM (SELECT CASE WHEN a.grantee=0 THEN NULL ELSE pg_catalog.pg_get_userbyid(a.grantee) END AS principal,a.grantee=0 AS public,a.grantee={oid} AS direct,a.grantee=o.owner AS owner,a.is_grantable AS grantable,a.scope,CASE WHEN a.grantee=0 THEN true ELSE pg_catalog.pg_has_role({oid},a.grantee,'USAGE') END AS active FROM ({acl}) a WHERE a.privilege_type=p.privilege AND (a.grantee=0 OR a.grantee={oid} OR pg_catalog.pg_has_role({oid},a.grantee,'MEMBER')) LIMIT {}) x),'[]') FROM o CROSS JOIN p",
            EVIDENCE_LIMIT + 1
        );
        let data = rows(db, &sql, 32).await?;
        ensure!(
            !data.is_empty(),
            "Selected object does not exist in the current database"
        );
        for r in data {
            let evidence: Vec<serde_json::Value> = serde_json::from_str(&text(&r, 5)?)?;
            let mut provenance = Vec::new();
            if evidence.len() > EVIDENCE_LIMIT {
                report.warnings.push("ACL provenance exceeded 4096 entries; native effective/grantable checks remain authoritative.".into());
            }
            for e in evidence.into_iter().take(EVIDENCE_LIMIT) {
                provenance.push(GrantEvidence {
                    kind: if e["public"] == true {
                        GrantSource::Public
                    } else if e["owner"] == true {
                        GrantSource::Owner
                    } else if e["direct"] == true {
                        GrantSource::Direct
                    } else {
                        GrantSource::Inherited
                    },
                    principal: e["principal"]
                        .as_str()
                        .map(|s| Principal::Postgres(s.into())),
                    scope: e["scope"].as_str().context("Missing ACL scope")?.into(),
                    grantable: e["grantable"] == true,
                    active: e["active"] == true,
                });
            }
            if yes(&r, 4)? {
                provenance.push(GrantEvidence {
                    kind: GrantSource::Owner,
                    principal: Some(Principal::Postgres(text(&r, 3)?)),
                    scope: "OWNER: implicit grant option, not an ordinary privilege grant".into(),
                    grantable: true,
                    active: true,
                });
            }
            if superuser {
                provenance.push(GrantEvidence {
                    kind: GrantSource::Superuser,
                    principal: Some(request.principal.clone()),
                    scope: "SUPERUSER".into(),
                    grantable: true,
                    active: true,
                });
            }
            report.rows.push(AccessRow {
                object: object.clone(),
                privilege: text(&r, 0)?,
                effective: AccessState::known(yes(&r, 1)?),
                grantable: AccessState::known(yes(&r, 2)?),
                provenance,
            });
        }
    }
    Ok(())
}

type AccountKey = (String, String);
fn account_key(account: &MySqlAccount) -> AccountKey {
    (account.user.clone(), account.host.clone())
}
fn principal(key: &AccountKey) -> Principal {
    Principal::MySql(MySqlAccount {
        user: key.0.clone(),
        host: key.1.clone(),
    })
}
fn account_filter(key: &AccountKey, user: &str, host: &str) -> String {
    format!(
        "(BINARY {user}=BINARY {} AND BINARY {host}=BINARY {})",
        lit(Engine::MySql, &key.0),
        lit(Engine::MySql, &key.1)
    )
}
fn closure(
    roots: BTreeSet<AccountKey>,
    edges: &[(AccountKey, AccountKey)],
) -> BTreeSet<AccountKey> {
    let mut result = roots;
    loop {
        let before = result.len();
        for (member, role) in edges {
            if result.contains(member) {
                result.insert(role.clone());
            }
        }
        if before == result.len() {
            return result;
        }
    }
}
const MYSQL_COLUMNS: &[(&str, &str)] = &[
    ("SELECT", "Select_priv"),
    ("INSERT", "Insert_priv"),
    ("UPDATE", "Update_priv"),
    ("DELETE", "Delete_priv"),
    ("REFERENCES", "References_priv"),
    ("TRIGGER", "Trigger_priv"),
    ("CREATE", "Create_priv"),
    ("ALTER", "Alter_priv"),
    ("DROP", "Drop_priv"),
    ("INDEX", "Index_priv"),
    ("CREATE VIEW", "Create_view_priv"),
    ("SHOW VIEW", "Show_view_priv"),
    ("EXECUTE", "Execute_priv"),
    ("CREATE ROUTINE", "Create_routine_priv"),
    ("ALTER ROUTINE", "Alter_routine_priv"),
    ("EVENT", "Event_priv"),
    ("LOCK TABLES", "Lock_tables_priv"),
    ("CREATE TEMPORARY TABLES", "Create_tmp_table_priv"),
];
fn mysql_privilege_json(alias: &str) -> String {
    format!(
        "JSON_OBJECT({})",
        MYSQL_COLUMNS
            .iter()
            .map(|(p, c)| format!("'{p}',{alias}.{c}"))
            .collect::<Vec<_>>()
            .join(",")
    )
}
fn json_privileges(value: &str) -> Result<BTreeSet<String>> {
    let values: BTreeMap<String, String> = serde_json::from_str(value)?;
    Ok(values
        .into_iter()
        .filter_map(|(p, v)| (v == "Y").then_some(p))
        .collect())
}
fn set_privileges(value: &str) -> BTreeSet<String> {
    value
        .split(',')
        .filter(|p| !p.is_empty())
        .map(|p| p.to_ascii_uppercase())
        .collect()
}
#[derive(Debug)]
struct MyGrant {
    account: AccountKey,
    schema: Option<String>,
    table: Option<String>,
    /// Server-produced utf8mb3_general_ci ACL lookup key, not ASCII case rules.
    column: Option<String>,
    privileges: BTreeSet<String>,
    grantable: bool,
}

async fn inspect_mysql(
    db: &impl QuerySession,
    request: &AccessRequest,
    report: &mut AccessReport,
) -> Result<()> {
    let Principal::MySql(account) = &request.principal else {
        unreachable!()
    };
    let selected = account_key(account);
    let settings = rows(db, "SELECT CURRENT_USER(),@@global.activate_all_roles_on_login,@@global.mandatory_roles,@@global.partial_revokes,@@lower_case_table_names", 1).await?;
    let settings = settings.first().context("Missing MySQL role settings")?;
    let current_matches = text(settings, 0)? == format!("{}@{}", account.user, account.host);
    if request.context == AccessContext::CurrentSession {
        ensure!(
            current_matches,
            "CurrentSession requires selected account to equal CURRENT_USER()"
        );
    }
    ensure!(
        text(settings, 2)?.is_empty(),
        "Mandatory MySQL roles are configured; effective-role proof is not supported for this configuration"
    );
    let partial_revokes = yes(settings, 3)?;
    let fold_names = text(settings, 4)? != "0";
    let role_cte = format!(
        "WITH RECURSIVE available(User,Host) AS (SELECT User,Host FROM mysql.user WHERE {} UNION DISTINCT SELECT e.FROM_USER,e.FROM_HOST FROM mysql.role_edges e JOIN available a ON BINARY e.TO_USER=BINARY a.User AND BINARY e.TO_HOST=BINARY a.Host)",
        account_filter(&selected, "User", "Host")
    );
    let accounts = rows(db, &format!("{role_cte} SELECT u.User,u.Host,{},u.Grant_priv,COALESCE(JSON_EXTRACT(u.User_attributes,'$.Restrictions'),JSON_ARRAY()) FROM available a JOIN mysql.user u ON BINARY u.User=BINARY a.User AND BINARY u.Host=BINARY a.Host LIMIT {}", mysql_privilege_json("u"), ROLE_LIMIT + 1), ROLE_LIMIT).await?;
    ensure!(
        !accounts.is_empty(),
        "Selected MySQL account does not exist"
    );
    let keys = accounts
        .iter()
        .map(|r| Ok((text(r, 0)?, text(r, 1)?)))
        .collect::<Result<BTreeSet<_>>>()?;
    let filter = keys
        .iter()
        .map(|k| account_filter(k, "User", "Host"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let edge_rows = rows(db, &format!("{role_cte} SELECT e.TO_USER,e.TO_HOST,e.FROM_USER,e.FROM_HOST FROM mysql.role_edges e JOIN available a ON BINARY e.TO_USER=BINARY a.User AND BINARY e.TO_HOST=BINARY a.Host LIMIT {}", EVIDENCE_LIMIT + 1), EVIDENCE_LIMIT).await?;
    let edges = edge_rows
        .iter()
        .map(|r| Ok(((text(r, 0)?, text(r, 1)?), (text(r, 2)?, text(r, 3)?))))
        .collect::<Result<Vec<_>>>()?;
    let defaults = rows(
        db,
        &format!(
            "SELECT DEFAULT_ROLE_USER,DEFAULT_ROLE_HOST FROM mysql.default_roles WHERE {} LIMIT {}",
            account_filter(&selected, "USER", "HOST"),
            ROLE_LIMIT + 1
        ),
        ROLE_LIMIT,
    )
    .await?
    .iter()
    .map(|r| Ok((text(r, 0)?, text(r, 1)?)))
    .collect::<Result<BTreeSet<_>>>()?;
    ensure!(
        defaults.is_subset(&keys),
        "Default role catalog changed during inspection"
    );
    let active_roots = match request.context {
        AccessContext::Principal => BTreeSet::new(),
        AccessContext::DefaultRoles if yes(settings, 1)? => keys.clone(),
        AccessContext::DefaultRoles => defaults.clone(),
        AccessContext::AvailableRoles => keys.clone(),
        AccessContext::CurrentSession => rows(
            db,
            &format!(
                "SELECT ROLE_NAME,ROLE_HOST FROM information_schema.ENABLED_ROLES LIMIT {}",
                ROLE_LIMIT + 1
            ),
            ROLE_LIMIT,
        )
        .await?
        .iter()
        .map(|r| Ok((text(r, 0)?, text(r, 1)?)))
        .collect::<Result<BTreeSet<_>>>()?,
    };
    ensure!(
        active_roots.is_subset(&keys),
        "Active roles are not fully visible in the selected account's role graph"
    );
    // The account is always active, but its available roles must not be activated
    // merely by traversing outgoing membership edges from that account.
    let mut active = closure(active_roots, &edges);
    active.insert(selected.clone());
    for key in keys.iter().filter(|k| **k != selected) {
        report.roles.push(RoleAccess {
            principal: principal(key),
            inherited: AccessState::known(active.contains(key)),
            can_set_role: AccessState::known(
                edges
                    .iter()
                    .any(|(member, role)| member == &selected && role == key),
            ),
            default_role: AccessState::known(defaults.contains(key)),
            active: AccessState::known(active.contains(key)),
        });
    }
    if request.context == AccessContext::AvailableRoles {
        report.warnings.push("Hypothetical union with every available role enabled; this is not the current session or default login state.".into());
    }
    report.warnings.push("MySQL does not give table owners or SUPER an implicit table-privilege bypass. DefaultRoles models configured login defaults; CurrentSession uses ENABLED_ROLES. Namespace rows cover schema/global grants, not an aggregation of individual table grants.".into());
    if request.context == AccessContext::CurrentSession {
        report.warnings.push("MySQL caches global/database privileges in existing sessions. CurrentSession proves positive table/column ACL grants with active roles, but other effective/grantable results are Unknown because catalog reads cannot reconstruct cached privileges. DefaultRoles models a fresh login against the current catalog.".into());
    }
    let mut grants = Vec::new();
    for r in accounts {
        let restrictions: serde_json::Value = serde_json::from_str(&text(&r, 4)?)?;
        ensure!(
            restrictions.as_array().is_some_and(Vec::is_empty),
            "MySQL partial-revoke restrictions are present; effective access requires restriction-aware server evaluation"
        );
        grants.push(MyGrant {
            account: (text(&r, 0)?, text(&r, 1)?),
            schema: None,
            table: None,
            column: None,
            privileges: json_privileges(&text(&r, 2)?)?,
            grantable: yes(&r, 3)?,
        });
    }
    // Only selected principals' grants are read. Never fetch mysql.user secrets.
    let database_rows = rows(
        db,
        &format!(
            "SELECT User,Host,Db,{},Grant_priv FROM mysql.db d WHERE {filter} LIMIT {}",
            mysql_privilege_json("d"),
            EVIDENCE_LIMIT + 1
        ),
        EVIDENCE_LIMIT,
    )
    .await?;
    for r in database_rows {
        grants.push(MyGrant {
            account: (text(&r, 0)?, text(&r, 1)?),
            schema: Some(text(&r, 2)?),
            table: None,
            column: None,
            privileges: json_privileges(&text(&r, 3)?)?,
            grantable: yes(&r, 4)?,
        });
    }
    let targets = request
        .objects
        .iter()
        .filter_map(|o| match o {
            AccessObject::Table(t) | AccessObject::Column { table: t, .. } => Some(format!(
                "(BINARY Db=BINARY {} AND BINARY Table_name=BINARY {})",
                lit(Engine::MySql, &t.schema),
                lit(Engine::MySql, &t.name)
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !targets.is_empty() {
        let targets = targets.join(" OR ");
        let table_rows = rows(db, &format!("SELECT User,Host,Db,Table_name,Table_priv FROM mysql.tables_priv WHERE ({filter}) AND ({targets}) LIMIT {}",EVIDENCE_LIMIT+1),EVIDENCE_LIMIT).await?;
        for r in table_rows {
            let privileges = set_privileges(&text(&r, 4)?);
            grants.push(MyGrant {
                account: (text(&r, 0)?, text(&r, 1)?),
                schema: Some(text(&r, 2)?),
                table: Some(text(&r, 3)?),
                column: None,
                grantable: privileges.contains("GRANT"),
                privileges,
            });
        }
        // MySQL's GRANT_TABLE column hash uses system_charset_info
        // (utf8mb3_general_ci), including its non-ASCII/accent equivalences.
        // Use server collation weights rather than reimplementing its tables.
        let column_rows = rows(db, &format!("SELECT User,Host,Db,Table_name,HEX(WEIGHT_STRING(RTRIM(Column_name) COLLATE utf8mb3_general_ci)),Column_priv FROM mysql.columns_priv WHERE ({filter}) AND ({targets}) LIMIT {}",EVIDENCE_LIMIT+1),EVIDENCE_LIMIT).await?;
        for r in column_rows {
            grants.push(MyGrant {
                account: (text(&r, 0)?, text(&r, 1)?),
                schema: Some(text(&r, 2)?),
                table: Some(text(&r, 3)?),
                column: Some(text(&r, 4)?),
                privileges: set_privileges(&text(&r, 5)?),
                grantable: false,
            });
        }
    }
    for object in &request.objects {
        let schema = match object {
            AccessObject::Namespace(s) => s,
            AccessObject::Table(t) | AccessObject::Column { table: t, .. } => &t.schema,
        };
        ensure!(
            !["mysql", "sys", "information_schema", "performance_schema"]
                .contains(&schema.to_ascii_lowercase().as_str()),
            "MySQL system-schema special privileges are outside this object ACL matrix"
        );
        let exists = match object {
            AccessObject::Namespace(s) => format!(
                "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE BINARY SCHEMA_NAME=BINARY {}",
                lit(Engine::MySql, s)
            ),
            AccessObject::Table(t) => format!(
                "SELECT TABLE_NAME FROM information_schema.TABLES WHERE BINARY TABLE_SCHEMA=BINARY {} AND BINARY TABLE_NAME=BINARY {}",
                lit(Engine::MySql, &t.schema),
                lit(Engine::MySql, &t.name)
            ),
            AccessObject::Column { table: t, column } => format!(
                "SELECT HEX(WEIGHT_STRING(RTRIM(CONVERT(COLUMN_NAME USING utf8mb3)) COLLATE utf8mb3_general_ci)) FROM information_schema.COLUMNS WHERE BINARY TABLE_SCHEMA=BINARY {} AND BINARY TABLE_NAME=BINARY {} AND BINARY COLUMN_NAME=BINARY {}",
                lit(Engine::MySql, &t.schema),
                lit(Engine::MySql, &t.name),
                lit(Engine::MySql, column)
            ),
        };
        let object_catalog = rows(db, &exists, 1).await?;
        if object_catalog.len() != 1 {
            report.warnings.push(format!("{object:?} does not exist with this exact catalog identity or is not visible to the inspector; its access is Unknown."));
            report.rows.extend(
                privileges(ServerKind::MySql, object, 0)
                    .into_iter()
                    .map(|p| AccessRow {
                        object: object.clone(),
                        privilege: p.into(),
                        effective: AccessState::Unknown,
                        grantable: AccessState::Unknown,
                        provenance: vec![],
                    }),
            );
            continue;
        }
        let column_key = if matches!(object, AccessObject::Column { .. }) {
            Some(text(&object_catalog[0], 0)?)
        } else {
            None
        };
        let relevant = grants
            .iter()
            .filter(|g| {
                mysql_applies(
                    g,
                    object,
                    partial_revokes,
                    fold_names,
                    column_key.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        let mut matches = BTreeMap::<AccountKey, usize>::new();
        for g in relevant
            .iter()
            .filter(|g| g.schema.is_some() && g.table.is_none())
        {
            *matches.entry(g.account.clone()).or_default() += 1;
        }
        let ambiguous_database = matches.iter().any(|(k, n)| *n > 1 && active.contains(k));
        if ambiguous_database {
            report.warnings.push(format!("Overlapping MySQL database grant patterns for {schema}; first-match precedence is not inferred. Effective/grantable rows are Unknown."));
        }
        let mut column_matches = BTreeMap::<AccountKey, usize>::new();
        for g in relevant.iter().filter(|g| g.column.is_some()) {
            *column_matches.entry(g.account.clone()).or_default() += 1;
        }
        let ambiguous_column = column_matches
            .iter()
            .any(|(k, n)| *n > 1 && active.contains(k));
        if ambiguous_column {
            report.warnings.push(format!("Overlapping MySQL column grant names under the native ACL collation for {object:?}; cache lookup precedence is not inferred. Effective/grantable rows are Unknown."));
        }
        let ambiguous = ambiguous_database || ambiguous_column;
        let go = relevant
            .iter()
            .any(|g| g.grantable && active.contains(&g.account));
        for privilege in privileges(ServerKind::MySql, object, 0) {
            let found = relevant
                .iter()
                .filter(|g| g.privileges.contains(privilege))
                .collect::<Vec<_>>();
            let allowed = found.iter().any(|g| active.contains(&g.account));
            let current_session = request.context == AccessContext::CurrentSession;
            let table_proof = found
                .iter()
                .any(|g| g.table.is_some() && active.contains(&g.account));
            let table_grant_option = relevant
                .iter()
                .any(|g| g.table.is_some() && g.grantable && active.contains(&g.account));
            report.rows.push(AccessRow {
                object: object.clone(),
                privilege: privilege.into(),
                effective: if ambiguous || (current_session && !table_proof) {
                    AccessState::Unknown
                } else {
                    AccessState::known(allowed)
                },
                grantable: if ambiguous || (current_session && !(table_proof && table_grant_option))
                {
                    AccessState::Unknown
                } else {
                    AccessState::known(allowed && go)
                },
                provenance: relevant
                    .iter()
                    .filter(|g| g.privileges.contains(privilege) || g.grantable)
                    .map(|g| GrantEvidence {
                        kind: if g.account == selected {
                            GrantSource::Direct
                        } else {
                            GrantSource::Inherited
                        },
                        principal: Some(principal(&g.account)),
                        scope: format!(
                            "{}{}",
                            if g.column.is_some() {
                                "COLUMN"
                            } else if g.table.is_some() {
                                "TABLE"
                            } else if g.schema.is_some() {
                                "SCHEMA"
                            } else {
                                "GLOBAL"
                            },
                            if !g.privileges.contains(privilege) {
                                ": GRANT OPTION authority only, not a privilege grant"
                            } else {
                                ""
                            }
                        ),
                        grantable: g.grantable,
                        active: active.contains(&g.account),
                    })
                    .collect(),
            });
        }
    }
    Ok(())
}

fn mysql_applies(
    grant: &MyGrant,
    object: &AccessObject,
    literal_db: bool,
    fold: bool,
    column_key: Option<&str>,
) -> bool {
    let (schema, table) = match object {
        AccessObject::Namespace(s) => (s, None),
        AccessObject::Table(t) | AccessObject::Column { table: t, .. } => {
            (&t.schema, Some(&t.name))
        }
    };
    let normalize = |s: &str| if fold { s.to_lowercase() } else { s.into() };
    if let Some(db) = &grant.schema {
        let matches = if grant.table.is_some() || literal_db {
            normalize(db) == normalize(schema)
        } else {
            mysql_pattern(&normalize(db), &normalize(schema))
        };
        if !matches {
            return false;
        }
    }
    if let Some(t) = &grant.table
        && !table.is_some_and(|name| normalize(name) == normalize(t))
    {
        return false;
    }
    if let Some(c) = &grant.column
        && column_key != Some(c.as_str())
    {
        return false;
    }
    true
}

/// MySQL grant-table wildcard matching, independent of session sql_mode.
fn mysql_pattern(pattern: &str, value: &str) -> bool {
    #[derive(Clone, Copy)]
    enum Token {
        Any,
        One,
        Char(char),
    }
    let mut tokens = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '%' => Token::Any,
            '_' => Token::One,
            '\\' => Token::Char(chars.next().unwrap_or('\\')),
            c => Token::Char(c),
        });
    }
    let value = value.chars().collect::<Vec<_>>();
    let mut prev = vec![false; value.len() + 1];
    prev[0] = true;
    for token in tokens {
        let mut next = vec![false; value.len() + 1];
        for i in 0..=value.len() {
            next[i] = match token {
                Token::Any => prev[i] || (i > 0 && next[i - 1]),
                Token::One => i > 0 && prev[i - 1],
                Token::Char(c) => i > 0 && prev[i - 1] && value[i - 1] == c,
            };
        }
        prev = next;
    }
    prev[value.len()]
}

/// Build a review-only privilege change against the exact inspected selection.
pub async fn preview_change(
    db: &impl QuerySession,
    request: &AccessRequest,
    change: &AccessChange,
) -> Result<DdlPlan> {
    validate_request(db.engine(), request)?;
    ensure!(
        request.objects.contains(&change.object),
        "Privilege change target is not in the inspected selection"
    );
    let target = match &change.object {
        AccessObject::Namespace(s) => GrantTarget::Schema(s.clone()),
        AccessObject::Table(t) | AccessObject::Column { table: t, .. } => {
            GrantTarget::Table(t.clone())
        }
    };
    let grantee = match &request.principal {
        Principal::Postgres(n) => Grantee::PgRole(n.clone()),
        Principal::MySql(a) => Grantee::MySqlAccount(a.clone()),
    };
    let operation = GrantOperation {
        target,
        grantee,
        privileges: change.privileges.clone(),
        revoke: change.revoke,
        grant_option: change.grant_option,
    };
    let mode = if db.engine() == Engine::MySql {
        let data = rows(db, "SELECT @@global.partial_revokes", 1).await?;
        if yes(data.first().context("Missing partial_revokes setting")?, 0)? {
            access::MySqlDatabaseGrantMode::LiteralNames
        } else {
            access::MySqlDatabaseGrantMode::WildcardPatterns
        }
    } else {
        access::MySqlDatabaseGrantMode::LiteralNames
    };
    let mut plan = if let AccessObject::Column { column, .. } = &change.object {
        access::column_grant_sql(db.engine(), &operation, std::slice::from_ref(column))?
    } else {
        access::grant_sql_with_mysql_mode(db.engine(), &operation, mode)?
    };
    if change.revoke {
        plan.warnings.push("Revoking a direct grant may not remove effective access supplied by inherited/active roles, PUBLIC, ownership/superuser, or broader schema/global grants. Re-inspect after applying; this preview does not simulate the resulting ACL.".into());
    }
    plan.warnings.push("Exact selected principal/object only. Grant authority and concurrent ACL changes are checked by the server at explicit execution; no automatic apply or atomic freshness guarantee.".into());
    Ok(plan)
}
