//! PostgreSQL/MySQL schema catalog providers; comparison policy stays in live_diff.
use super::{Operation, ServerKind, server};
use crate::live_diff::*;
use crate::{ddl::*, objects::cell_text};
use anyhow::{Context, Result, bail, ensure};
use rowboat_core::{Cell, Engine, QuerySession, TableRef, sql::literal};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn string(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Missing textual catalog field {key}"))
}
fn optional(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn boolean(v: &Value, key: &str) -> bool {
    v.get(key)
        .is_some_and(|v| v == &Value::Bool(true) || v.as_i64() == Some(1))
}
fn strings(v: &Value, key: &str) -> Result<Vec<String>> {
    v.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Missing catalog array {key}"))?
        .iter()
        .map(|s| {
            s.as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("Non-string catalog array {key}"))
        })
        .collect()
}
fn val(db: &impl QuerySession, s: &str) -> String {
    literal(db.engine(), &Cell::Text(s.into()))
}
async fn json(db: &impl QuerySession, sql: &str) -> Result<Vec<Value>> {
    let results = db.query(sql, 100_000).await?;
    let mut rows = Vec::new();
    for result in results {
        ensure!(
            !result.truncated,
            "Catalog inspection exceeded its row limit; no partial snapshot returned"
        );
        for row in result.rows {
            rows.push(serde_json::from_str(
                &cell_text(&row, 0)?.ok_or_else(|| anyhow::anyhow!("NULL JSON catalog row"))?,
            )?);
        }
    }
    Ok(rows)
}
async fn collect(
    db: &impl QuerySession,
    catalog: &mut BTreeMap<String, Vec<Value>>,
    key: &str,
    sql: String,
) -> Result<Vec<Value>> {
    let rows = json(db, &sql)
        .await
        .with_context(|| format!("Inspecting {key}"))?;
    catalog.insert(key.into(), rows.clone());
    Ok(rows)
}

pub async fn snapshot_schema(db: &impl QuerySession, schema: &str) -> Result<SchemaSnapshot> {
    validate_name(schema)?;
    let provider = server(db.engine(), Operation::SchemaInspection)?;
    let origin_sql = match provider {
        ServerKind::Postgres => {
            "SELECT json_build_object('database',current_database(),'database_oid',(SELECT oid::text FROM pg_database WHERE datname=current_database()),'address',inet_server_addr()::text,'port',inet_server_port(),'user',current_user,'version',current_setting('server_version'),'date_style',current_setting('DateStyle'),'interval_style',current_setting('IntervalStyle'),'standard_conforming_strings',current_setting('standard_conforming_strings'),'encoding',current_setting('client_encoding'))::text"
        }
        ServerKind::MySql => {
            "SELECT JSON_OBJECT('database',DATABASE(),'server_uuid',@@server_uuid,'host',@@hostname,'port',@@port,'user',CURRENT_USER(),'version',VERSION(),'sql_mode',@@session.sql_mode,'encoding',@@character_set_connection,'lower_case_table_names',@@lower_case_table_names)"
        }
    };
    let origin = json(db, origin_sql)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No connection identity"))?;
    let mut snapshot=SchemaSnapshot{engine:db.engine(),schema:schema.into(),origin,tables:vec![],unsupported:vec![],warnings:vec!["Comparison scope is supported table DDL, not data, grants, ownership migration, routines, views, or sequence values. Catalog collection and stale checks do not lock concurrent DDL.".into()],catalog:BTreeMap::new()};
    if db.engine() == Engine::Postgres && snapshot.origin["standard_conforming_strings"] != "on" {
        snapshot.unsupported.push("Inspection requires standard_conforming_strings=on so deparsed defaults have unambiguous literal semantics".into());
    }
    if db.engine() == Engine::MySql && snapshot.origin["lower_case_table_names"].as_u64() != Some(0)
    {
        snapshot.unsupported.push("Nonzero lower_case_table_names requires an explicit case-folding mapping not supported by this comparator".into());
    }
    match provider {
        ServerKind::Postgres => postgres(db, &mut snapshot).await?,
        ServerKind::MySql => mysql(db, &mut snapshot).await?,
    };
    snapshot.tables.sort_by(|a, b| a.name.cmp(&b.name));
    snapshot.unsupported.sort();
    snapshot.unsupported.dedup();
    Ok(snapshot)
}

async fn postgres(db: &impl QuerySession, s: &mut SchemaSnapshot) -> Result<()> {
    let schema = val(db, &s.schema);
    let ns=collect(db,&mut s.catalog,"namespace",format!("SELECT json_build_object('oid',oid::text,'name',nspname,'owner',pg_get_userbyid(nspowner),'usage',has_schema_privilege(oid,'USAGE'))::text FROM pg_namespace WHERE nspname={schema}")).await?;
    ensure!(ns.len() == 1, "Schema does not exist or is inaccessible");
    ensure!(
        boolean(&ns[0], "usage"),
        "Schema USAGE privilege is required for inspection"
    );
    let defaults=collect(db,&mut s.catalog,"default_acl",format!("SELECT json_build_object('namespace',defaclnamespace::text,'role',defaclrole::text,'acl',defaclacl::text)::text FROM pg_default_acl WHERE defaclobjtype='r' AND defaclrole=(SELECT oid FROM pg_roles WHERE rolname=current_user) AND defaclnamespace IN (0,(SELECT oid FROM pg_namespace WHERE nspname={schema})) ORDER BY defaclnamespace")).await?;
    if !defaults.is_empty() {
        s.unsupported
            .push("Creator default table privileges require explicit ACL migration policy".into());
    }
    let non_tables=collect(db,&mut s.catalog,"non_tables",format!("SELECT json_build_object('oid',c.oid::text,'name',c.relname,'kind',c.relkind::text)::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={schema} AND c.relkind NOT IN ('r','i','I','t') ORDER BY c.relname")).await?;
    for v in non_tables {
        s.unsupported.push(format!(
            "Object {} is outside the supported ordinary-table scope (kind {})",
            string(&v, "name")?,
            string(&v, "kind")?
        ));
    }
    let types=collect(db,&mut s.catalog,"custom_types",format!("SELECT json_build_object('oid',t.oid::text,'name',t.typname,'kind',t.typtype::text)::text FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace LEFT JOIN pg_type e ON e.oid=t.typelem WHERE n.nspname={schema} AND t.typrelid=0 AND COALESCE(e.typrelid,0)=0 ORDER BY t.typname")).await?;
    for v in types {
        s.unsupported.push(format!(
            "Custom type {} requires explicit type migration/mapping",
            string(&v, "name")?
        ));
    }
    let routines=collect(db,&mut s.catalog,"routines",format!("SELECT json_build_object('oid',p.oid::text,'name',p.proname,'kind',p.prokind::text)::text FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname={schema} ORDER BY p.proname,p.oid")).await?;
    if !routines.is_empty() {
        s.unsupported.push(
            "Schema contains routines/aggregates whose table dependencies are not migrated".into(),
        );
    }
    let relations=collect(db,&mut s.catalog,"tables",format!("SELECT json_build_object('oid',c.oid::text,'name',c.relname,'owner',pg_get_userbyid(c.relowner),'persistence',c.relpersistence::text,'rls',c.relrowsecurity,'force_rls',c.relforcerowsecurity,'options',c.reloptions,'tablespace',c.reltablespace,'replica_identity',c.relreplident::text,'acl',c.relacl::text,'comment',obj_description(c.oid,'pg_class'),'access',has_table_privilege(c.oid,'SELECT'),'inherited',EXISTS(SELECT 1 FROM pg_inherits WHERE inhrelid=c.oid OR inhparent=c.oid))::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={schema} AND c.relkind='r' ORDER BY c.relname")).await?;
    for relation in relations {
        let oid = string(&relation, "oid")?;
        let name = string(&relation, "name")?;
        let mut t = empty_table(&s.schema, &name);
        t.catalog.insert("relation".into(), vec![relation.clone()]);
        ensure!(
            boolean(&relation, "access"),
            "SELECT privilege is required to inspect table {name}"
        );
        if string(&relation, "persistence")? != "p"
            || boolean(&relation, "rls")
            || boolean(&relation, "force_rls")
            || boolean(&relation, "inherited")
            || relation["options"] != Value::Null
            || relation["tablespace"] != "0"
            || string(&relation, "replica_identity")? != "d"
            || relation["acl"] != Value::Null
            || relation["comment"] != Value::Null
        {
            t.unsupported.push("Table persistence/inheritance/RLS/options/tablespace/replica identity/ACL/comment is unmodeled".into());
        }
        let columns=collect(db,&mut t.catalog,"columns",format!("SELECT json_build_object('name',a.attname,'position',a.attnum,'type',format_type(a.atttypid,a.atttypmod),'type_schema',n.nspname,'type_name',ty.typname,'nullable',NOT a.attnotnull,'default',pg_get_expr(d.adbin,d.adrelid),'identity',a.attidentity::text,'generated',a.attgenerated::text,'collation',a.attcollation::text,'nondefault_collation',a.attcollation<>ty.typcollation,'comment',col_description(a.attrelid,a.attnum),'acl',a.attacl::text,'stats',a.attstattarget,'storage_changed',a.attstorage<>ty.typstorage,'compression',a.attcompression::text,'options',a.attoptions)::text FROM pg_attribute a JOIN pg_type ty ON ty.oid=a.atttypid JOIN pg_namespace n ON n.oid=ty.typnamespace LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid={oid}::oid AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum")).await?;
        let mut parsed = Vec::new();
        let mut missing_type = false;
        for c in &columns {
            let column_name = string(c, "name")?;
            let native_type = string(c, "type")?;
            let ty = match DataType::parse(Engine::Postgres, &native_type) {
                Ok(ty) => ty,
                Err(_) => {
                    missing_type = true;
                    t.unsupported.push(format!("Column {column_name} native type {native_type} needs an explicit supported type/mapping"));
                    continue;
                }
            };
            if string(c, "type_schema")? != "pg_catalog"
                || !string(c, "identity")?.is_empty()
                || !string(c, "generated")?.is_empty()
                || boolean(c, "nondefault_collation")
                || boolean(c, "storage_changed")
                || !string(c, "compression")?.is_empty()
                || c["comment"] != Value::Null
                || c["acl"] != Value::Null
                || c["options"] != Value::Null
                || (c["stats"] != Value::Null && c["stats"].as_i64() != Some(-1))
            {
                t.unsupported.push(format!("Column {column_name} has custom type/identity/generated/collation/comment/ACL/storage attributes"));
            }
            let default = optional(c, "default").map(DefaultValue::RawExpression);
            if let Some(d) = &default
                && let Err(e) = d.sql(Engine::Postgres)
            {
                t.unsupported.push(format!(
                    "Column {column_name} default is not safely renderable: {e}"
                ));
            }
            parsed.push(ColumnDesign {
                name: column_name,
                data_type: ty,
                nullable: boolean(c, "nullable"),
                default,
            });
        }
        if missing_type {
            t.definition = None;
        } else {
            t.definition.as_mut().unwrap().columns = parsed;
        }
        let constraints=collect(db,&mut t.catalog,"constraints",format!("SELECT json_build_object('name',c.conname,'kind',c.contype::text,'columns',COALESCE((SELECT json_agg(a.attname ORDER BY k.ord) FROM unnest(c.conkey) WITH ORDINALITY k(num,ord) JOIN pg_attribute a ON a.attrelid=c.conrelid AND a.attnum=k.num),'[]'::json),'ref_schema',rn.nspname,'ref_table',r.relname,'ref_columns',COALESCE((SELECT json_agg(a.attname ORDER BY k.ord) FROM unnest(c.confkey) WITH ORDINALITY k(num,ord) JOIN pg_attribute a ON a.attrelid=c.confrelid AND a.attnum=k.num),'[]'::json),'update',c.confupdtype::text,'delete',c.confdeltype::text,'expression',CASE WHEN c.contype='c' THEN pg_get_expr(c.conbin,c.conrelid) END,'definition',pg_get_constraintdef(c.oid,true),'validated',c.convalidated,'deferrable',c.condeferrable,'deferred',c.condeferred,'noinherit',c.connoinherit,'match',c.confmatchtype::text,'comment',obj_description(c.oid,'pg_constraint'))::text FROM pg_constraint c LEFT JOIN pg_class r ON r.oid=c.confrelid LEFT JOIN pg_namespace rn ON rn.oid=r.relnamespace WHERE c.conrelid={oid}::oid ORDER BY c.conname")).await?;
        for c in constraints {
            let cname = string(&c, "name")?;
            let kind = string(&c, "kind")?;
            let cols = strings(&c, "columns")?;
            if !boolean(&c, "validated")
                || boolean(&c, "deferrable")
                || boolean(&c, "deferred")
                || (matches!(kind.as_str(), "c" | "n") && boolean(&c, "noinherit"))
                || c["comment"] != Value::Null
            {
                t.unsupported.push(format!("Constraint {cname} has unmodeled validation/deferral/inheritance/comment attributes"));
            }
            match kind.as_str() {
                "p" => {
                    if let Some(d) = &mut t.definition {
                        d.primary_key = Some(PrimaryKeyDesign {
                            name: cname,
                            columns: cols,
                        });
                    }
                }
                "u" => t.uniques.push(UniqueDesign {
                    name: cname,
                    columns: cols,
                }),
                "c" => {
                    let expression = string(&c, "expression")?;
                    if let Err(e) = crate::query::validate_expression(Engine::Postgres, &expression)
                    {
                        t.unsupported
                            .push(format!("Check {cname} cannot be safely rendered: {e}"));
                    }
                    t.checks.push(CheckDesign {
                        name: cname,
                        expression,
                    });
                }
                "f" => {
                    if string(&c, "match")? != "s" {
                        t.unsupported.push(format!(
                            "Foreign key {cname} uses non-simple MATCH semantics"
                        ));
                    }
                    let fk = ForeignKeyDesign {
                        name: cname,
                        columns: cols,
                        referenced_table: TableRef {
                            schema: string(&c, "ref_schema")?,
                            name: string(&c, "ref_table")?,
                        },
                        referenced_columns: strings(&c, "ref_columns")?,
                        on_update: pg_action(&string(&c, "update")?)?,
                        on_delete: pg_action(&string(&c, "delete")?)?,
                    };
                    if fk.referenced_table.schema != s.schema {
                        t.unsupported.push(format!(
                            "Foreign key {} references an external namespace",
                            fk.name
                        ));
                    }
                    if let Some(d) = &mut t.definition {
                        d.foreign_keys.push(fk);
                    }
                }
                "n" => {
                    if cols.len() != 1 || cname != format!("{name}_{}_not_null", cols[0]) {
                        t.unsupported.push(format!("Named NOT NULL constraint {cname} is not represented by column nullability"));
                    }
                }
                _ => t
                    .unsupported
                    .push(format!("Unsupported constraint {cname} kind {kind}")),
            }
        }
        let indexes=collect(db,&mut t.catalog,"indexes",format!("SELECT json_build_object('name',ic.relname,'unique',i.indisunique,'primary',i.indisprimary,'valid',i.indisvalid,'ready',i.indisready,'nulls_not_distinct',i.indnullsnotdistinct,'method',am.amname,'predicate',pg_get_expr(i.indpred,i.indrelid),'expressions',pg_get_expr(i.indexprs,i.indrelid),'included',i.indnatts<>i.indnkeyatts,'options',ic.reloptions,'tablespace',ic.reltablespace,'comment',obj_description(ic.oid,'pg_class'),'definition',pg_get_indexdef(i.indexrelid),'constraint',EXISTS(SELECT 1 FROM pg_constraint c WHERE c.conindid=i.indexrelid AND c.conrelid=i.indrelid AND c.contype IN ('p','u','x')),'nondefault_ops',EXISTS(SELECT 1 FROM unnest(i.indclass::oid[]) x(oid) JOIN pg_opclass o ON o.oid=x.oid WHERE NOT o.opcdefault),'nondefault_sort',EXISTS(SELECT 1 FROM unnest(i.indoption::smallint[]) x(v) WHERE x.v<>0),'nondefault_collation',EXISTS(SELECT 1 FROM unnest(i.indkey::smallint[],i.indcollation::oid[]) x(num,coll) JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=x.num WHERE x.coll<>a.attcollation),'columns',COALESCE((SELECT json_agg(a.attname ORDER BY k.ord) FROM unnest(i.indkey::smallint[]) WITH ORDINALITY k(num,ord) JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=k.num),'[]'::json))::text FROM pg_index i JOIN pg_class ic ON ic.oid=i.indexrelid JOIN pg_am am ON am.oid=ic.relam WHERE i.indrelid={oid}::oid ORDER BY ic.relname")).await?;
        for i in indexes {
            let iname = string(&i, "name")?;
            if string(&i, "method")? != "btree"
                || !boolean(&i, "valid")
                || !boolean(&i, "ready")
                || boolean(&i, "nulls_not_distinct")
                || boolean(&i, "included")
                || boolean(&i, "nondefault_ops")
                || boolean(&i, "nondefault_sort")
                || boolean(&i, "nondefault_collation")
                || i["predicate"] != Value::Null
                || i["expressions"] != Value::Null
                || i["options"] != Value::Null
                || i["comment"] != Value::Null
                || i["tablespace"] != "0"
            {
                t.unsupported.push(format!("Index {iname} has unmodeled access method/expression/predicate/include/order/collation/opclass/storage attributes"));
            }
            if !boolean(&i, "constraint")
                && let Some(d) = &mut t.definition
            {
                d.indexes.push(IndexDesign {
                    name: iname,
                    columns: strings(&i, "columns")?,
                    unique: boolean(&i, "unique"),
                });
            }
        }
        let dependencies=collect(db,&mut t.catalog,"dependencies",format!("SELECT json_build_object('kind','trigger','name',tgname,'oid',oid::text)::text FROM pg_trigger WHERE tgrelid={oid}::oid AND NOT tgisinternal UNION ALL SELECT json_build_object('kind','external_fk','name',c.conname,'oid',c.oid::text)::text FROM pg_constraint c JOIN pg_class r ON r.oid=c.conrelid JOIN pg_namespace n ON n.oid=r.relnamespace WHERE c.confrelid={oid}::oid AND n.nspname<>{schema} UNION ALL SELECT json_build_object('kind','view_or_rule','name',r.rulename,'oid',r.oid::text)::text FROM pg_depend d JOIN pg_rewrite r ON r.oid=d.objid AND d.classid='pg_rewrite'::regclass WHERE d.refclassid='pg_class'::regclass AND d.refobjid={oid}::oid UNION ALL SELECT json_build_object('kind','expression_dependency','name',pg_describe_object(d.refclassid,d.refobjid,d.refobjsubid),'oid',d.objid::text)::text FROM pg_depend d WHERE ((d.classid='pg_attrdef'::regclass AND d.objid IN (SELECT oid FROM pg_attrdef WHERE adrelid={oid}::oid)) OR (d.classid='pg_constraint'::regclass AND d.objid IN (SELECT oid FROM pg_constraint WHERE conrelid={oid}::oid AND contype='c'))) AND ((d.refclassid='pg_class'::regclass AND d.refobjid<>{oid}::oid) OR (d.refclassid='pg_proc'::regclass AND d.refobjid IN (SELECT p.oid FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname<>'pg_catalog')) OR (d.refclassid='pg_type'::regclass AND d.refobjid IN (SELECT t.oid FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace WHERE n.nspname<>'pg_catalog'))) ORDER BY 1")).await?;
        for dep in dependencies {
            t.unsupported.push(format!(
                "Unmodeled {} dependency {}",
                string(&dep, "kind")?,
                string(&dep, "name")?
            ));
        }
        let advanced=collect(db,&mut t.catalog,"advanced_attributes",format!("SELECT json_build_object('kind','constraint flags/delete-column subset','name',c.conname)::text FROM pg_constraint c WHERE c.conrelid={oid}::oid AND (c.confdelsetcols IS NOT NULL OR COALESCE((to_jsonb(c)->>'conenforced')::boolean,true)=false OR COALESCE((to_jsonb(c)->>'conperiod')::boolean,false)) UNION ALL SELECT json_build_object('kind','policy','name',polname)::text FROM pg_policy WHERE polrelid={oid}::oid UNION ALL SELECT json_build_object('kind','clustered index','name',c.relname)::text FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid={oid}::oid AND i.indisclustered UNION ALL SELECT json_build_object('kind','table access method','name',am.amname)::text FROM pg_class c JOIN pg_am am ON am.oid=c.relam WHERE c.oid={oid}::oid AND am.amname<>'heap' UNION ALL SELECT json_build_object('kind','expression operator/collation dependency','name',pg_describe_object(d.refclassid,d.refobjid,d.refobjsubid))::text FROM pg_depend d WHERE ((d.classid='pg_attrdef'::regclass AND d.objid IN (SELECT oid FROM pg_attrdef WHERE adrelid={oid}::oid)) OR (d.classid='pg_constraint'::regclass AND d.objid IN (SELECT oid FROM pg_constraint WHERE conrelid={oid}::oid AND contype='c'))) AND ((d.refclassid='pg_collation'::regclass AND d.refobjid IN (SELECT c.oid FROM pg_collation c JOIN pg_namespace n ON n.oid=c.collnamespace WHERE n.nspname<>'pg_catalog')) OR (d.refclassid='pg_operator'::regclass AND d.refobjid IN (SELECT o.oid FROM pg_operator o JOIN pg_namespace n ON n.oid=o.oprnamespace WHERE n.nspname<>'pg_catalog'))) ORDER BY 1")).await?;
        for a in advanced {
            t.unsupported.push(format!(
                "Unmodeled {}: {}",
                string(&a, "kind")?,
                string(&a, "name")?
            ));
        }
        s.tables.push(t);
    }
    Ok(())
}

fn empty_table(schema: &str, name: &str) -> TableSnapshot {
    TableSnapshot {
        name: name.into(),
        definition: Some(TableDesign {
            table: TableRef {
                schema: schema.into(),
                name: name.into(),
            },
            columns: vec![],
            primary_key: None,
            indexes: vec![],
            foreign_keys: vec![],
        }),
        mysql_columns: vec![],
        mysql_options: None,
        uniques: vec![],
        checks: vec![],
        unsupported: vec![],
        catalog: BTreeMap::new(),
    }
}
fn pg_action(s: &str) -> Result<ReferentialAction> {
    Ok(match s {
        "a" => ReferentialAction::NoAction,
        "r" => ReferentialAction::Restrict,
        "c" => ReferentialAction::Cascade,
        "n" => ReferentialAction::SetNull,
        "d" => ReferentialAction::SetDefault,
        _ => bail!("Unknown PostgreSQL referential action"),
    })
}
fn mysql_action(s: &str) -> Result<ReferentialAction> {
    Ok(match s {
        "NO ACTION" => ReferentialAction::NoAction,
        "RESTRICT" => ReferentialAction::Restrict,
        "CASCADE" => ReferentialAction::Cascade,
        "SET NULL" => ReferentialAction::SetNull,
        "SET DEFAULT" => ReferentialAction::SetDefault,
        _ => bail!("Unknown MySQL referential action"),
    })
}

async fn mysql(db: &impl QuerySession, s: &mut SchemaSnapshot) -> Result<()> {
    let schema = val(db, &s.schema);
    let ns=collect(db,&mut s.catalog,"namespace",format!("SELECT JSON_OBJECT('name',SCHEMA_NAME,'charset',DEFAULT_CHARACTER_SET_NAME,'collation',DEFAULT_COLLATION_NAME) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME={schema}")).await?;
    ensure!(ns.len() == 1, "Database does not exist or is inaccessible");
    // Global visibility is necessary to detect inbound dependencies from other
    // databases. Schema-only/role-derived rights are not proof of completeness.
    let user = string(&s.origin, "user")?;
    let (name, host) = user
        .rsplit_once('@')
        .ok_or_else(|| anyhow::anyhow!("Unrecognized MySQL authenticated user"))?;
    let grantee = val(
        db,
        &format!(
            "'{}'@'{}'",
            name.replace('\'', "''"),
            host.replace('\'', "''")
        ),
    );
    let privileges=collect(db,&mut s.catalog,"visibility",format!("SELECT JSON_OBJECT('privilege',PRIVILEGE_TYPE) FROM information_schema.USER_PRIVILEGES WHERE GRANTEE={grantee} ORDER BY PRIVILEGE_TYPE")).await?;
    for needed in ["SELECT", "TRIGGER", "SHOW VIEW", "EXECUTE"] {
        if !privileges.iter().any(|p| p["privilege"] == needed) {
            s.unsupported.push(format!("Cannot prove complete inbound-dependency visibility: direct global {needed} privilege required (schema-only/role-derived grants are not inferred)"));
        }
    }
    let other=collect(db,&mut s.catalog,"non_tables",format!("SELECT JSON_OBJECT('name',TABLE_NAME,'kind',TABLE_TYPE) FROM information_schema.TABLES WHERE TABLE_SCHEMA={schema} AND TABLE_TYPE<>'BASE TABLE' UNION ALL SELECT JSON_OBJECT('name',ROUTINE_NAME,'kind',ROUTINE_TYPE) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA={schema} ORDER BY 1")).await?;
    for o in other {
        s.unsupported.push(format!(
            "Object {} ({}) is outside supported table scope",
            string(&o, "name")?,
            string(&o, "kind")?
        ));
    }
    let tables=collect(db,&mut s.catalog,"tables",format!("SELECT JSON_OBJECT('name',t.TABLE_NAME,'engine',t.ENGINE,'row_format',t.ROW_FORMAT,'collation',t.TABLE_COLLATION,'charset',c.CHARACTER_SET_NAME,'comment',t.TABLE_COMMENT,'options',t.CREATE_OPTIONS,'partitioned',EXISTS(SELECT 1 FROM information_schema.PARTITIONS p WHERE p.TABLE_SCHEMA=t.TABLE_SCHEMA AND p.TABLE_NAME=t.TABLE_NAME AND p.PARTITION_NAME IS NOT NULL)) FROM information_schema.TABLES t JOIN information_schema.COLLATION_CHARACTER_SET_APPLICABILITY c ON c.COLLATION_NAME=t.TABLE_COLLATION WHERE t.TABLE_SCHEMA={schema} AND t.TABLE_TYPE='BASE TABLE' ORDER BY t.TABLE_NAME")).await?;
    for table in tables {
        let name = string(&table, "name")?;
        let table_name = val(db, &name);
        let mut t = empty_table(&s.schema, &name);
        t.catalog.insert("relation".into(), vec![table.clone()]);
        if string(&table, "engine")? != "InnoDB"
            || string(&table, "row_format")? != "Dynamic"
            || !string(&table, "options")?.is_empty()
            || boolean(&table, "partitioned")
        {
            t.unsupported.push("Only ordinary InnoDB/Dynamic tables without engine options or partitioning are supported".into());
        }
        let comment = string(&table, "comment")?;
        if comment.contains('\\') || comment.contains('\0') {
            t.unsupported
                .push("Table comment requires SQL-mode-dependent quoting".into());
        }
        t.mysql_options = Some(MySqlTableOptions {
            character_set: string(&table, "charset")?,
            collation: string(&table, "collation")?,
            comment,
        });
        let columns=collect(db,&mut t.catalog,"columns",format!("SELECT JSON_OBJECT('name',COLUMN_NAME,'position',ORDINAL_POSITION,'type',COLUMN_TYPE,'nullable',IS_NULLABLE,'default',COLUMN_DEFAULT,'charset',CHARACTER_SET_NAME,'collation',COLLATION_NAME,'comment',COLUMN_COMMENT,'extra',EXTRA,'generation',GENERATION_EXPRESSION) FROM information_schema.COLUMNS WHERE TABLE_SCHEMA={schema} AND TABLE_NAME={table_name} ORDER BY ORDINAL_POSITION")).await?;
        let mut missing = false;
        for c in &columns {
            let column_name = string(c, "name")?;
            match mysql_column_definition(
                db,
                &TableRef {
                    schema: s.schema.clone(),
                    name: name.clone(),
                },
                &column_name,
            )
            .await
            {
                Ok(full) => {
                    if let Some(DefaultValue::Literal(Cell::Text(value))) = &full.column.default
                        && value.contains('\\')
                    {
                        t.unsupported.push(format!("Column {column_name} literal default requires SQL-mode-dependent quoting"));
                    }
                    if let Some(DefaultValue::RawExpression(value)) = &full.column.default
                        && value.contains('\\')
                    {
                        t.unsupported.push(format!("Column {column_name} default expression requires SQL-mode-dependent quoting"));
                    }
                    t.definition
                        .as_mut()
                        .unwrap()
                        .columns
                        .push(full.column.clone());
                    t.mysql_columns.push(full);
                }
                Err(e) => {
                    missing = true;
                    t.unsupported.push(format!(
                        "Column {column_name} cannot be represented without loss: {e}"
                    ));
                }
            }
        }
        if missing {
            t.definition = None;
        }
        let constraint_rows=collect(db,&mut t.catalog,"constraints",format!("SELECT JSON_OBJECT('name',t.CONSTRAINT_NAME,'kind',t.CONSTRAINT_TYPE,'enforced',t.ENFORCED,'column',k.COLUMN_NAME,'position',k.ORDINAL_POSITION,'ref_schema',k.REFERENCED_TABLE_SCHEMA,'ref_table',k.REFERENCED_TABLE_NAME,'ref_column',k.REFERENCED_COLUMN_NAME,'update',r.UPDATE_RULE,'delete',r.DELETE_RULE,'expression',c.CHECK_CLAUSE) FROM information_schema.TABLE_CONSTRAINTS t LEFT JOIN information_schema.KEY_COLUMN_USAGE k ON k.CONSTRAINT_SCHEMA=t.CONSTRAINT_SCHEMA AND k.TABLE_NAME=t.TABLE_NAME AND k.CONSTRAINT_NAME=t.CONSTRAINT_NAME LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS r ON r.CONSTRAINT_SCHEMA=t.CONSTRAINT_SCHEMA AND r.TABLE_NAME=t.TABLE_NAME AND r.CONSTRAINT_NAME=t.CONSTRAINT_NAME LEFT JOIN information_schema.CHECK_CONSTRAINTS c ON c.CONSTRAINT_SCHEMA=t.CONSTRAINT_SCHEMA AND c.CONSTRAINT_NAME=t.CONSTRAINT_NAME AND t.CONSTRAINT_TYPE='CHECK' WHERE t.TABLE_SCHEMA={schema} AND t.TABLE_NAME={table_name} ORDER BY t.CONSTRAINT_NAME,k.ORDINAL_POSITION")).await?;
        let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for c in constraint_rows {
            groups.entry(string(&c, "name")?).or_default().push(c);
        }
        for (cname, rows) in &groups {
            let c = &rows[0];
            let kind = string(c, "kind")?;
            if string(c, "enforced")? != "YES" {
                t.unsupported
                    .push(format!("Constraint {cname} is NOT ENFORCED"));
            }
            let cols = rows
                .iter()
                .filter_map(|v| optional(v, "column"))
                .collect::<Vec<_>>();
            match kind.as_str() {
                "PRIMARY KEY" => {
                    if let Some(d) = &mut t.definition {
                        d.primary_key = Some(PrimaryKeyDesign {
                            name: cname.clone(),
                            columns: cols,
                        });
                    }
                }
                "UNIQUE" => t.uniques.push(UniqueDesign {
                    name: cname.clone(),
                    columns: cols,
                }),
                "CHECK" => {
                    let expression = string(c, "expression")?;
                    if expression.contains('\\') {
                        t.unsupported
                            .push(format!("Check {cname} requires SQL-mode-dependent quoting"));
                    }
                    if let Err(e) = crate::query::validate_expression(Engine::MySql, &expression) {
                        t.unsupported
                            .push(format!("Check {cname} cannot be rendered: {e}"));
                    }
                    t.checks.push(CheckDesign {
                        name: cname.clone(),
                        expression,
                    });
                }
                "FOREIGN KEY" => {
                    let fk = ForeignKeyDesign {
                        name: cname.clone(),
                        columns: cols,
                        referenced_table: TableRef {
                            schema: string(c, "ref_schema")?,
                            name: string(c, "ref_table")?,
                        },
                        referenced_columns: rows
                            .iter()
                            .map(|v| string(v, "ref_column"))
                            .collect::<Result<_>>()?,
                        on_update: mysql_action(&string(c, "update")?)?,
                        on_delete: mysql_action(&string(c, "delete")?)?,
                    };
                    if fk.referenced_table.schema != s.schema {
                        t.unsupported.push(format!(
                            "Foreign key {cname} references an external namespace"
                        ));
                    }
                    if let Some(d) = &mut t.definition {
                        d.foreign_keys.push(fk);
                    }
                }
                _ => t
                    .unsupported
                    .push(format!("Unsupported constraint kind {kind}")),
            }
        }
        let indexes=collect(db,&mut t.catalog,"indexes",format!("SELECT JSON_OBJECT('name',INDEX_NAME,'position',SEQ_IN_INDEX,'column',COLUMN_NAME,'non_unique',NON_UNIQUE,'collation',COLLATION,'prefix',SUB_PART,'method',INDEX_TYPE,'comment',INDEX_COMMENT,'state',COMMENT,'visible',IS_VISIBLE,'expression',EXPRESSION) FROM information_schema.STATISTICS WHERE TABLE_SCHEMA={schema} AND TABLE_NAME={table_name} ORDER BY INDEX_NAME,SEQ_IN_INDEX")).await?;
        let mut idx: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for i in indexes {
            idx.entry(string(&i, "name")?).or_default().push(i);
        }
        for (iname, rows) in idx {
            for i in &rows {
                if optional(i, "column").is_none()
                    || optional(i, "collation").as_deref() != Some("A")
                    || i["prefix"] != Value::Null
                    || i["expression"] != Value::Null
                    || string(i, "method")? != "BTREE"
                    || string(i, "visible")? != "YES"
                    || !string(i, "comment")?.is_empty()
                    || !string(i, "state")?.is_empty()
                {
                    t.unsupported.push(format!("Index {iname} uses unmodeled expression/prefix/order/access method/visibility/comment attributes"));
                }
            }
            let constraint_index = groups
                .get(&iname)
                .is_some_and(|r| matches!(r[0]["kind"].as_str(), Some("PRIMARY KEY" | "UNIQUE")));
            if !constraint_index && let Some(d) = &mut t.definition {
                d.indexes.push(IndexDesign {
                    name: iname,
                    columns: rows.iter().filter_map(|i| optional(i, "column")).collect(),
                    unique: rows[0]["non_unique"].as_u64() == Some(0),
                });
            }
        }
        let dependencies=collect(db,&mut t.catalog,"dependencies",format!("SELECT JSON_OBJECT('name',TRIGGER_NAME,'kind','trigger') FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA={schema} AND EVENT_OBJECT_TABLE={table_name} UNION ALL SELECT JSON_OBJECT('name',CONSTRAINT_NAME,'kind','external_fk') FROM information_schema.KEY_COLUMN_USAGE WHERE REFERENCED_TABLE_SCHEMA={schema} AND REFERENCED_TABLE_NAME={table_name} AND TABLE_SCHEMA<>{schema} ORDER BY 1")).await?;
        for dep in dependencies {
            t.unsupported.push(format!(
                "Unmodeled {} dependency {}",
                string(&dep, "kind")?,
                string(&dep, "name")?
            ));
        }
        let views=collect(db,&mut t.catalog,"external_views",format!("SELECT JSON_OBJECT('schema',VIEW_SCHEMA,'name',VIEW_NAME) FROM information_schema.VIEW_TABLE_USAGE WHERE TABLE_SCHEMA={schema} AND TABLE_NAME={table_name} ORDER BY VIEW_SCHEMA,VIEW_NAME")).await?;
        for v in views {
            t.unsupported.push(format!(
                "Dependent view {}.{} is not migrated",
                string(&v, "schema")?,
                string(&v, "name")?
            ));
        }
        s.tables.push(t);
    }
    Ok(())
}
