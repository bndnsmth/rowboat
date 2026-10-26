//! Catalog-backed object identity and review-only, attribute-preserving patches.
//! Raw full DDL is deliberately not an input to this API.
use super::{Operation, ServerKind, operation_support, server};
use crate::{
    ddl::{DdlPlan, validate_name},
    objects::{ObjectKind, browse_objects_on, cell_text},
    query::{SpanKind, spans},
};
use anyhow::{Context, Result, bail, ensure};
use rowboat_core::{
    Cell, Engine, QuerySession,
    sql::{literal, quote_ident},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectIdentity {
    pub engine: Engine,
    pub schema: String,
    pub kind: ObjectKind,
    pub name: String,
    pub signature: Option<String>,
    pub parent: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectSnapshot {
    identity: ObjectIdentity,
    definition: String,
    catalog: Value,
    origin: Value,
    warnings: Vec<String>,
}
impl ObjectSnapshot {
    pub fn identity(&self) -> &ObjectIdentity {
        &self.identity
    }
    pub fn definition(&self) -> &str {
        &self.definition
    }
    pub fn catalog(&self) -> &Value {
        &self.catalog
    }
    pub fn origin(&self) -> &Value {
        &self.origin
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct ObjectEdit {
    original: ObjectSnapshot,
    plan: DdlPlan,
    recovery_plan: Option<DdlPlan>,
    non_atomic: bool,
}
impl ObjectEdit {
    pub fn original(&self) -> &ObjectSnapshot {
        &self.original
    }
    pub fn plan(&self) -> &DdlPlan {
        &self.plan
    }
    pub fn recovery_plan(&self) -> Option<&DdlPlan> {
        self.recovery_plan.as_ref()
    }
    pub fn non_atomic(&self) -> bool {
        self.non_atomic
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TriggerState {
    Enabled,
    Disabled,
    Replica,
    Always,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequencePatch {
    pub increment: Option<i64>,
    pub min_value: Option<i64>,
    pub max_value: Option<i64>,
    pub start: Option<i64>,
    pub cache: Option<i64>,
    pub cycle: Option<bool>,
    pub restart: Option<i64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ObjectPatch {
    ViewQuery { query: String },
    RoutineBody { body: String },
    RoutineComment { comment: String },
    RoutineSecurity { definer: bool },
    TriggerEnabled { state: TriggerState },
    Sequence { patch: SequencePatch },
    MySqlBody { body: String },
    MaterializedRefresh { concurrently: bool, with_data: bool },
    MaterializedIndex { index: crate::ddl::IndexDesign },
    MaterializedQuery { query: String },
}
fn text(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("Missing catalog field {key}"))
}
fn q(engine: Engine, s: &str) -> String {
    quote_ident(engine, s)
}
fn val(engine: Engine, s: &str) -> String {
    literal(engine, &Cell::Text(s.into()))
}
fn qualified(id: &ObjectIdentity) -> String {
    format!("{}.{}", q(id.engine, &id.schema), q(id.engine, &id.name))
}
async fn json_query(db: &impl QuerySession, sql: &str) -> Result<Value> {
    let results = db.query(sql, 2).await?;
    ensure!(
        results.len() == 1 && !results[0].truncated && results[0].rows.len() == 1,
        "Expected exactly one complete catalog object"
    );
    serde_json::from_str(&cell_text(&results[0].rows[0], 0)?.context("NULL catalog JSON")?)
        .map_err(Into::into)
}
pub(crate) async fn connection_origin(db: &impl QuerySession) -> Result<Value> {
    json_query(db, crate::providers::connection_origin_sql(db.engine())?).await
}

pub async fn list_objects(
    db: &impl QuerySession,
    schema: &str,
    kind: ObjectKind,
) -> Result<Vec<ObjectIdentity>> {
    list_objects_on(db, schema, kind).await
}
async fn list_objects_on(
    db: &impl QuerySession,
    schema: &str,
    kind: ObjectKind,
) -> Result<Vec<ObjectIdentity>> {
    operation_support(db.engine(), Operation::ObjectCatalog(kind))?;
    validate_name(schema)?;
    ensure!(
        kind != ObjectKind::Role,
        "Use the access API for accounts/roles"
    );
    if db.engine() == Engine::MySql && kind == ObjectKind::Trigger {
        let results=db.query(&format!("SELECT TRIGGER_NAME,EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA={} ORDER BY TRIGGER_NAME,EVENT_OBJECT_TABLE",val(db.engine(),schema)),100_000).await?;
        let mut out = vec![];
        for r in results {
            ensure!(!r.truncated, "Trigger catalog truncated");
            for row in r.rows {
                out.push(ObjectIdentity {
                    engine: db.engine(),
                    schema: schema.into(),
                    kind,
                    name: cell_text(&row, 0)?.context("Missing trigger name")?,
                    signature: None,
                    parent: cell_text(&row, 1)?,
                });
            }
        }
        return Ok(out);
    }
    Ok(browse_objects_on(db, kind)
        .await?
        .into_iter()
        .filter(|o| o.schema.as_deref() == Some(schema))
        .map(|o| ObjectIdentity {
            engine: db.engine(),
            schema: schema.into(),
            kind,
            name: o.name,
            signature: if db.engine() == Engine::Postgres
                && matches!(kind, ObjectKind::Function | ObjectKind::Procedure)
            {
                o.details.clone()
            } else {
                None
            },
            parent: if kind == ObjectKind::Trigger {
                o.details
            } else {
                None
            },
        })
        .collect())
}
/// Signature and parent filters are exact native catalog strings, never guessed
/// or normalized. Omit a filter to search; multiple matches are an error.
pub async fn resolve_object(
    db: &impl QuerySession,
    schema: &str,
    kind: ObjectKind,
    name: &str,
    signature: Option<&str>,
    parent: Option<&str>,
) -> Result<ObjectIdentity> {
    let matches: Vec<_> = list_objects(db, schema, kind)
        .await?
        .into_iter()
        .filter(|id| {
            id.name == name
                && signature.is_none_or(|s| id.signature.as_deref() == Some(s))
                && parent.is_none_or(|p| id.parent.as_deref() == Some(p))
        })
        .collect();
    ensure!(
        matches.len() == 1,
        "Object resolution requires exactly one match; found {} (use exact catalog signature/parent)",
        matches.len()
    );
    Ok(matches.into_iter().next().unwrap())
}
pub async fn inspect_object(db: &impl QuerySession, id: &ObjectIdentity) -> Result<ObjectSnapshot> {
    inspect_object_on(db, id).await
}
pub(crate) async fn inspect_object_on(
    db: &impl QuerySession,
    id: &ObjectIdentity,
) -> Result<ObjectSnapshot> {
    operation_support(db.engine(), Operation::ObjectCatalog(id.kind))?;
    let provider = server(db.engine(), Operation::ObjectEdit)?;
    ensure!(
        db.engine() == id.engine,
        "Object identity belongs to a different engine"
    );
    validate_name(&id.schema)?;
    validate_name(&id.name)?;
    ensure!(
        identity_present_on(db, id).await?,
        "Object identity is absent or inaccessible; resolve a fresh exact catalog identity"
    );
    let origin = connection_origin(db).await?;
    let (definition, catalog, warnings) = match provider {
        ServerKind::Postgres => inspect_pg(db, id).await?,
        ServerKind::MySql => inspect_mysql(db, id).await?,
    };
    Ok(ObjectSnapshot {
        identity: id.clone(),
        definition,
        catalog,
        origin,
        warnings,
    })
}
async fn inspect_mysql(
    db: &impl QuerySession,
    id: &ObjectIdentity,
) -> Result<(String, Value, Vec<String>)> {
    let kind = match id.kind {
        ObjectKind::View => "VIEW",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Trigger => "TRIGGER",
        _ => bail!("MySQL does not support this object kind"),
    };
    let raw = db
        .query(&format!("SHOW CREATE {kind} {}", qualified(id)), 2)
        .await?;
    ensure!(
        raw.len() == 1 && !raw[0].truncated && raw[0].rows.len() == 1,
        "Incomplete SHOW CREATE result"
    );
    let mut catalog = BTreeMap::new();
    for (i, c) in raw[0].columns.iter().enumerate() {
        catalog.insert(c.name.clone(), cell_text(&raw[0].rows[0], i)?);
    }
    let field = match id.kind {
        ObjectKind::View => "Create View",
        ObjectKind::Function => "Create Function",
        ObjectKind::Procedure => "Create Procedure",
        ObjectKind::Trigger => "SQL Original Statement",
        _ => unreachable!(),
    };
    let definition = catalog
        .get(field)
        .and_then(|s| s.clone())
        .context("SHOW CREATE definition is unavailable; full definition privileges required")?;
    let mut evidence = json!({"show_create":catalog});
    if id.kind == ObjectKind::View {
        evidence["view"]=json_query(db,&format!("SELECT JSON_OBJECT('check_option',CHECK_OPTION,'security',SECURITY_TYPE,'definer',DEFINER,'charset',CHARACTER_SET_CLIENT,'collation',COLLATION_CONNECTION) FROM information_schema.VIEWS WHERE TABLE_SCHEMA={} AND TABLE_NAME={}",val(id.engine,&id.schema),val(id.engine,&id.name))).await?;
        let rows=db.query(&format!("SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA={} AND TABLE_NAME={} ORDER BY ORDINAL_POSITION",val(id.engine,&id.schema),val(id.engine,&id.name)),100_000).await?;
        let mut columns = vec![];
        for result in rows {
            ensure!(!result.truncated, "View columns truncated");
            for row in result.rows {
                columns
                    .push(json!({"name":cell_text(&row,0)?.context("Missing view column name")?}));
            }
        }
        evidence["columns"] = json!(columns);
    }
    if matches!(
        id.kind,
        ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Trigger
    ) {
        match mysql_replacement_evidence(db, id).await {
            Ok(extra) => evidence["replacement"] = extra,
            Err(error) => {
                evidence["replacement_blocker"] =
                    json!(format!("Cannot inspect replacement fidelity: {error:#}"))
            }
        }
    }
    Ok((definition,evidence,vec!["Native SHOW CREATE includes definer/security/creation settings. MySqlBody is an explicit non-atomic replacement, not an ALTER; stored bodies are not executed during inspection.".into()]))
}

async fn inspect_pg(
    db: &impl QuerySession,
    id: &ObjectIdentity,
) -> Result<(String, Value, Vec<String>)> {
    let schema = val(id.engine, &id.schema);
    let name = val(id.engine, &id.name);
    let mut warnings = vec![];
    let mut catalog=match id.kind{
        ObjectKind::Function|ObjectKind::Procedure=>json_query(db,&format!("SELECT json_build_object('record',to_jsonb(p),'definition',pg_get_functiondef(p.oid),'language',l.lanname,'comment',obj_description(p.oid,'pg_proc'))::text FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace JOIN pg_language l ON l.oid=p.prolang WHERE n.nspname={schema} AND p.proname={name} AND p.prokind='{}' AND pg_get_function_identity_arguments(p.oid)={}",if id.kind==ObjectKind::Function{"f"}else{"p"},val(id.engine,id.signature.as_deref().context("PG routine requires identity signature")?))).await?,
        ObjectKind::Trigger=>json_query(db,&format!("SELECT json_build_object('record',to_jsonb(t),'definition',pg_get_triggerdef(t.oid,false),'comment',obj_description(t.oid,'pg_trigger'),'parent_oid',c.oid::text,'parent_owner',c.relowner::text)::text FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={schema} AND t.tgname={name} AND c.relname={} AND NOT t.tgisinternal",val(id.engine,id.parent.as_deref().context("Trigger requires parent table")?))).await?,
        ObjectKind::View|ObjectKind::MaterializedView=>json_query(db,&format!("SELECT json_build_object('record',to_jsonb(c),'query',pg_get_viewdef(c.oid,false),'comment',obj_description(c.oid,'pg_class'),'tablespace',t.spcname,'access_method',a.amname,'columns',(SELECT json_agg(json_build_object('name',attname,'record',to_jsonb(x),'default',pg_get_expr(d.adbin,d.adrelid,false),'default_record',to_jsonb(d),'comment',col_description(c.oid,x.attnum)) ORDER BY attnum) FROM pg_attribute x LEFT JOIN pg_attrdef d ON d.adrelid=x.attrelid AND d.adnum=x.attnum WHERE x.attrelid=c.oid AND x.attnum>0 AND NOT x.attisdropped))::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace LEFT JOIN pg_tablespace t ON t.oid=c.reltablespace LEFT JOIN pg_am a ON a.oid=c.relam WHERE n.nspname={schema} AND c.relname={name} AND c.relkind='{}'",if id.kind==ObjectKind::View{"v"}else{"m"})).await?,
        ObjectKind::Sequence=>json_query(db,&format!("SELECT json_build_object('record',to_jsonb(c),'sequence',to_jsonb(s),'type',format_type(s.seqtypid,NULL),'comment',obj_description(c.oid,'pg_class'),'ownership',(SELECT json_agg(json_build_object('schema',n.nspname,'table',r.relname,'column',a.attname,'dependency',d.deptype::text)) FROM pg_depend d JOIN pg_class r ON r.oid=d.refobjid JOIN pg_namespace n ON n.oid=r.relnamespace JOIN pg_attribute a ON a.attrelid=r.oid AND a.attnum=d.refobjsubid WHERE d.classid='pg_class'::regclass AND d.objid=c.oid AND d.refclassid='pg_class'::regclass AND d.deptype IN ('a','i')))::text FROM pg_sequence s JOIN pg_class c ON c.oid=s.seqrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={schema} AND c.relname={name}")).await?,
        ObjectKind::Role=>bail!("Use access APIs for roles"),
    };
    if id.kind == ObjectKind::MaterializedView {
        let oid = catalog["record"]["oid"]
            .as_u64()
            .or_else(|| {
                catalog["record"]["oid"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
            })
            .context("Missing materialized-view OID")?;
        catalog["indexes"]=json!(json_rows(db,&format!("SELECT json_build_object('name',c.relname,'definition',pg_get_indexdef(i.indexrelid),'record',to_jsonb(i),'comment',obj_description(c.oid,'pg_class'))::text FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid={oid}::oid ORDER BY c.relname")).await?);
        // Only this materialized view's verified, internally owned TOAST table
        // is recreated automatically. Other internal dependencies still block.
        catalog["dependencies"]=json!(json_rows(db,&format!("SELECT json_build_object('description',pg_describe_object(d.classid,d.objid,d.objsubid),'type',d.deptype::text)::text FROM pg_depend d WHERE d.refclassid='pg_class'::regclass AND d.refobjid={oid}::oid AND NOT (d.classid='pg_type'::regclass AND d.objid IN (SELECT reltype FROM pg_class WHERE oid={oid}::oid)) AND NOT (d.classid='pg_rewrite'::regclass AND d.objid IN (SELECT oid FROM pg_rewrite WHERE ev_class={oid}::oid)) AND NOT (d.classid='pg_class'::regclass AND d.objid IN (SELECT indexrelid FROM pg_index WHERE indrelid={oid}::oid)) AND NOT (d.classid='pg_class'::regclass AND d.deptype='i' AND d.objsubid=0 AND d.refobjsubid=0 AND d.objid IN (SELECT toast.oid FROM pg_class mv JOIN pg_class toast ON toast.oid=mv.reltoastrelid WHERE mv.oid={oid}::oid AND mv.relkind='m' AND toast.relkind='t' AND toast.relnamespace='pg_toast'::regnamespace)) UNION ALL SELECT json_build_object('description','extension membership/dependency','type',d.deptype::text)::text FROM pg_depend d WHERE d.classid='pg_class'::regclass AND d.objid={oid}::oid AND d.refclassid='pg_extension'::regclass ORDER BY 1")).await?);
        catalog["owner"]=json_query(db,&format!("SELECT json_build_object('name',pg_get_userbyid(relowner),'is_current',relowner=(SELECT oid FROM pg_roles WHERE rolname=current_user))::text FROM pg_class WHERE oid={oid}::oid")).await?;
        catalog["replacement_limits"]=json!(json_rows(db,&format!("SELECT json_build_object('kind','column/index storage/statistics attributes')::text FROM pg_attribute a JOIN pg_type t ON t.oid=a.atttypid WHERE (a.attrelid={oid}::oid OR a.attrelid IN (SELECT indexrelid FROM pg_index WHERE indrelid={oid}::oid)) AND a.attnum>0 AND NOT a.attisdropped AND (a.attstorage<>t.typstorage OR a.attcompression<>'' OR a.attoptions IS NOT NULL OR (a.attstattarget IS NOT NULL AND a.attstattarget<>-1)) UNION ALL SELECT json_build_object('kind','security label')::text FROM pg_seclabel WHERE classoid='pg_class'::regclass AND (objoid={oid}::oid OR objoid IN (SELECT indexrelid FROM pg_index WHERE indrelid={oid}::oid)) UNION ALL SELECT json_build_object('kind','creator default ACL')::text FROM pg_default_acl WHERE defaclobjtype='r' AND defaclrole=(SELECT oid FROM pg_roles WHERE rolname=current_user) AND defaclnamespace IN (0,(SELECT relnamespace FROM pg_class WHERE oid={oid}::oid)) ORDER BY 1")).await?);
    }
    if id.kind == ObjectKind::MaterializedView {
        // Native default TOAST storage is recreated; custom storage metadata is
        // not represented by pg_get_viewdef or the parent's reloptions.
        let limits=json_rows(db,&format!("SELECT json_build_object('kind','custom TOAST storage/ownership/ACL/comment/security label')::text FROM pg_class mv JOIN pg_class toast ON toast.oid=mv.reltoastrelid JOIN pg_namespace n ON n.oid=mv.relnamespace WHERE n.nspname={schema} AND mv.relname={name} AND (cardinality(toast.reloptions)>0 OR toast.relacl IS NOT NULL OR toast.relowner<>mv.relowner OR toast.reltablespace<>mv.reltablespace OR obj_description(toast.oid,'pg_class') IS NOT NULL OR EXISTS(SELECT 1 FROM pg_seclabel WHERE classoid='pg_class'::regclass AND objoid=toast.oid))")).await?;
        catalog["replacement_limits"]
            .as_array_mut()
            .context("Missing replacement limits")?
            .extend(limits);
    }
    let definition = if let Some(s) = catalog.get("definition").and_then(Value::as_str) {
        s.into()
    } else if id.kind == ObjectKind::Sequence {
        let seq = &catalog["sequence"];
        let mut sql = format!(
            "CREATE SEQUENCE {} AS {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {} CACHE {} {}CYCLE",
            qualified(id),
            text(&catalog, "type")?,
            seq["seqincrement"],
            seq["seqmin"],
            seq["seqmax"],
            seq["seqstart"],
            seq["seqcache"],
            if seq["seqcycle"] == true { "" } else { "NO " }
        );
        if let Some(owned) = catalog["ownership"].as_array().and_then(|a| a.first()) {
            if owned["dependency"] == "a" {
                sql.push_str(&format!(
                    " OWNED BY {}.{}.{}",
                    q(id.engine, &text(owned, "schema")?),
                    q(id.engine, &text(owned, "table")?),
                    q(id.engine, &text(owned, "column")?)
                ));
            } else {
                warnings.push("Identity-owned sequence: reconstructed definition is not a standalone identity migration".into());
            }
        }
        warnings.push("Sequence definition excludes the mutable current counter; owner/ACL/comment remain in catalog evidence".into());
        sql
    } else {
        let columns = column_names(&catalog)?
            .iter()
            .map(|s| q(id.engine, s))
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = format!(
            "CREATE {}VIEW {} ({columns})",
            if id.kind == ObjectKind::MaterializedView {
                "MATERIALIZED "
            } else {
                ""
            },
            qualified(id)
        );
        if id.kind == ObjectKind::MaterializedView
            && let Some(access) = catalog["access_method"].as_str()
        {
            sql.push_str(&format!(" USING {}", q(id.engine, access)));
        }
        sql.push_str(&pg_options(&catalog)?);
        if let Some(space) = catalog["tablespace"].as_str() {
            sql.push_str(&format!(" TABLESPACE {}", q(id.engine, space)));
        }
        sql.push_str(&format!(
            " AS\n{}",
            text(&catalog, "query")?.trim_end_matches(';')
        ));
        if id.kind == ObjectKind::MaterializedView {
            sql.push_str(if catalog["record"]["relispopulated"] == true {
                "\nWITH DATA"
            } else {
                "\nWITH NO DATA"
            });
        }
        warnings.push("View definition reconstructs native query/options/column names; owner/ACL/comments remain in catalog. Materialized-view indexes/data are not recreated by this text.".into());
        sql
    };
    Ok((definition, catalog, warnings))
}
fn column_names(catalog: &Value) -> Result<Vec<String>> {
    catalog["columns"]
        .as_array()
        .context("Missing view column metadata")?
        .iter()
        .map(|v| text(v, "name"))
        .collect()
}
fn pg_options(catalog: &Value) -> Result<String> {
    let Some(options) = catalog["record"]["reloptions"].as_array() else {
        return Ok(String::new());
    };
    let options = options
        .iter()
        .map(|v| {
            let (key, value) = v
                .as_str()
                .context("Nontext reloption")?
                .split_once('=')
                .context("Invalid reloption")?;
            Ok(format!(
                "{}={}",
                q(Engine::Postgres, key),
                val(Engine::Postgres, value)
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(format!(" WITH ({})", options.join(", ")))
}

pub fn plan_edit(original: &ObjectSnapshot, patch: ObjectPatch) -> Result<ObjectEdit> {
    operation_support(original.identity.engine, Operation::ObjectEdit)?;
    let id = &original.identity;
    let mut plan = DdlPlan::default();
    let destructive = matches!(&patch,ObjectPatch::Sequence{patch} if patch.restart.is_some());
    if id.engine == Engine::MySql && matches!(id.kind, ObjectKind::Function | ObjectKind::Procedure)
    {
        let native = &original.catalog["show_create"];
        ensure!(
            native["sql_mode"] == original.origin["sql_mode"]
                && native["character_set_client"] == original.origin["client_encoding"]
                && native["collation_connection"] == original.origin["collation"],
            "MySQL routine creation SQL mode/charset/collation differs from this session; ALTER must not silently change its execution settings"
        );
    }
    let sql = match patch {
        ObjectPatch::MySqlBody { body } => return plan_mysql_body(original, &body),
        ObjectPatch::MaterializedRefresh {
            concurrently,
            with_data,
        } => {
            return plan_materialized(
                original,
                MaterializedAction::Refresh {
                    concurrently,
                    with_data,
                },
            );
        }
        ObjectPatch::MaterializedIndex { index } => {
            return plan_materialized(original, MaterializedAction::Index(index));
        }
        ObjectPatch::MaterializedQuery { query } => {
            return plan_materialized(original, MaterializedAction::Query(query));
        }
        ObjectPatch::ViewQuery { query } => {
            ensure!(
                id.kind == ObjectKind::View,
                "Only ordinary views support query replacement; materialized views require an explicit dependent-object migration"
            );
            crate::query::view_query(id.engine, &query)?;
            if id.engine == Engine::Postgres {
                let columns = column_names(&original.catalog)?
                    .iter()
                    .map(|s| q(id.engine, s))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "CREATE OR REPLACE VIEW {} ({columns}){} AS (\n{query}\n)",
                    qualified(id),
                    pg_options(&original.catalog)?
                )
            } else {
                mysql_view_patch(original, &query)?
            }
        }
        ObjectPatch::RoutineBody { body } => {
            ensure!(
                id.engine == Engine::Postgres
                    && matches!(id.kind, ObjectKind::Function | ObjectKind::Procedure),
                "Routine-body replacement is supported only for PostgreSQL SQL/plpgsql quoted bodies; MySQL drop/create would lose attributes/privileges/dependency state"
            );
            ensure!(
                matches!(
                    original.catalog["language"].as_str(),
                    Some("sql" | "plpgsql")
                ),
                "Only SQL/plpgsql body patches are supported"
            );
            ensure!(!body.contains('\0'), "Routine body contains NUL");
            pg_body_patch(original, &body)?
        }
        ObjectPatch::RoutineComment { comment } => {
            ensure!(
                matches!(id.kind, ObjectKind::Function | ObjectKind::Procedure),
                "Expected a routine"
            );
            let kind = if id.kind == ObjectKind::Function {
                "FUNCTION"
            } else {
                "PROCEDURE"
            };
            if id.engine == Engine::Postgres {
                format!(
                    "COMMENT ON {kind} {}({}) IS {}",
                    qualified(id),
                    id.signature.as_deref().context("Missing signature")?,
                    val(id.engine, &comment)
                )
            } else {
                format!(
                    "ALTER {kind} {} COMMENT {}",
                    qualified(id),
                    mysql_literal(&comment)?
                )
            }
        }
        ObjectPatch::RoutineSecurity { definer } => {
            ensure!(
                matches!(id.kind, ObjectKind::Function | ObjectKind::Procedure),
                "Expected a routine"
            );
            let kind = if id.kind == ObjectKind::Function {
                "FUNCTION"
            } else {
                "PROCEDURE"
            };
            format!(
                "ALTER {kind} {}{} {}SECURITY {}",
                qualified(id),
                if id.engine == Engine::Postgres {
                    format!(
                        "({})",
                        id.signature.as_deref().context("Missing signature")?
                    )
                } else {
                    String::new()
                },
                if id.engine == Engine::MySql {
                    "SQL "
                } else {
                    ""
                },
                if definer { "DEFINER" } else { "INVOKER" }
            )
        }
        ObjectPatch::TriggerEnabled { state } => {
            ensure!(
                id.engine == Engine::Postgres && id.kind == ObjectKind::Trigger,
                "Trigger enabled-state patches are PostgreSQL-only"
            );
            format!(
                "ALTER TABLE {}.{} {} TRIGGER {}",
                q(id.engine, &id.schema),
                q(
                    id.engine,
                    id.parent.as_deref().context("Missing trigger parent")?
                ),
                match state {
                    TriggerState::Enabled => "ENABLE",
                    TriggerState::Disabled => "DISABLE",
                    TriggerState::Replica => "ENABLE REPLICA",
                    TriggerState::Always => "ENABLE ALWAYS",
                },
                q(id.engine, &id.name)
            )
        }
        ObjectPatch::Sequence { patch } => {
            ensure!(
                id.engine == Engine::Postgres && id.kind == ObjectKind::Sequence,
                "Sequence patches are PostgreSQL-only"
            );
            let mut clauses = vec![];
            if let Some(n) = patch.increment {
                ensure!(n != 0, "Sequence increment cannot be zero");
                clauses.push(format!("INCREMENT BY {n}"));
            }
            if let Some(n) = patch.min_value {
                clauses.push(format!("MINVALUE {n}"));
            }
            if let Some(n) = patch.max_value {
                clauses.push(format!("MAXVALUE {n}"));
            }
            if let Some(n) = patch.start {
                clauses.push(format!("START WITH {n}"));
            }
            if let Some(n) = patch.cache {
                ensure!(n > 0, "Sequence cache must be positive");
                clauses.push(format!("CACHE {n}"));
            }
            if let Some(n) = patch.restart {
                clauses.push(format!("RESTART WITH {n}"));
                plan.warnings.push("RESTART changes generated values and may collide with existing rows; current counter changes are excluded from stale checks".into());
            }
            if let Some(cycle) = patch.cycle {
                clauses.push(if cycle { "CYCLE" } else { "NO CYCLE" }.into());
            }
            ensure!(!clauses.is_empty(), "Empty sequence patch");
            format!("ALTER SEQUENCE {} {}", qualified(id), clauses.join(" "))
        }
    };
    plan.push(sql,destructive,"Modify only the selected object attributes, preserving identity and existing grants/dependents");
    plan.warnings.extend(original.warnings.clone());
    plan.warnings.push("Review explicitly; recheck the immutable original immediately before apply. Inspection/check do not lock concurrent DDL. Definition/body changes may affect application behavior.".into());
    if id.engine == Engine::MySql {
        plan.warnings.push(
            "MySQL object DDL implicitly commits; apply separately from data transactions".into(),
        );
    }
    Ok(ObjectEdit {
        original: original.clone(),
        plan,
        recovery_plan: None,
        non_atomic: id.engine == Engine::MySql,
    })
}
fn mysql_literal(s: &str) -> Result<String> {
    ensure!(
        !s.contains(['\\', '\0']),
        "Value requires SQL-mode-dependent quoting"
    );
    Ok(format!("'{}'", s.replace('\'', "''")))
}

async fn json_rows(db: &impl QuerySession, sql: &str) -> Result<Vec<Value>> {
    let results = db.query(sql, 100_000).await?;
    let mut rows = vec![];
    for result in results {
        ensure!(!result.truncated, "Catalog evidence truncated");
        for row in result.rows {
            rows.push(serde_json::from_str(
                &cell_text(&row, 0)?.context("NULL catalog evidence")?,
            )?);
        }
    }
    Ok(rows)
}
async fn mysql_replacement_evidence(db: &impl QuerySession, id: &ObjectIdentity) -> Result<Value> {
    let schema = val(id.engine, &id.schema);
    let name = val(id.engine, &id.name);
    let namespace=json_query(db,&format!("SELECT JSON_OBJECT('collation',DEFAULT_COLLATION_NAME) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME={schema}")).await?;
    if id.kind == ObjectKind::Trigger {
        let trigger=json_query(db,&format!("SELECT JSON_OBJECT('body',ACTION_STATEMENT,'timing',ACTION_TIMING,'event',EVENT_MANIPULATION,'parent',EVENT_OBJECT_TABLE,'order',ACTION_ORDER) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA={schema} AND TRIGGER_NAME={name}")).await?;
        let siblings=json_rows(db,&format!("SELECT JSON_OBJECT('name',TRIGGER_NAME,'order',ACTION_ORDER,'body',ACTION_STATEMENT,'definer',DEFINER) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA={schema} AND EVENT_OBJECT_TABLE={} AND ACTION_TIMING={} AND EVENT_MANIPULATION={} ORDER BY ACTION_ORDER",val(id.engine,&text(&trigger,"parent")?),val(id.engine,&text(&trigger,"timing")?),val(id.engine,&text(&trigger,"event")?))).await?;
        Ok(json!({"native":trigger,"siblings":siblings,"namespace":namespace}))
    } else {
        let kind = if id.kind == ObjectKind::Function {
            "FUNCTION"
        } else {
            "PROCEDURE"
        };
        let routine=json_query(db,&format!("SELECT JSON_OBJECT('body',ROUTINE_DEFINITION,'definer',DEFINER,'security',SECURITY_TYPE,'comment',ROUTINE_COMMENT,'created',CREATED,'altered',LAST_ALTERED) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA={schema} AND ROUTINE_NAME={name} AND ROUTINE_TYPE='{kind}'")).await?;
        let grants=json_rows(db,&format!("SELECT JSON_OBJECT('user',User,'host',Host,'privileges',CAST(Proc_priv AS CHAR)) FROM mysql.procs_priv WHERE Db={schema} AND Routine_name={name} AND Routine_type='{kind}' ORDER BY User,Host")).await?;
        let visibility=json_rows(db,"SELECT JSON_OBJECT('privilege',PRIVILEGE_TYPE) FROM information_schema.USER_PRIVILEGES WHERE GRANTEE=CONCAT(QUOTE(SUBSTRING_INDEX(CURRENT_USER(),'@',1)),'@',QUOTE(SUBSTRING_INDEX(CURRENT_USER(),'@',-1))) ORDER BY PRIVILEGE_TYPE").await?;
        Ok(json!({"native":routine,"grants":grants,"visibility":visibility,"namespace":namespace}))
    }
}

/// A deliberately bounded stored-program grammar. The generated BEGIN/END is
/// the only outer control block. Comments/strings can contain semicolons; code
/// cannot contain END, dynamic SQL, declarations or procedural control tokens.
/// The server still validates statement semantics when CREATE is explicitly run.
fn mysql_program(body: &str, is_function: bool) -> Result<String> {
    ensure!(
        !body.contains(['\\', '\r', '\0']),
        "MySQL body requires mode-independent quoting and LF line endings"
    );
    let mut cleaned = String::new();
    for span in spans(Engine::MySql, body)? {
        if span.kind == SpanKind::Comment {
            ensure!(
                !body[span.start..span.end].starts_with("/*+"),
                "Optimizer-hint comments are not supported by the bounded stored-program editor"
            );
            cleaned.push(' ');
        } else {
            cleaned.push_str(&body[span.start..span.end]);
        }
    }
    let mut inner = cleaned.trim().trim_end_matches(';').trim();
    if inner
        .get(..5)
        .is_some_and(|s| s.eq_ignore_ascii_case("BEGIN"))
        && inner.as_bytes().get(5).is_some_and(u8::is_ascii_whitespace)
    {
        ensure!(
            inner.len() >= 8
                && inner
                    .get(inner.len() - 3..)
                    .is_some_and(|s| s.eq_ignore_ascii_case("END")),
            "Expected final END"
        );
        inner = inner[5..inner.len() - 3].trim();
    }
    let tokens = spans(Engine::MySql, inner)?;
    let mut cuts = vec![0];
    let mut depth = 0usize;
    for span in &tokens {
        if span.kind != SpanKind::Code {
            continue;
        }
        let code = &inner[span.start..span.end];
        for word in code
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .filter(|w| !w.is_empty())
        {
            ensure!(
                ![
                    "BEGIN",
                    "END",
                    "CASE",
                    "IF",
                    "THEN",
                    "ELSE",
                    "ELSEIF",
                    "LOOP",
                    "WHILE",
                    "REPEAT",
                    "DECLARE",
                    "LEAVE",
                    "ITERATE",
                    "CALL",
                    "PREPARE",
                    "EXECUTE",
                    "DEALLOCATE",
                    "DELIMITER",
                    "CREATE",
                    "DROP",
                    "ALTER",
                    "GRANT",
                    "REVOKE",
                    "USE"
                ]
                .iter()
                .any(|w| word.eq_ignore_ascii_case(w)),
                "Unsupported stored-program control/DDL token {word}; use a bounded DML/SET/RETURN body"
            );
        }
        for (offset, c) in code.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    ensure!(depth > 0, "Unbalanced stored-program parentheses");
                    depth -= 1;
                }
                ';' => {
                    ensure!(depth == 0, "Statement terminator inside expression");
                    cuts.push(span.start + offset + 1);
                }
                _ => (),
            }
        }
    }
    ensure!(depth == 0, "Unbalanced stored-program parentheses");
    cuts.push(inner.len());
    let mut statements = vec![];
    for pair in cuts.windows(2) {
        let statement = inner[pair[0]..pair[1]].trim().trim_end_matches(';').trim();
        if statement.is_empty() {
            continue;
        }
        let first = statement
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        ensure!(
            matches!(
                first.as_str(),
                "SELECT" | "INSERT" | "REPLACE" | "UPDATE" | "DELETE" | "SET" | "SIGNAL" | "RETURN"
            ),
            "Unsupported stored-program statement {first}"
        );
        ensure!(
            first != "RETURN" || is_function,
            "RETURN is only supported in functions"
        );
        statements.push((first, statement));
    }
    ensure!(!statements.is_empty(), "Empty stored-program body");
    ensure!(
        !is_function
            || statements
                .last()
                .is_some_and(|(first, _)| first == "RETURN"),
        "Function body must end in RETURN"
    );
    Ok(format!(
        "BEGIN\n{}\nEND",
        statements
            .iter()
            .map(|(_, s)| format!("{s};"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

fn mysql_native_prefix(original: &ObjectSnapshot, body: &str) -> Result<String> {
    let id = original.identity();
    let prefix = original
        .definition
        .trim_end()
        .strip_suffix(body.trim_end())
        .context("Native SHOW CREATE body differs from catalog body; cannot replace faithfully")?;
    ensure!(
        !prefix.contains('\\'),
        "Native header contains SQL-mode-dependent escapes; cannot replace faithfully"
    );
    let kind = match id.kind {
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Trigger => "TRIGGER",
        _ => bail!("Expected routine/trigger"),
    };
    let mut name_start = None;
    for span in spans(Engine::MySql, prefix)? {
        if span.kind != SpanKind::Code {
            continue;
        }
        let mut offset = span.start;
        for part in prefix[span.start..span.end]
            .split_inclusive(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        {
            let word = part.trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
            if word.eq_ignore_ascii_case(kind) {
                name_start = Some(offset + word.len());
                break;
            }
            offset += part.len();
        }
        if name_start.is_some() {
            break;
        }
    }
    let start = name_start.context("Unrecognized native CREATE header")?;
    let rest = prefix[start..].trim_start();
    let start = prefix.len() - rest.len();
    let qualified = qualified(id);
    let simple = q(id.engine, &id.name);
    let tail = rest
        .strip_prefix(&qualified)
        .or_else(|| rest.strip_prefix(&simple))
        .context("Native CREATE target does not match identity")?;
    ensure!(
        tail.starts_with('(') || tail.starts_with(char::is_whitespace),
        "Unexpected native target suffix"
    );
    Ok(format!("{}{}{}", &prefix[..start], qualified, tail))
}
fn mysql_grant_statements(original: &ObjectSnapshot) -> Result<Vec<String>> {
    let id = original.identity();
    let mut out = vec![];
    if id.kind == ObjectKind::Trigger {
        return Ok(out);
    }
    let grants = original.catalog["replacement"]["grants"]
        .as_array()
        .context("Routine grants unavailable")?;
    for grant in grants {
        let mut rights = vec![];
        let mut option = false;
        for right in text(grant, "privileges")?.split(',') {
            match right.to_ascii_uppercase().as_str() {
                "EXECUTE" => rights.push("EXECUTE"),
                "ALTER ROUTINE" => rights.push("ALTER ROUTINE"),
                "GRANT" => option = true,
                _ => bail!("Unrepresented routine privilege {right}"),
            }
        }
        ensure!(
            !rights.is_empty(),
            "Grant-option-only routine ACL cannot be restored faithfully"
        );
        out.push(format!(
            "GRANT {} ON {} {} TO {}@{}{}",
            rights.join(", "),
            if id.kind == ObjectKind::Function {
                "FUNCTION"
            } else {
                "PROCEDURE"
            },
            qualified(id),
            mysql_literal(&text(grant, "user")?)?,
            mysql_literal(&text(grant, "host")?)?,
            if option { " WITH GRANT OPTION" } else { "" }
        ));
    }
    Ok(out)
}
fn plan_mysql_body(original: &ObjectSnapshot, body: &str) -> Result<ObjectEdit> {
    let id = original.identity();
    ensure!(
        id.engine == Engine::MySql
            && matches!(
                id.kind,
                ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Trigger
            ),
        "MySqlBody requires a MySQL routine or trigger"
    );
    ensure!(
        original.catalog.get("replacement_blocker").is_none(),
        "{}",
        original.catalog["replacement_blocker"]
    );
    let native = &original.catalog["show_create"];
    ensure!(
        native["sql_mode"] == original.origin["sql_mode"]
            && native["character_set_client"] == original.origin["client_encoding"]
            && native["collation_connection"] == original.origin["collation"]
            && native["Database Collation"]
                == original.catalog["replacement"]["namespace"]["collation"],
        "Native creation SQL mode/charset/database collation must match replacement session"
    );
    if id.kind != ObjectKind::Trigger {
        let privileges = original.catalog["replacement"]["visibility"]
            .as_array()
            .context("Missing privilege visibility")?;
        for needed in ["CREATE ROUTINE", "ALTER ROUTINE", "EXECUTE"] {
            ensure!(
                privileges.iter().any(|p| p["privilege"] == needed),
                "Replacement requires direct global {needed} privilege so automatic routine grants cannot introduce new creator rights"
            );
        }
    }
    let old_body = text(&original.catalog["replacement"]["native"], "body")?;
    let mut prefix = mysql_native_prefix(original, &old_body)?;
    if id.kind == ObjectKind::Trigger {
        let siblings = original.catalog["replacement"]["siblings"]
            .as_array()
            .context("Missing trigger order")?;
        let pos = siblings
            .iter()
            .position(|v| v["name"] == id.name)
            .context("Trigger missing from sibling order")?;
        // Native SHOW CREATE does not retain FOLLOWS/PRECEDES, so reproduce its
        // effective order relative to an existing sibling at replacement time.
        if let Some(next) = siblings.get(pos + 1) {
            prefix.push_str(&format!(
                " PRECEDES {} ",
                q(id.engine, &text(next, "name")?)
            ));
        } else if pos > 0 {
            prefix.push_str(&format!(
                " FOLLOWS {} ",
                q(id.engine, &text(&siblings[pos - 1], "name")?)
            ));
        }
    }
    let body = mysql_program(body, id.kind == ObjectKind::Function)?;
    let mut plan = DdlPlan::default();
    let kind = match id.kind {
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Procedure => "PROCEDURE",
        _ => "TRIGGER",
    };
    plan.push(
        format!("DROP {kind} {}", qualified(id)),
        true,
        "Explicit non-atomic replacement: object is absent until CREATE succeeds",
    );
    plan.push(
        format!("{prefix}{body}"),
        false,
        "Recreate with inspected native attributes and trigger order",
    );
    let mut recovery = DdlPlan::default();
    recovery.push(
        format!("{prefix}{old_body}"),
        false,
        "Manual recovery if replacement left the object absent; never automatically replay",
    );
    for grant in mysql_grant_statements(original)? {
        plan.push(grant.clone(), false, "Restore inspected routine privileges");
        recovery.push(
            grant,
            false,
            "Restore original routine privileges after manual recovery",
        );
    }
    plan.warnings.push("MySQL DROP/CREATE/GRANT implicitly commit separately. CREATE failure leaves the object missing; GRANT failure leaves a replacement with incomplete privileges. Concurrent calls can fail during the gap. Original recovery SQL is retained; recovery is never automatic. Grantor/timestamp audit fields cannot be preserved, though grantee rights/grant options are restored.".into());
    Ok(ObjectEdit {
        original: original.clone(),
        plan,
        recovery_plan: Some(recovery),
        non_atomic: true,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ObjectPresence {
    Present,
    Absent,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ObjectApplyStatus {
    Applied,
    Stale,
    Failed,
    OutcomeUnknown,
}
#[derive(Debug, Clone, Serialize)]
pub struct ObjectApplyOutcome {
    pub status: ObjectApplyStatus,
    pub completed_statements: usize,
    pub failed_statement: Option<usize>,
    pub rolled_back: bool,
    pub presence: ObjectPresence,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ObjectAttributeComparison {
    pub differences: Vec<String>,
    edit: ObjectEdit,
}
impl ObjectAttributeComparison {
    pub fn edit(&self) -> &ObjectEdit {
        &self.edit
    }
}
/// Existing-object attribute synchronization only, never a claim that bodies,
/// queries, owners, grants or whole schemas are equal. Namespace mapping is exact.
pub fn compare_object_attributes(
    source: &ObjectSnapshot,
    destination: &ObjectSnapshot,
    mapping: &crate::live_diff::NamespaceMapping,
) -> Result<ObjectAttributeComparison> {
    let a = source.identity();
    let b = destination.identity();
    ensure!(
        a.engine == b.engine
            && a.kind == b.kind
            && a.name == b.name
            && a.signature == b.signature
            && a.parent == b.parent
            && a.schema == mapping.source
            && b.schema == mapping.destination,
        "Attribute comparison requires explicit matching same-engine object identities and namespace mapping"
    );
    let mut patches = vec![];
    let mut differences = vec![];
    match a.kind {
        ObjectKind::Function | ObjectKind::Procedure => {
            let (comment_a, comment_b, security_a, security_b) = if a.engine == Engine::Postgres {
                (
                    source.catalog["comment"].as_str().unwrap_or(""),
                    destination.catalog["comment"].as_str().unwrap_or(""),
                    source.catalog["record"]["prosecdef"] == true,
                    destination.catalog["record"]["prosecdef"] == true,
                )
            } else {
                let x = &source.catalog["replacement"]["native"];
                let y = &destination.catalog["replacement"]["native"];
                (
                    x["comment"]
                        .as_str()
                        .context("Missing source routine attributes")?,
                    y["comment"]
                        .as_str()
                        .context("Missing destination routine attributes")?,
                    x["security"] == "DEFINER",
                    y["security"] == "DEFINER",
                )
            };
            if comment_a != comment_b {
                patches.push(ObjectPatch::RoutineComment {
                    comment: comment_a.into(),
                });
                differences.push("routine comment".into());
            }
            if security_a != security_b {
                patches.push(ObjectPatch::RoutineSecurity {
                    definer: security_a,
                });
                differences.push("routine security mode".into());
            }
        }
        ObjectKind::Trigger if a.engine == Engine::Postgres => {
            if source.catalog["record"]["tgenabled"] != destination.catalog["record"]["tgenabled"] {
                let state = match source.catalog["record"]["tgenabled"].as_str() {
                    Some("O") => TriggerState::Enabled,
                    Some("D") => TriggerState::Disabled,
                    Some("R") => TriggerState::Replica,
                    Some("A") => TriggerState::Always,
                    _ => bail!("Unknown trigger state"),
                };
                patches.push(ObjectPatch::TriggerEnabled { state });
                differences.push("trigger enabled mode".into());
            }
        }
        ObjectKind::Sequence if a.engine == Engine::Postgres => {
            ensure!(
                source.catalog["type"] == destination.catalog["type"],
                "Sequence type conversion requires explicit policy"
            );
            let x = &source.catalog["sequence"];
            let y = &destination.catalog["sequence"];
            let number = |name: &str| -> Result<Option<i64>> {
                if x[name] == y[name] {
                    Ok(None)
                } else {
                    Ok(Some(
                        x[name]
                            .as_i64()
                            .context("Missing numeric sequence attribute")?,
                    ))
                }
            };
            let patch = SequencePatch {
                increment: number("seqincrement")?,
                min_value: number("seqmin")?,
                max_value: number("seqmax")?,
                start: number("seqstart")?,
                cache: number("seqcache")?,
                cycle: if x["seqcycle"] != y["seqcycle"] {
                    Some(x["seqcycle"].as_bool().context("Missing sequence cycle")?)
                } else {
                    None
                },
                restart: None,
            };
            if [
                "seqincrement",
                "seqmin",
                "seqmax",
                "seqstart",
                "seqcache",
                "seqcycle",
            ]
            .iter()
            .any(|name| x[name] != y[name])
            {
                patches.push(ObjectPatch::Sequence { patch });
                differences.push("sequence declaration (not current counter)".into());
            }
        }
        _ => bail!(
            "Attribute comparison supports routines, PostgreSQL triggers and PostgreSQL sequences; body/query synchronization is not inferred"
        ),
    }
    let mut plan = DdlPlan::default();
    for patch in patches {
        let edit = plan_edit(destination, patch)?;
        plan.statements.extend(edit.plan.statements);
    }
    plan.warnings.push("Attribute-only synchronization; bodies/queries, ownership, grants and dependencies are retained, not compared for equivalence. MySQL implicitly commits each ALTER. Apply only after reviewing the complete plan and checking the original destination.".into());
    Ok(ObjectAttributeComparison {
        differences,
        edit: ObjectEdit {
            original: destination.clone(),
            plan,
            recovery_plan: None,
            non_atomic: b.engine == Engine::MySql,
        },
    })
}
pub(crate) async fn identity_present_on(
    db: &impl QuerySession,
    id: &ObjectIdentity,
) -> Result<bool> {
    let results = db.query(&presence_sql(id)?, 2).await?;
    ensure!(
        results.len() == 1 && !results[0].truncated && results[0].rows.len() == 1,
        "Incomplete identity presence evidence"
    );
    Ok(cell_text(&results[0].rows[0], 0)?.as_deref() == Some("present"))
}
pub(crate) fn presence_sql(id: &ObjectIdentity) -> Result<String> {
    operation_support(id.engine, Operation::ObjectCatalog(id.kind))?;
    let schema = val(id.engine, &id.schema);
    let name = val(id.engine, &id.name);
    let query = match (id.engine, id.kind) {
        (Engine::Postgres, ObjectKind::Function | ObjectKind::Procedure) => format!(
            "SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname={schema} AND p.proname={name} AND p.prokind='{}' AND pg_get_function_identity_arguments(p.oid)={}",
            if id.kind == ObjectKind::Function {
                "f"
            } else {
                "p"
            },
            val(
                id.engine,
                id.signature
                    .as_deref()
                    .context("Routine signature missing")?
            )
        ),
        (Engine::Postgres, ObjectKind::Trigger) => format!(
            "SELECT 1 FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={schema} AND t.tgname={name} AND c.relname={}",
            val(
                id.engine,
                id.parent.as_deref().context("Trigger parent missing")?
            )
        ),
        (
            Engine::Postgres,
            ObjectKind::View | ObjectKind::MaterializedView | ObjectKind::Sequence,
        ) => format!(
            "SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={schema} AND c.relname={name} AND c.relkind='{}'",
            match id.kind {
                ObjectKind::View => "v",
                ObjectKind::MaterializedView => "m",
                _ => "S",
            }
        ),
        (Engine::MySql, ObjectKind::Function | ObjectKind::Procedure) => format!(
            "SELECT 1 FROM information_schema.ROUTINES WHERE BINARY ROUTINE_SCHEMA=BINARY {schema} AND BINARY ROUTINE_NAME=BINARY {name} AND ROUTINE_TYPE='{}'",
            if id.kind == ObjectKind::Function {
                "FUNCTION"
            } else {
                "PROCEDURE"
            }
        ),
        (Engine::MySql, ObjectKind::Trigger) => format!(
            "SELECT 1 FROM information_schema.TRIGGERS WHERE BINARY TRIGGER_SCHEMA=BINARY {schema} AND BINARY TRIGGER_NAME=BINARY {name} AND BINARY EVENT_OBJECT_TABLE=BINARY {}",
            val(
                id.engine,
                id.parent.as_deref().context("Trigger parent missing")?
            )
        ),
        (Engine::MySql, ObjectKind::View) => format!(
            "SELECT 1 FROM information_schema.VIEWS WHERE BINARY TABLE_SCHEMA=BINARY {schema} AND BINARY TABLE_NAME=BINARY {name}"
        ),
        _ => bail!("Unsupported object presence kind"),
    };
    Ok(format!(
        "SELECT CASE WHEN EXISTS({query}) THEN 'present' ELSE 'absent' END"
    ))
}
enum MaterializedAction {
    Refresh { concurrently: bool, with_data: bool },
    Index(crate::ddl::IndexDesign),
    Query(String),
}
fn plan_materialized(original: &ObjectSnapshot, action: MaterializedAction) -> Result<ObjectEdit> {
    let id = original.identity();
    ensure!(
        id.engine == Engine::Postgres && id.kind == ObjectKind::MaterializedView,
        "Expected PostgreSQL materialized view"
    );
    let mut plan = DdlPlan::default();
    let indexes = original.catalog["indexes"]
        .as_array()
        .context("Missing materialized-view indexes")?;
    match action {
        MaterializedAction::Refresh {
            concurrently,
            with_data,
        } => {
            if concurrently {
                ensure!(
                    with_data && original.catalog["record"]["relispopulated"] == true,
                    "CONCURRENTLY requires an already populated view and WITH DATA"
                );
                ensure!(
                    indexes.iter().any(|i| i["record"]["indisunique"] == true
                        && i["record"]["indisvalid"] == true
                        && i["record"]["indpred"].is_null()
                        && i["record"]["indexprs"].is_null()),
                    "CONCURRENTLY requires a valid nonpartial unique index on ordinary columns"
                );
            }
            plan.push(format!("REFRESH MATERIALIZED VIEW {}{} WITH {}DATA",if concurrently{"CONCURRENTLY "}else{""},qualified(id),if with_data{""}else{"NO "}),!with_data,"Refresh materialized results; query execution and locks occur only on explicit apply");
        }
        MaterializedAction::Index(index) => {
            let columns = column_names(&original.catalog)?;
            ensure!(
                !index.columns.is_empty() && index.columns.iter().all(|n| columns.contains(n)),
                "Index must use inspected materialized-view columns"
            );
            plan = crate::ddl::preview(
                id.engine,
                &[crate::ddl::DdlOperation::CreateIndex {
                    table: rowboat_core::TableRef {
                        schema: id.schema.clone(),
                        name: id.name.clone(),
                    },
                    index,
                }],
            )?;
        }
        MaterializedAction::Query(query) => {
            crate::query::view_query(id.engine, &query)?;
            ensure!(
                original.catalog["replacement_limits"]
                    .as_array()
                    .is_some_and(Vec::is_empty),
                "Unmodeled materialized-view attributes/default privileges block replacement: {}",
                original.catalog["replacement_limits"]
            );
            ensure!(
                indexes.iter().all(|i| i["record"]["indisvalid"] == true
                    && i["record"]["indisready"] == true
                    && i["record"]["indisclustered"] == false
                    && i["record"]["indisreplident"] == false),
                "Invalid/clustered/replica-identity index needs explicit migration"
            );
            ensure!(
                original.catalog["dependencies"]
                    .as_array()
                    .is_some_and(Vec::is_empty),
                "Materialized-view replacement blocked by dependent objects: {}",
                original.catalog["dependencies"]
            );
            ensure!(
                original.catalog["owner"]["is_current"] == true
                    && original.catalog["record"]["relacl"].is_null(),
                "Materialized-view replacement requires current ownership and default ACL; explicit grants/other owners need a migration policy"
            );
            for column in original.catalog["columns"]
                .as_array()
                .context("Missing columns")?
            {
                let record = &column["record"];
                ensure!(
                    record["attacl"].is_null()
                        && record["attoptions"].is_null()
                        && (record["attstattarget"].is_null() || record["attstattarget"] == -1)
                        && column["default"].is_null(),
                    "Materialized column ACL/options/statistics/default requires explicit migration"
                );
            }
            let columns = column_names(&original.catalog)?
                .iter()
                .map(|n| q(id.engine, n))
                .collect::<Vec<_>>()
                .join(", ");
            let mut create = format!("CREATE MATERIALIZED VIEW {} ({columns})", qualified(id));
            if let Some(am) = original.catalog["access_method"].as_str() {
                create.push_str(&format!(" USING {}", q(id.engine, am)));
            }
            create.push_str(&pg_options(&original.catalog)?);
            if let Some(space) = original.catalog["tablespace"].as_str() {
                create.push_str(&format!(" TABLESPACE {}", q(id.engine, space)));
            }
            create.push_str(&format!(
                " AS (\n{query}\n) WITH {}DATA",
                if original.catalog["record"]["relispopulated"] == true {
                    ""
                } else {
                    "NO "
                }
            ));
            plan.push(format!("DROP MATERIALIZED VIEW {}",qualified(id)),true,"Replace only after dependency inspection; never CASCADE; requires atomic PG transaction");
            plan.push(
                create,
                false,
                "Recreate query with native options, names and population state",
            );
            if let Some(comment) = original.catalog["comment"].as_str() {
                plan.push(
                    format!(
                        "COMMENT ON MATERIALIZED VIEW {} IS {}",
                        qualified(id),
                        val(id.engine, comment)
                    ),
                    false,
                    "Preserve materialized-view comment",
                );
            }
            for column in original.catalog["columns"].as_array().unwrap() {
                if let Some(comment) = column["comment"].as_str() {
                    plan.push(
                        format!(
                            "COMMENT ON COLUMN {}.{} IS {}",
                            qualified(id),
                            q(id.engine, &text(column, "name")?),
                            val(id.engine, comment)
                        ),
                        false,
                        "Preserve column comment",
                    );
                }
            }
            for index in indexes {
                plan.push(
                    text(index, "definition")?,
                    false,
                    "Restore native materialized-view index",
                );
                if let Some(comment) = index["comment"].as_str() {
                    plan.push(
                        format!(
                            "COMMENT ON INDEX {}.{} IS {}",
                            q(id.engine, &id.schema),
                            q(id.engine, &text(index, "name")?),
                            val(id.engine, comment)
                        ),
                        false,
                        "Preserve index comment",
                    );
                }
            }
            plan.warnings.push("Replacement must execute in one PostgreSQL transaction (use apply_object_edit). The object OID changes. Incompatible query columns/indexes roll back; untracked application/dynamic SQL dependencies cannot be inferred.".into());
        }
    }
    Ok(ObjectEdit {
        original: original.clone(),
        plan,
        recovery_plan: None,
        non_atomic: false,
    })
}
fn pg_body_patch(original: &ObjectSnapshot, body: &str) -> Result<String> {
    let sql = &original.definition;
    let tokens = spans(Engine::Postgres, sql)?;
    let candidates: Vec<_> = tokens
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            s.kind == SpanKind::Quoted
                && sql[s.start..s.end].starts_with('$')
                && *i > 0
                && tokens[*i - 1].kind == SpanKind::Code
                && sql[tokens[*i - 1].start..tokens[*i - 1].end]
                    .trim_end()
                    .ends_with("AS")
        })
        .collect();
    ensure!(
        candidates.len() == 1,
        "Native routine has no unique quoted AS body (SQL-standard/C/internal bodies are not patchable)"
    );
    let (_, span) = candidates[0];
    let mut tag = "$rowboat_body$".to_owned();
    while body.contains(&tag) {
        tag.insert(tag.len() - 1, '_');
    }
    Ok(format!(
        "{}{}{}{}{}",
        &sql[..span.start],
        tag,
        body,
        tag,
        &sql[span.end..]
    ))
}
/// Consume the native SHOW CREATE prefix, never user DDL. Locate AS in code
/// after VIEW, respecting quoted definer/name tokens. Replace its target with
/// the immutable qualified identity and retain native ALGORITHM/DEFINER/SECURITY.
fn mysql_view_patch(original: &ObjectSnapshot, query: &str) -> Result<String> {
    let sql = &original.definition;
    // Native metadata stores creation settings; require the apply connection's
    // modes/encoding to match rather than silently reinterpret attributes.
    let view = &original.catalog["view"];
    ensure!(
        view["charset"] == original.origin["client_encoding"]
            && view["collation"] == original.origin["collation"],
        "MySQL view creation charset/collation differs from this session; select matching session settings before editing"
    );
    let mut view_word = None;
    let mut as_word = None;
    for span in spans(Engine::MySql, sql)? {
        if span.kind != SpanKind::Code {
            continue;
        }
        let bytes = sql.as_bytes();
        let mut i = span.start;
        while i < span.end {
            if bytes[i].is_ascii_alphabetic() {
                let start = i;
                while i < span.end && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let word = &sql[start..i];
                if word.eq_ignore_ascii_case("VIEW") && view_word.is_none() {
                    view_word = Some((start, i));
                } else if word.eq_ignore_ascii_case("AS") && view_word.is_some() {
                    as_word = Some((start, i));
                    break;
                }
            } else {
                i += 1;
            }
        }
        if as_word.is_some() {
            break;
        }
    }
    let (view_start, view_end) = view_word.context("Unrecognized SHOW CREATE VIEW header")?;
    let (as_start, _) = as_word.context("Unrecognized SHOW CREATE VIEW query")?;
    let target = sql[view_end..as_start].trim();
    let names = column_names(&original.catalog)?;
    validate_mysql_view_target(target, &original.identity, &names)?;
    ensure!(
        sql.starts_with("CREATE "),
        "Unrecognized SHOW CREATE prefix"
    );
    let check = match view["check_option"].as_str() {
        Some("NONE") => "",
        Some("LOCAL") => " WITH LOCAL CHECK OPTION",
        Some("CASCADED") => " WITH CASCADED CHECK OPTION",
        _ => bail!("Unknown MySQL view check option"),
    };
    let columns = names
        .iter()
        .map(|name| q(Engine::MySql, name))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "ALTER {}VIEW {} ({columns}) AS (\n{query}\n){check}",
        &sql[7..view_start],
        qualified(&original.identity)
    ))
}

/// Strict native-header grammar: quoted [schema.]name [(quoted columns)].
/// Compare decoded identifiers with the immutable identity and ordered catalog
/// column names. SHOW CREATE may add the explicit list after our first ALTER.
/// Comments, arbitrary suffixes, mismatched names and malformed lists fail closed.
fn validate_mysql_view_target(target: &str, id: &ObjectIdentity, columns: &[String]) -> Result<()> {
    #[derive(PartialEq, Eq)]
    enum Token {
        Ident(String),
        Dot,
        Open,
        Close,
        Comma,
    }
    let mut tokens = vec![];
    for span in spans(Engine::MySql, target)? {
        let text = &target[span.start..span.end];
        match span.kind {
            SpanKind::Quoted => {
                ensure!(
                    text.starts_with('`') && text.ends_with('`'),
                    "Expected native backtick-quoted view identifier"
                );
                tokens.push(Token::Ident(text[1..text.len() - 1].replace("``", "`")));
            }
            SpanKind::Code => {
                for c in text.chars() {
                    match c {
                        '.' => tokens.push(Token::Dot),
                        '(' => tokens.push(Token::Open),
                        ')' => tokens.push(Token::Close),
                        ',' => tokens.push(Token::Comma),
                        c if c.is_ascii_whitespace() => (),
                        _ => bail!("Unsupported SHOW CREATE VIEW target token"),
                    }
                }
            }
            SpanKind::Comment => bail!("Unexpected comment in SHOW CREATE VIEW target"),
        }
    }
    let mut suffix = vec![Token::Open];
    ensure!(!columns.is_empty(), "Missing native view columns");
    for (i, name) in columns.iter().enumerate() {
        if i > 0 {
            suffix.push(Token::Comma);
        }
        suffix.push(Token::Ident(name.clone()));
    }
    suffix.push(Token::Close);
    for head in [
        vec![Token::Ident(id.name.clone())],
        vec![
            Token::Ident(id.schema.clone()),
            Token::Dot,
            Token::Ident(id.name.clone()),
        ],
    ] {
        if tokens == head || tokens.iter().eq(head.iter().chain(suffix.iter())) {
            return Ok(());
        }
    }
    bail!(
        "SHOW CREATE VIEW target/column list does not match inspected identity and ordered columns"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pass5_mysql_body_boundaries_keep_statements_inside_the_generated_block() {
        for body in [
            "RETURN 1",
            "BEGIN RETURN '; END; DROP TABLE t'; END",
            "RETURN '日本語'",
            "BEGIN /* END; */ RETURN 2; END;",
            "RETURN (1 + 2)",
        ] {
            assert!(mysql_program(body, true).is_ok(), "Rejected {body}");
        }
        for body in [
            "BEGIN RETURN 1; END; DROP TABLE t",
            "RETURN 1 /*!; END; DROP TABLE t */",
            "RETURN 1 /*+ optimizer_hint */",
            "BEGIN RETURN 1; END $$",
            "RETURN 'bad\\quote'",
            "RETURN 1 -- comment\rEND",
            "BEGIN RETURN 1; END IF",
            "BEGIN RETURN 1; 日本語",
            "BEGIN PREPARE x FROM 'SELECT 1'; END",
            "RETURN (1;)",
            "RETURN 1); END; DROP TABLE t",
            "BEGIN IF 1 THEN RETURN 1; END IF; END",
            "",
            "RETURN 1\0",
        ] {
            assert!(mysql_program(body, true).is_err(), "Accepted {body}");
        }
        assert!(mysql_program("RETURN 1", false).is_err());
        assert!(mysql_program("SET NEW.value=2; INSERT INTO audit VALUES ('END;')", false).is_ok());
    }
    #[test]
    fn mysql_native_view_target_accepts_only_exact_identity_and_column_list() {
        let id = ObjectIdentity {
            engine: Engine::MySql,
            schema: "s.with.dot".into(),
            kind: ObjectKind::View,
            name: "v`name".into(),
            signature: None,
            parent: None,
        };
        let columns = vec!["id".into(), "AS,`)".into()];
        for target in [
            "`v``name`",
            "`s.with.dot`.`v``name`",
            "`v``name` (`id`, `AS,``)`)",
            "`s.with.dot` . `v``name`\n( `id` , `AS,``)` )",
        ] {
            assert!(
                validate_mysql_view_target(target, &id, &columns).is_ok(),
                "Rejected {target}"
            );
        }
        for target in [
            "`other`.`v``name` (`id`, `AS,``)`)",
            "`v``name` (`AS,``)`, `id`)",
            "`v``name` (`id`)",
            "`v``name` (`id`, `AS,``)`,)",
            "`v``name` (`id`, `AS,``)`); DROP VIEW `other`",
            "`v``name` /* unexpected */",
            "`v``name` (`id`, `renamed`)",
            "`v``name` (`id`, `AS,``)`))",
        ] {
            assert!(
                validate_mysql_view_target(target, &id, &columns).is_err(),
                "Accepted {target}"
            );
        }
    }
    #[test]
    fn view_queries_cannot_escape_the_generated_statement() {
        for engine in [Engine::Postgres, Engine::MySql] {
            for sql in [
                "SELECT 1); DROP TABLE t; --",
                "SELECT 1; INSERT INTO t VALUES(1)",
                "SELECT (1",
                "SELECT 1 /*",
                "SELECT 1 --x\r; DROP TABLE t",
            ] {
                assert!(
                    crate::query::view_query(engine, sql).is_err(),
                    "Accepted {engine:?}: {sql}"
                );
            }
            for sql in [
                "SELECT ';' AS label",
                "SELECT 1 /* parentheses ) ; are trivia */",
                "WITH x AS (SELECT 1 AS id) SELECT id FROM x",
                "SELECT 1 -- final comment",
            ] {
                assert!(
                    crate::query::view_query(engine, sql).is_ok(),
                    "Rejected {engine:?}: {sql}"
                );
            }
        }
    }
    #[test]
    fn routine_body_semicolons_comments_and_dollar_tags_are_only_quoted_data() {
        let original=ObjectSnapshot{identity:ObjectIdentity{engine:Engine::Postgres,schema:"s".into(),kind:ObjectKind::Function,name:"f".into(),signature:Some("".into()),parent:None},definition:"CREATE OR REPLACE FUNCTION s.f() RETURNS integer\n LANGUAGE plpgsql\n SECURITY DEFINER\nAS $function$BEGIN RETURN 1; END$function$\n".into(),catalog:json!({"language":"plpgsql"}),origin:json!({}),warnings:vec![]};
        let body = "BEGIN -- ; $function$\nPERFORM '$rowboat_body$'; RETURN 2; END;";
        let edit = plan_edit(&original, ObjectPatch::RoutineBody { body: body.into() }).unwrap();
        let sql = &edit.plan().statements[0].sql;
        assert!(sql.contains("SECURITY DEFINER"));
        assert!(sql.contains("$rowboat_body_$"));
        for span in spans(Engine::Postgres, sql).unwrap() {
            if span.kind == SpanKind::Code {
                assert!(
                    !sql[span.start..span.end]
                        .trim_end()
                        .trim_end_matches(';')
                        .contains(';')
                );
            }
        }
        assert_eq!(edit.original(), &original);
        assert!(
            plan_edit(
                &original,
                ObjectPatch::ViewQuery {
                    query: "SELECT 1".into()
                }
            )
            .is_err()
        );
    }
}
