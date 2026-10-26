//! Connection-affine catalog operations. Only this layer knows catalog SQL.
use super::super::*;
use sqlx::Connection;
pub(crate) async fn browse(
    conn: &mut Conn,
    engine: Engine,
    table: &TableRef,
    request: &BrowseRequest,
    expression: Option<&FilterExpr>,
) -> Result<QueryResult> {
    if let Conn::Sqlite(c) = conn {
        return c.browse(table, request, expression).await;
    }
    let structure = structure(conn, engine, table).await?;
    let (sql, parameters, limit) = if expression.is_none() {
        browse_sql(engine, table, &structure.columns, request)?
    } else {
        browse_sql_filtered(engine, table, &structure.columns, request, expression)?
    };
    let mut result = bound_results(conn, &sql, &parameters, limit)
        .await
        .context("Browsing table")?;
    result.columns = structure.columns;
    Ok(result)
}
pub(crate) fn operation_reusable(conn: &Conn) -> bool {
    match conn {
        Conn::Sqlite(c) => c.edit_reusable,
        Conn::Closed => false,
        _ => true,
    }
}
pub(crate) async fn relationships(
    conn: &mut Conn,
    engine: Engine,
    table: &TableRef,
) -> Result<Vec<ForeignKeyRelationship>> {
    if let Conn::Sqlite(c) = conn {
        return c.relationships(table).await;
    }
    let mut params = vec![
        Cell::Text(table.schema.clone()),
        Cell::Text(table.name.clone()),
    ];
    let sql = match engine {
        Engine::Postgres => {
            r#"SELECT con.conname,n.nspname,c.relname,rn.nspname,rc.relname,a.attname,ra.attname,NOT a.attnotnull,
                 CASE con.confupdtype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' END,
                 CASE con.confdeltype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' END
                 FROM pg_constraint con JOIN pg_class c ON c.oid=con.conrelid JOIN pg_namespace n ON n.oid=c.relnamespace
                 JOIN pg_class rc ON rc.oid=con.confrelid JOIN pg_namespace rn ON rn.oid=rc.relnamespace
                 CROSS JOIN LATERAL unnest(con.conkey) WITH ORDINALITY k(num,ord)
                 JOIN pg_attribute a ON a.attrelid=c.oid AND a.attnum=k.num
                 JOIN pg_attribute ra ON ra.attrelid=rc.oid AND ra.attnum=con.confkey[k.ord::int]
                 WHERE con.contype='f' AND ((n.nspname=COALESCE(NULLIF($1::text,''),current_schema()) AND c.relname=$2::text)
                   OR (rn.nspname=COALESCE(NULLIF($1::text,''),current_schema()) AND rc.relname=$2::text))
                 ORDER BY n.nspname,c.relname,con.conname,k.ord"#
        }
        Engine::MySql => {
            params.extend(params.clone());
            "SELECT k.CONSTRAINT_NAME,k.TABLE_SCHEMA,k.TABLE_NAME,k.REFERENCED_TABLE_SCHEMA,k.REFERENCED_TABLE_NAME,k.COLUMN_NAME,k.REFERENCED_COLUMN_NAME,c.IS_NULLABLE,r.UPDATE_RULE,r.DELETE_RULE FROM information_schema.KEY_COLUMN_USAGE k JOIN information_schema.REFERENTIAL_CONSTRAINTS r ON r.CONSTRAINT_SCHEMA=k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME=k.CONSTRAINT_NAME AND r.TABLE_NAME=k.TABLE_NAME JOIN information_schema.COLUMNS c ON c.TABLE_SCHEMA=k.TABLE_SCHEMA AND c.TABLE_NAME=k.TABLE_NAME AND c.COLUMN_NAME=k.COLUMN_NAME WHERE k.REFERENCED_TABLE_NAME IS NOT NULL AND ((k.TABLE_SCHEMA=COALESCE(NULLIF(?,''),DATABASE()) AND k.TABLE_NAME=?) OR (k.REFERENCED_TABLE_SCHEMA=COALESCE(NULLIF(?,''),DATABASE()) AND k.REFERENCED_TABLE_NAME=?)) ORDER BY k.TABLE_SCHEMA,k.TABLE_NAME,k.CONSTRAINT_NAME,k.ORDINAL_POSITION"
        }
        Engine::Sqlite => bail!("Connection closed"),
    };
    let result = bound_results(conn, sql, &params, 10000)
        .await
        .context("Loading typed foreign-key relationships")?;
    ensure!(
        !result.truncated,
        "Relationship metadata exceeds the 10,000-column bound"
    );
    let mut relationships: Vec<ForeignKeyRelationship> = Vec::new();
    for row in result.rows {
        let name = text(&row[0]);
        let source = TableRef {
            schema: text(&row[1]),
            name: text(&row[2]),
        };
        let referenced = TableRef {
            schema: text(&row[3]),
            name: text(&row[4]),
        };
        if relationships
            .last()
            .is_none_or(|last| last.name != name || last.source != source)
        {
            relationships.push(ForeignKeyRelationship {
                name,
                source,
                referenced,
                columns: Vec::new(),
                on_update: text(&row[8]),
                on_delete: text(&row[9]),
            });
        }
        relationships
            .last_mut()
            .expect("relationship inserted")
            .columns
            .push(ForeignKeyColumn {
                source: text(&row[5]),
                referenced: text(&row[6]),
                nullable: truth(&row[7]),
            });
    }
    Ok(relationships)
}
async fn metadata(conn: &mut Conn, sql: &str, params: &[Cell]) -> Result<QueryResult> {
    let rows = bound_results(conn, sql, params, 10001).await?;
    ensure!(
        !rows.truncated && rows.rows.len() <= 10000,
        "Namespace catalog exceeds 10000 entries; narrow the context"
    );
    Ok(rows)
}
pub(crate) async fn namespace_snapshot(
    conn: &mut Conn,
    engine: Engine,
    namespace: Option<&str>,
) -> Result<NamespaceCatalog> {
    let (database, current_schema) = match conn {
        Conn::Sqlite(c) => {
            let namespaces = c
                .databases()
                .await?
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            return Ok(NamespaceCatalog {
                completion_proof: None,
                database: c.path.to_string_lossy().into_owned(),
                current_schema: Some("main".into()),
                namespaces,
                tables: c.tables(namespace.unwrap_or("main")).await?,
            });
        }
        Conn::Pg(c) => {
            sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT current_database(), current_schema()",
            )
            .fetch_one(c)
            .await?
        }
        Conn::My(c) => {
            c.clear_cached_statements().await?;
            (
                sqlx::query_scalar::<_, Option<String>>("SELECT DATABASE()")
                    .persistent(false)
                    .fetch_one(c)
                    .await?
                    .context("No current MySQL database; select a database first")?,
                None,
            )
        }
        Conn::Closed => bail!("Connection closed; reconnect required"),
    };
    let selected = namespace.or(match engine {
        Engine::Postgres | Engine::Sqlite => current_schema.as_deref(),
        Engine::MySql => Some(database.as_str()),
    });
    let selected_value = selected.map_or(Cell::Null, |s| Cell::Text(s.into()));
    let actual = match engine {
        Engine::Postgres | Engine::Sqlite => current_schema.clone().map_or(Cell::Null, Cell::Text),
        Engine::MySql => Cell::Text(database.clone()),
    };
    let sql = match engine {
        Engine::Postgres => {
            "SELECT nspname FROM pg_catalog.pg_namespace WHERE has_schema_privilege(oid,'USAGE') AND ((left(nspname,3)<>'pg_' AND nspname<>'information_schema') OR nspname=$1 OR nspname=$2) ORDER BY nspname LIMIT 10001"
        }
        Engine::MySql => {
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME NOT IN ('information_schema','performance_schema','mysql','sys') OR SCHEMA_NAME=? OR SCHEMA_NAME=? ORDER BY SCHEMA_NAME LIMIT 10001"
        }
        Engine::Sqlite => unreachable!(),
    };
    let namespaces: Vec<_> = metadata(conn, sql, &[selected_value.clone(), actual])
        .await?
        .rows
        .iter()
        .map(|r| text(&r[0]))
        .collect();
    if let Some(selected) = selected {
        ensure!(
            namespaces.iter().any(|n| n == selected),
            "Selected namespace does not exist or is not accessible"
        );
    }
    let sql = match engine {
        Engine::Postgres => {
            "SELECT n.nspname, c.relname, CASE c.relkind WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' WHEN 'f' THEN 'FOREIGN TABLE' WHEN 'p' THEN 'PARTITIONED TABLE' ELSE 'TABLE' END FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relkind IN ('r','p','v','m','f') AND has_schema_privilege(n.oid,'USAGE') ORDER BY c.relname LIMIT 10001"
        }
        Engine::MySql => {
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA=? ORDER BY TABLE_NAME LIMIT 10001"
        }
        Engine::Sqlite => unreachable!(),
    };
    let tables = metadata(conn, sql, &[selected_value])
        .await?
        .rows
        .into_iter()
        .map(|r| TableInfo {
            table: TableRef {
                schema: text(&r[0]),
                name: text(&r[1]),
            },
            kind: text(&r[2]),
        })
        .collect();
    Ok(NamespaceCatalog {
        completion_proof: None,
        database,
        current_schema,
        namespaces,
        tables,
    })
}
pub(crate) async fn catalog(conn: &mut Conn, engine: Engine) -> Result<Vec<TableInfo>> {
    let sql = match engine {
        Engine::Sqlite => {
            let Conn::Sqlite(c) = conn else {
                bail!("Connection closed")
            };
            return Ok(c.completion_catalog(&[]).await?.tables);
        }
        Engine::Postgres => {
            "SELECT n.nspname AS schema, c.relname AS name, CASE c.relkind WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' WHEN 'f' THEN 'FOREIGN TABLE' WHEN 'p' THEN 'PARTITIONED TABLE' ELSE 'TABLE' END AS kind FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','p','v','m','f') AND n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg_toast%' AND has_schema_privilege(n.oid,'USAGE') ORDER BY n.nspname,c.relname"
        }
        Engine::MySql => {
            "SELECT TABLE_SCHEMA AS `schema`, TABLE_NAME AS name, TABLE_TYPE AS kind FROM information_schema.TABLES WHERE TABLE_SCHEMA NOT IN ('information_schema','performance_schema','mysql','sys') ORDER BY TABLE_SCHEMA,TABLE_NAME"
        }
    };
    let rows = raw_results(conn, sql, MAX_ROWS)
        .await
        .context("Loading database catalog")?;
    Ok(rows
        .into_iter()
        .flat_map(|r| r.rows)
        .map(|r| TableInfo {
            table: TableRef {
                schema: text(&r[0]),
                name: text(&r[1]),
            },
            kind: text(&r[2]),
        })
        .collect())
}
