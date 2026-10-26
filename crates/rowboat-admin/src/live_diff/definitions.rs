//! Bounded existing-object definition synchronization. Review is immutable;
//! apply checks both endpoints while owning their physical sessions.
#[path = "definition_sql.rs"]
mod grammar;
use super::NamespaceMapping;
use crate::{
    ddl::DdlPlan,
    object_editor::{
        self, ObjectApplyOutcome, ObjectApplyStatus, ObjectEdit, ObjectIdentity, ObjectPatch,
        ObjectSnapshot,
    },
    objects::{ObjectKind, cell_text},
};
use anyhow::{Context, Result, ensure};
use rowboat_core::{
    AdministrativeOperation, Cell, Database, Engine, QuerySession,
    sql::{literal, quote_ident},
};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Scope {
    ViewQuery,
    RoutineBody,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DefinitionSnapshot {
    object: ObjectSnapshot,
    scope: Scope,
    evidence: Value,
    blockers: Vec<String>,
}
impl DefinitionSnapshot {
    pub fn object(&self) -> &ObjectSnapshot {
        &self.object
    }
    pub fn scope(&self) -> Scope {
        self.scope
    }
    pub fn evidence(&self) -> &Value {
        &self.evidence
    }
    pub fn blockers(&self) -> &[String] {
        &self.blockers
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct DefinitionComparison {
    source: DefinitionSnapshot,
    destination: DefinitionSnapshot,
    mapping: NamespaceMapping,
    differences: Vec<String>,
    blockers: Vec<String>,
    warnings: Vec<String>,
    plan: DdlPlan,
    edit: Option<ObjectEdit>,
}
impl DefinitionComparison {
    pub fn source(&self) -> &DefinitionSnapshot {
        &self.source
    }
    pub fn destination(&self) -> &DefinitionSnapshot {
        &self.destination
    }
    pub fn mapping(&self) -> &NamespaceMapping {
        &self.mapping
    }
    pub fn differences(&self) -> &[String] {
        &self.differences
    }
    pub fn blockers(&self) -> &[String] {
        &self.blockers
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
    pub fn plan(&self) -> &DdlPlan {
        &self.plan
    }
    pub fn recovery_plan(&self) -> Option<&DdlPlan> {
        self.edit.as_ref().and_then(ObjectEdit::recovery_plan)
    }
    pub fn non_atomic(&self) -> bool {
        self.edit.as_ref().is_some_and(ObjectEdit::non_atomic)
    }
    pub fn is_blocked(&self) -> bool {
        !self.blockers.is_empty()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DefinitionApplyStatus {
    Applied,
    Unchanged,
    Blocked,
    StaleSource,
    StaleDestination,
    Failed,
    OutcomeUnknown,
}
#[derive(Debug, Clone, Serialize)]
pub struct DefinitionApplyOutcome {
    pub status: DefinitionApplyStatus,
    pub object_outcome: Option<ObjectApplyOutcome>,
    pub detail: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DefinitionFreshness {
    pub source: bool,
    pub destination: bool,
}
fn val(engine: Engine, s: &str) -> String {
    literal(engine, &Cell::Text(s.into()))
}
async fn rows(db: &impl QuerySession, sql: &str) -> Result<Vec<Value>> {
    let r = db.query(sql, 10_000).await?;
    ensure!(
        r.len() == 1 && !r[0].truncated,
        "Incomplete definition/dependency evidence"
    );
    r[0].rows
        .iter()
        .map(|row| {
            Ok(serde_json::from_str(
                &cell_text(row, 0)?.context("NULL catalog evidence")?,
            )?)
        })
        .collect()
}
async fn one(db: &impl QuerySession, sql: &str) -> Result<Value> {
    let mut r = rows(db, sql).await?;
    ensure!(r.len() == 1, "Expected one complete definition record");
    Ok(r.remove(0))
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("Missing definition evidence {key}"))
}
fn text_list(v: &Value) -> Result<Vec<String>> {
    v.as_array()
        .context("Expected identifier list")?
        .iter()
        .map(|v| Ok(v.as_str().context("Nontext identifier")?.into()))
        .collect()
}
fn scalar_type(ty: &str) -> bool {
    let ty = ty.to_ascii_lowercase();
    [
        "smallint",
        "integer",
        "bigint",
        "int",
        "tinyint",
        "mediumint",
        "numeric",
        "decimal",
        "real",
        "double precision",
        "float",
        "double",
        "boolean",
        "bool",
        "text",
        "varchar",
        "character varying",
        "char",
        "character",
    ]
    .iter()
    .any(|base| {
        ty == *base
            || ty.strip_prefix(base).is_some_and(|rest| {
                rest.starts_with('(')
                    && rest.ends_with(')')
                    && rest[1..rest.len() - 1]
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == ',' || c == ' ')
            })
    })
}

// Quoted routine bodies have no complete native dependency graph. Catalog-first
// is not a resolution proof: an exact user overload can beat an implicit built-in
// candidate. Accept only environments containing bootstrap operators in every
// effective search-path namespace, and bootstrap casts database-wide (casts do
// not have a search path). This intentionally blocks unrelated user additions,
// rather than implementing PostgreSQL's overload/unknown-literal/type-category
// resolution algorithm. OIDs below FirstNormalObjectId (16384) identify initdb
// objects; namespace alone would also admit extension/user objects in pg_catalog.
async fn pg_resolution_evidence(db: &impl QuerySession) -> Result<Value> {
    let operators = rows(
        db,
        r#"SELECT json_build_object(
        'operator',to_jsonb(o),'implementation',to_jsonb(p),
        'builtin',coalesce(o.oid<16384 AND o.oprnamespace='pg_catalog'::regnamespace
          AND o.oprleft<16384 AND o.oprright<16384 AND o.oprresult>0 AND o.oprresult<16384
          AND p.oid<16384 AND p.pronamespace='pg_catalog'::regnamespace,false))::text
        FROM pg_operator o LEFT JOIN pg_proc p ON p.oid=o.oprcode
        WHERE o.oprnamespace=ANY(current_schemas(true)::regnamespace[]) ORDER BY o.oid"#,
    )
    .await?;
    let casts=rows(db,r#"SELECT json_build_object(
        'cast',to_jsonb(c),'source_type',to_jsonb(s),'target_type',to_jsonb(t),
        'implementation',to_jsonb(p),'source_output',to_jsonb(op),'target_input',to_jsonb(ip),
        'builtin',coalesce(c.oid<16384 AND s.oid<16384 AND t.oid<16384
          AND s.typnamespace='pg_catalog'::regnamespace AND t.typnamespace='pg_catalog'::regnamespace
          AND (c.castfunc=0 OR (p.oid<16384 AND p.pronamespace='pg_catalog'::regnamespace))
          AND op.oid<16384 AND op.pronamespace='pg_catalog'::regnamespace
          AND ip.oid<16384 AND ip.pronamespace='pg_catalog'::regnamespace,false))::text
        FROM pg_cast c JOIN pg_type s ON s.oid=c.castsource JOIN pg_type t ON t.oid=c.casttarget
        LEFT JOIN pg_proc p ON p.oid=c.castfunc LEFT JOIN pg_proc op ON op.oid=s.typoutput
        LEFT JOIN pg_proc ip ON ip.oid=t.typinput ORDER BY c.oid"#).await?;
    Ok(json!({"operators":operators,"casts":casts}))
}

pub async fn inspect_definition(
    db: &Database,
    id: &ObjectIdentity,
    scope: Scope,
) -> Result<DefinitionSnapshot> {
    inspect_on(db, id, scope).await
}
async fn inspect_on(
    db: &impl QuerySession,
    id: &ObjectIdentity,
    scope: Scope,
) -> Result<DefinitionSnapshot> {
    ensure!(
        matches!(
            (scope, id.kind),
            (Scope::ViewQuery, ObjectKind::View)
                | (
                    Scope::RoutineBody,
                    ObjectKind::Function | ObjectKind::Procedure
                )
        ),
        "Scope requires an ordinary view or routine identity"
    );
    let object = object_editor::inspect_object_on(db, id).await?;
    let engine = id.engine;
    let schema = val(engine, &id.schema);
    let name = val(engine, &id.name);
    let mut evidence = json!({});
    let mut blockers = vec![];
    if engine == Engine::Postgres {
        let oid = object.catalog()["record"]["oid"]
            .as_u64()
            .or_else(|| {
                object.catalog()["record"]["oid"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
            })
            .context("Missing PG object OID")?;
        let class = if scope == Scope::ViewQuery {
            "pg_class"
        } else {
            "pg_proc"
        };
        evidence["dependencies"]=json!(rows(db,&format!("SELECT json_build_object('direction','incoming','record',to_jsonb(d),'description',pg_describe_object(d.classid,d.objid,d.objsubid))::text FROM pg_depend d WHERE d.refclassid='{class}'::regclass AND d.refobjid={oid}::oid UNION ALL SELECT json_build_object('direction','outgoing','record',to_jsonb(d),'description',pg_describe_object(d.refclassid,d.refobjid,d.refobjsubid))::text FROM pg_depend d WHERE d.classid='{class}'::regclass AND d.objid={oid}::oid ORDER BY 1")).await?);
        evidence["environment"]=one(db,"SELECT json_build_object('catalog_first',(current_schemas(true))[1]='pg_catalog','role',current_user,'session_user',session_user)::text").await?;
        evidence["locale"]=one(db,"SELECT json_build_object('encoding',pg_encoding_to_char(encoding),'collate',datcollate,'ctype',datctype,'provider',datlocprovider,'locale',to_jsonb(d)->'datlocale','icu',to_jsonb(d)->'daticulocale','version',datcollversion)::text FROM pg_database d WHERE datname=current_database()").await?;
        evidence["resolution"] = pg_resolution_evidence(db).await?;
        for category in ["operators", "casts"] {
            if evidence["resolution"][category]
                .as_array()
                .context("Missing PG resolution proof")?
                .iter()
                .any(|v| v["builtin"] != true)
            {
                blockers.push(format!("Custom/unprovable PG {category} in resolution environment: definition synchronization requires bootstrap pg_catalog operators and casts; implicit overload/cast resolution is not migrated"));
            }
        }
        if evidence["environment"]["catalog_first"] != true {
            blockers.push("PG catalog must be first in effective search path; shadowable operators/types block".into());
        }
        if evidence["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| {
                d["direction"] == "outgoing"
                    && ["e", "x"].iter().any(|v| d["record"]["deptype"] == *v)
            })
        {
            blockers
                .push("Extension-owned/dependent definitions require explicit migration".into());
        }
        if scope == Scope::ViewQuery {
            evidence["query"] = object.catalog()["query"].clone();
            evidence["outputs"] = json!(view_output_columns(&object)?);
            evidence["output_types"]=json!(rows(db,&format!("SELECT json_build_object('name',a.attname,'type',format_type(a.atttypid,a.atttypmod),'type_schema',n.nspname,'collation',co.collname,'collation_schema',cn.nspname)::text FROM pg_attribute a JOIN pg_type t ON t.oid=a.atttypid JOIN pg_namespace n ON n.oid=t.typnamespace LEFT JOIN pg_collation co ON co.oid=a.attcollation LEFT JOIN pg_namespace cn ON cn.oid=co.collnamespace WHERE a.attrelid={oid}::oid AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum")).await?);
            evidence["view_dependencies"]=json!(rows(db,&format!("SELECT json_build_object('record',to_jsonb(d),'description',pg_describe_object(d.refclassid,d.refobjid,d.refobjsubid))::text FROM pg_depend d JOIN pg_rewrite r ON r.oid=d.objid WHERE d.classid='pg_rewrite'::regclass AND r.ev_class={oid}::oid AND NOT (d.refclassid='pg_class'::regclass AND d.refobjid={oid}::oid) ORDER BY 1")).await?);
            evidence["relations"]=json!(rows(db,&format!("SELECT DISTINCT jsonb_build_object('schema',n.nspname,'name',c.relname)::text FROM pg_depend d JOIN pg_rewrite r ON r.oid=d.objid JOIN pg_class c ON d.refclassid='pg_class'::regclass AND c.oid=d.refobjid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE d.classid='pg_rewrite'::regclass AND r.ev_class={oid}::oid AND c.oid<>{oid}::oid ORDER BY 1")).await?);
            let unsupported=one(db,&format!("SELECT json_build_object('custom_dependencies',EXISTS(SELECT 1 FROM pg_depend d JOIN pg_rewrite r ON r.oid=d.objid WHERE d.classid='pg_rewrite'::regclass AND r.ev_class={oid}::oid AND d.refclassid<>'pg_class'::regclass),'extra_rules',EXISTS(SELECT 1 FROM pg_rewrite WHERE ev_class={oid}::oid AND rulename<>'_RETURN'))::text")).await?;
            if unsupported["custom_dependencies"] == true || unsupported["extra_rules"] == true {
                blockers.push("View has custom function/type/rule dependencies outside bounded projection scope".into());
            }
            evidence["unsupported"] = unsupported;
        } else {
            let p = &object.catalog()["record"];
            evidence["body"] = p["prosrc"].clone();
            evidence["language"] = object.catalog()["language"].clone();
            evidence["signature"] = json!({"arguments":p["proargtypes"],"all_types":p["proallargtypes"],"names":p["proargnames"],"modes":p["proargmodes"],"return":p["prorettype"],"set":p["proretset"],"language":object.catalog()["language"]});
            let names = p["proargnames"].as_array().cloned().unwrap_or_default();
            let parameters: Vec<String> = names
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            let count = p["pronargs"].as_u64().context("Missing argument count")?;
            evidence["positional_count"] = json!(count);
            evidence["parameters"] = json!(parameters);
            evidence["out_parameters"] = json!([]);
            evidence["types"]=json!(rows(db,&format!("SELECT json_build_object('type',format_type(t.oid,NULL),'schema',n.nspname,'kind',t.typtype)::text FROM pg_proc p CROSS JOIN LATERAL unnest(coalesce(p.proallargtypes,p.proargtypes::oid[])||ARRAY[p.prorettype]) arg(oid) JOIN pg_type t ON t.oid=arg.oid JOIN pg_namespace n ON n.oid=t.typnamespace WHERE p.oid={oid}::oid ORDER BY 1")).await?);
            if id.kind != ObjectKind::Function
                || p["proretset"] != false
                || !p["proargmodes"].is_null()
                || !p["proargdefaults"].is_null()
                || !p["prosqlbody"].is_null()
                || !p["proconfig"].is_null()
            {
                blockers.push("Only scalar quoted PG functions without defaults/OUT modes/configuration/SQL-standard bodies are supported".into());
            }
            if evidence["types"].as_array().unwrap().iter().any(|t| {
                t["schema"] != "pg_catalog"
                    || t["kind"] != "b"
                    || !t["type"].as_str().is_some_and(scalar_type)
            }) {
                blockers.push("Routine uses a nonportable/custom scalar type".into());
            }
        }
    } else {
        // Deliberately require full native grant visibility, not an empty hidden
        // information_schema projection. No account secrets are queried.
        evidence["environment"]=one(db,"SELECT JSON_OBJECT('roles',CAST(CURRENT_ROLE() AS CHAR),'mode',@@session.sql_mode,'charset',@@character_set_connection,'collation',@@collation_connection)").await?;
        if evidence["environment"]["roles"] != "NONE" {
            blockers.push(
                "Active MySQL roles are outside this definition proof; use a direct-grant session"
                    .into(),
            );
        }
        if object.origin()["sql_mode"]
            .as_str()
            .is_some_and(|s| s.split(',').any(|m| m == "HIGH_NOT_PRECEDENCE"))
        {
            blockers.push("HIGH_NOT_PRECEDENCE is outside the bounded expression grammar".into());
        }
        evidence["grants"] = mysql_grants(db, &id.schema, &id.name).await?;
        evidence["visibility"]=json!(rows(db,"SELECT JSON_OBJECT('privilege',PRIVILEGE_TYPE) FROM information_schema.USER_PRIVILEGES WHERE GRANTEE=CONCAT(QUOTE(SUBSTRING_INDEX(CURRENT_USER(),'@',1)),'@',QUOTE(SUBSTRING_INDEX(CURRENT_USER(),'@',-1))) ORDER BY PRIVILEGE_TYPE").await?);
        if !["SELECT", "SHOW VIEW"].iter().all(|required| {
            evidence["visibility"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["privilege"] == *required)
        }) {
            blockers.push("Direct global SELECT and SHOW VIEW are required to prove MySQL dependency visibility".into());
        }
        evidence["incoming_views"]=json!(rows(db,&if scope==Scope::ViewQuery{format!("SELECT JSON_OBJECT('schema',VIEW_SCHEMA,'name',VIEW_NAME) FROM information_schema.VIEW_TABLE_USAGE WHERE TABLE_SCHEMA={schema} AND TABLE_NAME={name} ORDER BY VIEW_SCHEMA,VIEW_NAME")}else{format!("SELECT JSON_OBJECT('schema',TABLE_SCHEMA,'name',TABLE_NAME) FROM information_schema.VIEW_ROUTINE_USAGE WHERE SPECIFIC_SCHEMA={schema} AND SPECIFIC_NAME={name} ORDER BY TABLE_SCHEMA,TABLE_NAME")}).await?);
        if !evidence["incoming_views"].as_array().unwrap().is_empty() {
            blockers.push("MySQL dependent views require an explicit coordinated migration".into());
        }
        if scope == Scope::ViewQuery {
            evidence["query"]=one(db,&format!("SELECT JSON_OBJECT('query',VIEW_DEFINITION) FROM information_schema.VIEWS WHERE TABLE_SCHEMA={schema} AND TABLE_NAME={name}")).await?["query"].clone();
            evidence["outputs"] = json!(view_output_columns(&object)?);
            evidence["output_types"] = mysql_columns(db, &id.schema, &id.name).await?;
            evidence["relations"]=json!(rows(db,&format!("SELECT JSON_OBJECT('schema',TABLE_SCHEMA,'name',TABLE_NAME) FROM information_schema.VIEW_TABLE_USAGE WHERE VIEW_SCHEMA={schema} AND VIEW_NAME={name} ORDER BY TABLE_SCHEMA,TABLE_NAME")).await?);
            evidence["view_dependencies"]=json!(rows(db,&format!("SELECT JSON_OBJECT('schema',SPECIFIC_SCHEMA,'name',SPECIFIC_NAME) FROM information_schema.VIEW_ROUTINE_USAGE WHERE TABLE_SCHEMA={schema} AND TABLE_NAME={name} ORDER BY SPECIFIC_SCHEMA,SPECIFIC_NAME")).await?);
            if !evidence["view_dependencies"].as_array().unwrap().is_empty() {
                blockers
                    .push("View calls stored routines; dependency migration unsupported".into());
            }
        } else {
            evidence["body"] = object.catalog()["replacement"]["native"]["body"].clone();
            evidence["language"] = json!("sql");
            for (native, current) in [
                ("sql_mode", "sql_mode"),
                ("character_set_client", "client_encoding"),
                ("collation_connection", "collation"),
            ] {
                if object.catalog()["show_create"][native] != object.origin()[current] {
                    blockers.push(format!(
                        "Routine creation {native} differs from current parse settings"
                    ));
                }
            }
            evidence["signature"]=json!(rows(db,&format!("SELECT JSON_OBJECT('ordinal',ORDINAL_POSITION,'mode',PARAMETER_MODE,'name',PARAMETER_NAME,'type',DTD_IDENTIFIER,'charset',CHARACTER_SET_NAME,'collation',COLLATION_NAME) FROM information_schema.PARAMETERS WHERE SPECIFIC_SCHEMA={schema} AND SPECIFIC_NAME={name} ORDER BY ORDINAL_POSITION")).await?);
            let params = evidence["signature"].as_array().unwrap();
            evidence["parameters"] = json!(
                params
                    .iter()
                    .filter_map(|p| p["name"].as_str())
                    .collect::<Vec<_>>()
            );
            evidence["out_parameters"] = json!(
                evidence["signature"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|p| p["mode"] == "OUT" || p["mode"] == "INOUT")
                    .filter_map(|p| p["name"].as_str())
                    .collect::<Vec<_>>()
            );
            if evidence["signature"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| !p["type"].as_str().is_some_and(scalar_type))
            {
                blockers.push(
                    "MySQL routine parameter/return type outside bounded scalar matrix".into(),
                );
            }
            if let Some(reason) = object.catalog()["replacement_blocker"].as_str() {
                blockers.push(reason.into());
            }
        }
    }
    if scope == Scope::ViewQuery {
        for col in evidence["output_types"]
            .as_array()
            .context("Missing output type proof")?
        {
            if !col["type"].as_str().is_some_and(scalar_type)
                || col.get("type_schema").is_some_and(|n| n != "pg_catalog")
                || col
                    .get("collation_schema")
                    .is_some_and(|n| !n.is_null() && n != "pg_catalog")
            {
                blockers.push("View output uses unsupported/custom type/collation".into());
            }
        }
        let parsed = grammar::view(engine, string(&evidence, "query")?);
        match parsed {
            Err(error) => blockers.push(format!("Unsupported view definition: {error:#}")),
            Ok(view) => {
                let relations = evidence["relations"]
                    .as_array()
                    .context("Missing view dependency list")?;
                if let Some((qualifier, table)) = &view.relation {
                    if qualifier.as_deref().is_some_and(|s| s != id.schema)
                        || relations.len() != 1
                        || relations[0]["schema"] != id.schema
                        || relations[0]["name"] != *table
                    {
                        blockers.push("FROM relation must be the one catalog-proven base table in the explicitly mapped namespace".into());
                    } else {
                        match table_evidence(db, &id.schema, table).await {
                            Ok(table) => {
                                evidence["table"] = table;
                            }
                            Err(error) => {
                                blockers.push(format!("Base-table proof unavailable: {error:#}"))
                            }
                        };
                    }
                } else if !relations.is_empty() {
                    blockers.push("Constant SELECT has unrepresented native dependencies".into());
                }
            }
        }
    }
    Ok(DefinitionSnapshot {
        object,
        scope,
        evidence,
        blockers,
    })
}
fn view_output_columns(object: &ObjectSnapshot) -> Result<Vec<String>> {
    object.catalog()["columns"]
        .as_array()
        .context("Missing native view columns")?
        .iter()
        .map(|v| Ok(string(v, "name")?.into()))
        .collect()
}
async fn mysql_columns(db: &impl QuerySession, schema: &str, name: &str) -> Result<Value> {
    Ok(json!(rows(db,&format!("SELECT JSON_OBJECT('name',COLUMN_NAME,'type',COLUMN_TYPE,'charset',CHARACTER_SET_NAME,'collation',COLLATION_NAME,'nullable',IS_NULLABLE,'extra',EXTRA,'generation',GENERATION_EXPRESSION) FROM information_schema.COLUMNS WHERE TABLE_SCHEMA={} AND TABLE_NAME={} ORDER BY ORDINAL_POSITION",val(db.engine(),schema),val(db.engine(),name))).await?))
}
async fn mysql_grants(db: &impl QuerySession, schema: &str, name: &str) -> Result<Value> {
    let schema = val(db.engine(), schema);
    let name = val(db.engine(), name);
    Ok(
        json!({"table":rows(db,&format!("SELECT JSON_OBJECT('user',User,'host',Host,'table',CAST(Table_priv AS CHAR),'column',CAST(Column_priv AS CHAR)) FROM mysql.tables_priv WHERE Db={schema} AND Table_name={name} ORDER BY User,Host")).await?,"columns":rows(db,&format!("SELECT JSON_OBJECT('user',User,'host',Host,'column',Column_name,'privileges',CAST(Column_priv AS CHAR)) FROM mysql.columns_priv WHERE Db={schema} AND Table_name={name} ORDER BY User,Host,Column_name")).await?,"schema":rows(db,"SELECT JSON_OBJECT('user',User,'host',Host,'database',Db,'select',Select_priv,'create_view',Create_view_priv,'show_view',Show_view_priv,'drop',Drop_priv,'alter_routine',Alter_routine_priv,'execute',Execute_priv) FROM mysql.db ORDER BY User,Host,Db").await?}),
    )
}
async fn table_evidence(db: &impl QuerySession, schema: &str, name: &str) -> Result<Value> {
    let table = format!(
        "{}.{}",
        quote_ident(db.engine(), schema),
        quote_ident(db.engine(), name)
    );
    if db.engine() == Engine::Postgres {
        let evidence=one(db,&format!("SELECT json_build_object('record',to_jsonb(c),'columns',(SELECT json_agg(json_build_object('name',a.attname,'type',format_type(a.atttypid,a.atttypmod),'schema',tn.nspname,'collation',co.collname,'collation_schema',cn.nspname,'record',to_jsonb(a)) ORDER BY a.attnum) FROM pg_attribute a JOIN pg_type t ON t.oid=a.atttypid JOIN pg_namespace tn ON tn.oid=t.typnamespace LEFT JOIN pg_collation co ON co.oid=a.attcollation LEFT JOIN pg_namespace cn ON cn.oid=co.collnamespace WHERE a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped),'inherited',EXISTS(SELECT 1 FROM pg_inherits WHERE inhrelid=c.oid OR inhparent=c.oid))::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={} AND c.relname={}",val(db.engine(),schema),val(db.engine(),name))).await?;
        ensure!(
            evidence["record"]["relkind"] == "r"
                && evidence["record"]["relpersistence"] == "p"
                && evidence["record"]["relrowsecurity"] == false
                && evidence["inherited"] == false,
            "Only permanent plain base tables without RLS/inheritance are supported"
        );
        for c in evidence["columns"]
            .as_array()
            .context("Missing table columns")?
        {
            ensure!(
                c["schema"] == "pg_catalog"
                    && c["type"].as_str().is_some_and(scalar_type)
                    && (c["collation_schema"].is_null() || c["collation_schema"] == "pg_catalog")
                    && c["record"]["attgenerated"] == "",
                "Custom types/collations/generated columns block mapped table proof"
            );
        }
        Ok(evidence)
    } else {
        let show = db.query(&format!("SHOW CREATE TABLE {table}"), 2).await?;
        ensure!(
            show.len() == 1 && !show[0].truncated && show[0].rows.len() == 1,
            "Incomplete effective table definition"
        );
        let ddl = cell_text(&show[0].rows[0], 1)?.context("Missing SHOW CREATE")?;
        ensure!(
            ddl.starts_with("CREATE TABLE "),
            "Temporary shadows/views are not mapped base tables"
        );
        let columns = mysql_columns(db, schema, name).await?;
        for c in columns.as_array().context("Missing table columns")? {
            ensure!(
                c["type"].as_str().is_some_and(scalar_type) && c["generation"] == "",
                "Unsupported/generated mapped table column"
            );
        }
        Ok(json!({"ddl":ddl,"columns":columns,"grants":mysql_grants(db,schema,name).await?}))
    }
}

fn table_shape(snapshot: &DefinitionSnapshot) -> Result<Vec<Value>> {
    let Some(table) = snapshot.evidence.get("table") else {
        return Ok(vec![]);
    };
    table["columns"].as_array().context("Missing table shape")?.iter().map(|c|Ok(json!({"name":c["name"],"type":c["type"],"charset":c["charset"],"collation":c["collation"],"collation_schema":c["collation_schema"]}))).collect()
}
fn rendered(snapshot: &DefinitionSnapshot, target_schema: &str) -> Result<String> {
    let id = snapshot.object.identity();
    let e = &snapshot.evidence;
    if snapshot.scope == Scope::ViewQuery {
        let v = grammar::view(id.engine, string(e, "query")?)?;
        let columns = table_shape(snapshot)?
            .iter()
            .map(|v| Ok(string(v, "name")?.into()))
            .collect::<Result<Vec<String>>>()?;
        v.render(
            id.engine,
            &id.schema,
            target_schema,
            &columns,
            &text_list(&e["outputs"])?,
        )
    } else {
        grammar::routine(
            id.engine,
            string(e, "language")?,
            id.kind == ObjectKind::Procedure,
            string(e, "body")?,
            &text_list(&e["parameters"])?,
            &text_list(&e["out_parameters"])?,
            e["positional_count"].as_u64().unwrap_or(0),
        )
    }
}
pub async fn compare_definitions(
    source: &Database,
    source_id: &ObjectIdentity,
    destination: &Database,
    destination_id: &ObjectIdentity,
    mapping: &NamespaceMapping,
    scope: Scope,
) -> Result<DefinitionComparison> {
    ensure!(
        source.engine() == destination.engine(),
        "Cross-engine definition synchronization unsupported"
    );
    let a = inspect_definition(source, source_id, scope).await?;
    let b = inspect_definition(destination, destination_id, scope).await?;
    compare_definition_snapshots(&a, &b, mapping)
}
pub fn compare_definition_snapshots(
    source: &DefinitionSnapshot,
    destination: &DefinitionSnapshot,
    mapping: &NamespaceMapping,
) -> Result<DefinitionComparison> {
    let a = source.object.identity();
    let b = destination.object.identity();
    let provider = crate::providers::server(b.engine, crate::providers::Operation::ObjectEdit)?;
    ensure!(
        source.scope == destination.scope
            && a.engine == b.engine
            && a.kind == b.kind
            && a.name == b.name
            && a.signature == b.signature
            && a.parent == b.parent
            && a.schema == mapping.source
            && b.schema == mapping.destination,
        "Same-engine/kind/name/signature and exact explicit namespace mapping required"
    );
    let mut result=DefinitionComparison{source:source.clone(),destination:destination.clone(),mapping:mapping.clone(),differences:vec![],blockers:vec![],warnings:vec!["Definition-only synchronization of existing objects. Destination ownership, privileges, options and comments are retained, not copied or declared equivalent. No whole-schema synchronization, inferred renames or dependency dropping. External-connection DDL races remain possible after catalog checks.".into()],plan:DdlPlan::default(),edit:None};
    result
        .blockers
        .extend(source.blockers.iter().map(|s| format!("Source: {s}")));
    result.blockers.extend(
        destination
            .blockers
            .iter()
            .map(|s| format!("Destination: {s}")),
    );
    let planned: Result<()> = (|| {
        ensure!(
            source.object.origin()["encoding"] == destination.object.origin()["encoding"],
            "Connection encodings differ"
        );
        ensure!(
            source.object.origin()["version"] == destination.object.origin()["version"],
            "Server versions differ; bounded expression semantics require matching versions"
        );
        ensure!(
            source.evidence["locale"] == destination.evidence["locale"],
            "Database locale/collation environments differ"
        );
        ensure!(
            source.evidence["resolution"] == destination.evidence["resolution"],
            "PG operator/cast implementations differ between source and destination; binding equivalence is unproven"
        );
        if a.engine == Engine::MySql {
            for setting in ["sql_mode", "client_encoding", "collation"] {
                ensure!(
                    source.object.origin()[setting] == destination.object.origin()[setting],
                    "MySQL parse setting {setting} differs"
                );
            }
        }
        if source.scope == Scope::ViewQuery {
            ensure!(
                source.evidence["outputs"] == destination.evidence["outputs"]
                    && source.evidence["output_types"] == destination.evidence["output_types"],
                "View output names/types/attributes must match exactly"
            );
            ensure!(
                table_shape(source)? == table_shape(destination)?,
                "Mapped base-table column shapes/types/collations differ"
            );
            let src = source.evidence["relations"]
                .as_array()
                .context("Missing relation proof")?;
            let dst = destination.evidence["relations"]
                .as_array()
                .context("Missing relation proof")?;
            ensure!(
                src.len() == dst.len() && src.iter().zip(dst).all(|(a, b)| a["name"] == b["name"]),
                "View relation sets differ; dependency migration unsupported"
            );
        } else {
            ensure!(
                source.evidence["signature"] == destination.evidence["signature"],
                "Routine signatures/languages/types must match exactly"
            );
        }
        let desired = rendered(source, &b.schema)?;
        let current = rendered(destination, &b.schema)?;
        if desired != current {
            result
                .differences
                .push(format!("{:?} definition differs", source.scope));
            let patch = match (source.scope, provider) {
                (Scope::ViewQuery, _) => ObjectPatch::ViewQuery { query: desired },
                (Scope::RoutineBody, crate::providers::ServerKind::Postgres) => {
                    ObjectPatch::RoutineBody { body: desired }
                }
                (Scope::RoutineBody, crate::providers::ServerKind::MySql) => {
                    ObjectPatch::MySqlBody { body: desired }
                }
            };
            let edit = object_editor::plan_edit(&destination.object, patch)?;
            result.warnings.extend(edit.plan().warnings.clone());
            result.edit = Some(edit);
        }
        Ok(())
    })();
    if let Err(error) = planned {
        result.blockers.push(format!("{error:#}"));
    }
    if result.blockers.is_empty() {
        if let Some(edit) = &result.edit {
            result.plan = edit.plan().clone();
        }
    } else {
        result.edit = None;
    }
    Ok(result)
}
pub async fn definitions_are_current(
    source: &Database,
    destination: &Database,
    comparison: &DefinitionComparison,
) -> Result<DefinitionFreshness> {
    Ok(DefinitionFreshness {
        source: inspect_definition(
            source,
            comparison.source.object.identity(),
            comparison.source.scope,
        )
        .await?
            == comparison.source,
        destination: inspect_definition(
            destination,
            comparison.destination.object.identity(),
            comparison.destination.scope,
        )
        .await?
            == comparison.destination,
    })
}
fn simple(status: DefinitionApplyStatus, detail: impl Into<String>) -> DefinitionApplyOutcome {
    DefinitionApplyOutcome {
        status,
        object_outcome: None,
        detail: Some(detail.into()),
    }
}
fn session_key(snapshot: &DefinitionSnapshot) -> String {
    let o = snapshot.object.origin();
    format!(
        "{:?}|{}|{}|{}|{}|{}",
        snapshot.object.identity().engine,
        o["uuid"],
        o["address"],
        o["port"],
        o["started"],
        o["session"]
    )
}
async fn checked(
    source: &impl QuerySession,
    destination: &impl QuerySession,
    c: &DefinitionComparison,
) -> Result<Option<DefinitionApplyOutcome>> {
    if inspect_on(source, c.source.object.identity(), c.source.scope).await? != c.source {
        return Ok(Some(simple(
            DefinitionApplyStatus::StaleSource,
            "Source definition/dependencies/privileges/session changed",
        )));
    }
    if inspect_on(
        destination,
        c.destination.object.identity(),
        c.destination.scope,
    )
    .await?
        != c.destination
    {
        return Ok(Some(simple(
            DefinitionApplyStatus::StaleDestination,
            "Destination definition/dependencies/privileges/session changed",
        )));
    }
    Ok(None)
}
async fn execute_review(
    operation: AdministrativeOperation,
    c: &DefinitionComparison,
) -> Result<DefinitionApplyOutcome> {
    let Some(edit) = &c.edit else {
        operation.close().await?;
        return Ok(simple(
            DefinitionApplyStatus::Unchanged,
            "Definitions already match within selected scope",
        ));
    };
    let result = object_editor::apply_object_edit_on(operation, edit).await?;
    let status = match result.status {
        ObjectApplyStatus::Applied => DefinitionApplyStatus::Applied,
        ObjectApplyStatus::Stale => DefinitionApplyStatus::StaleDestination,
        ObjectApplyStatus::Failed => DefinitionApplyStatus::Failed,
        ObjectApplyStatus::OutcomeUnknown => DefinitionApplyStatus::OutcomeUnknown,
    };
    Ok(DefinitionApplyOutcome {
        status,
        object_outcome: Some(result),
        detail: None,
    })
}
// A second session can wait arbitrarily long for a catalog/DDL lock. Keep the
// other administrative producer alive beyond its native 30s idle bound. Once a
// pulse is sent it is always awaited, even if work completes concurrently:
// dropping an individual query future would cancel its entire lease.
async fn with_lease<T>(
    lease: &AdministrativeOperation,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::pin!(work);
    let ended = lease.monitor().wait();
    tokio::pin!(ended);
    loop {
        tokio::select! {biased;
            result=&mut work=>return result,
            result=&mut ended=>return Err(anyhow::anyhow!("Review session lease ended before paired operation: {result:?}")),
            _=tokio::time::sleep(std::time::Duration::from_secs(5))=>{
                let pulse=lease.query("SELECT 1",1);tokio::pin!(pulse);
                tokio::select!{biased;
                    result=&mut work=>{pulse.await?;return result;},
                    result=&mut pulse=>{result?;},
                }
            }
        }
    }
}
/// Explicit reviewed apply. Never call while merely inspecting or comparing.
/// Both leases are acquired in stable physical-session order; same-session
/// source/destination uses one worker, avoiding recursive mutex acquisition.
pub async fn apply_definition_comparison(
    source: &Database,
    destination: &Database,
    c: &DefinitionComparison,
) -> Result<DefinitionApplyOutcome> {
    if c.is_blocked() {
        return Ok(simple(
            DefinitionApplyStatus::Blocked,
            c.blockers.join("; "),
        ));
    }
    ensure!(
        source.engine() == c.source.object.identity().engine
            && destination.engine() == c.destination.object.identity().engine,
        "Review engine mismatch"
    );
    // Reject rebound/swapped handles before trying to lease two sessions. In
    // particular, two originally distinct endpoints cannot now be one handle.
    if object_editor::connection_origin(source).await? != *c.source.object.origin() {
        return Ok(simple(
            DefinitionApplyStatus::StaleSource,
            "Source session changed",
        ));
    }
    if object_editor::connection_origin(destination).await? != *c.destination.object.origin() {
        return Ok(simple(
            DefinitionApplyStatus::StaleDestination,
            "Destination session changed",
        ));
    }
    let a = session_key(&c.source);
    let b = session_key(&c.destination);
    let observation = Some(object_editor::presence_sql(
        c.destination.object.identity(),
    )?);
    if a == b {
        // Verify both supplied handles still name the reviewed physical session
        // before using the single lease. Different/reconnected handles reject.
        let operation = destination.administrative_operation(observation).await?;
        let fresh = checked(&operation, &operation, c).await;
        match fresh {
            Ok(None) => execute_review(operation, c).await,
            Ok(Some(outcome)) => {
                operation.close().await?;
                Ok(outcome)
            }
            Err(error) => {
                operation.close().await?;
                Err(error)
            }
        }
    } else {
        let (src, dst) = if a < b {
            let src = source.administrative_operation(None).await?;
            let dst = with_lease(&src, destination.administrative_operation(observation)).await?;
            (src, dst)
        } else {
            let dst = destination.administrative_operation(observation).await?;
            let src = with_lease(&dst, source.administrative_operation(None)).await?;
            (src, dst)
        };
        let fresh = with_lease(&src, with_lease(&dst, checked(&src, &dst, c))).await;
        let result = match fresh {
            Ok(None) => {
                let monitor = dst.monitor();
                match with_lease(&src, execute_review(dst, c)).await {
                    Ok(result) => Ok(result),
                    Err(error) => {
                        // The destination may already have entered COMMIT. Its
                        // monitor, never a guessed rollback, is authoritative.
                        match monitor.wait().await {
                            Ok(done) => {
                                let object_outcome = object_editor::applied_outcome(done);
                                let status = match object_outcome.status {
                                    ObjectApplyStatus::Applied => DefinitionApplyStatus::Applied,
                                    ObjectApplyStatus::OutcomeUnknown => {
                                        DefinitionApplyStatus::OutcomeUnknown
                                    }
                                    _ => DefinitionApplyStatus::Failed,
                                };
                                Ok(DefinitionApplyOutcome {
                                    status,
                                    object_outcome: Some(object_outcome),
                                    detail: Some(format!("Paired operation stopped: {error:#}")),
                                })
                            }
                            Err(cleanup) => Ok(simple(
                                DefinitionApplyStatus::OutcomeUnknown,
                                format!(
                                    "Paired operation stopped: {error:#}; terminal destination evidence unavailable: {cleanup:#}"
                                ),
                            )),
                        }
                    }
                }
            }
            Ok(Some(outcome)) => {
                dst.close().await?;
                Ok(outcome)
            }
            Err(error) => {
                dst.close().await?;
                Err(error)
            }
        };
        let cleanup = src.close().await;
        match (result, cleanup) {
            (Ok(mut outcome), Ok(source)) => {
                if !source.reusable {
                    outcome.detail = Some(format!(
                        "Destination outcome retained; source session retired during cleanup: {:?}",
                        source.error
                    ));
                }
                Ok(outcome)
            }
            (Ok(mut outcome), Err(error)) => {
                outcome.detail = Some(format!(
                    "Destination outcome retained; source cleanup unavailable: {error:#}"
                ));
                Ok(outcome)
            }
            (Err(error), _) => Err(error),
        }
    }
}
