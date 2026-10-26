//! Engine-specific completion discovery, always on the caller's leased session.
use super::super::*;
pub(crate) async fn server_version(conn: &mut Conn, engine: Engine) -> Result<String> {
    let sql = match engine {
        Engine::Postgres | Engine::MySql => "SELECT version()",
        Engine::Sqlite => "SELECT sqlite_version()",
    };
    let rows = raw_results(conn, sql, 1).await?;
    ensure!(
        rows.len() == 1
            && rows[0].rows.len() == 1
            && rows[0].rows[0].len() == 1
            && !rows[0].truncated,
        "Incomplete server version metadata"
    );
    Ok(text(&rows[0].rows[0][0]))
}
pub(crate) async fn catalog(
    conn: &mut Conn,
    engine: Engine,
    extra: &[String],
) -> Result<CompletionCatalog> {
    ensure!(
        extra.len() <= 128
            && extra
                .iter()
                .all(|s| !s.is_empty() && !s.contains('\0') && s.len() <= 1024),
        "Completion namespace bounds exceeded"
    );
    if let Conn::Sqlite(c) = conn {
        return c.completion_catalog(extra).await;
    }
    let sql = match engine {
        Engine::Postgres => "SELECT unnest(current_schemas(true))::text LIMIT 129",
        Engine::MySql => "SELECT DATABASE()",
        Engine::Sqlite => unreachable!(),
    };
    let rows = raw_results(conn, sql, 129).await?;
    ensure!(
        rows.len() == 1 && !rows[0].truncated && rows[0].rows.len() <= 128,
        "Completion search path exceeds bounds"
    );
    let path: Vec<_> = rows[0]
        .rows
        .iter()
        .filter_map(|r| r.first().and_then(optional_text))
        .collect();
    let mut names = path.clone();
    for name in extra {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    ensure!(
        names.len() <= 256,
        "Completion namespace count exceeds bounds"
    );
    if names.is_empty() {
        return Ok(CompletionCatalog {
            search_path: path,
            tables: Vec::new(),
        });
    }
    let placeholders = (1..=names.len())
        .map(|i| {
            if engine == Engine::Postgres {
                format!("${i}::text")
            } else {
                "?".into()
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let sql = match engine {
        Engine::Postgres => format!(
            "SELECT n.nspname,c.relname,CASE c.relkind WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' WHEN 'f' THEN 'FOREIGN TABLE' ELSE 'TABLE' END FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname IN ({placeholders}) AND c.relkind IN ('r','p','v','m','f') AND has_schema_privilege(n.oid,'USAGE') ORDER BY n.nspname,c.relname LIMIT 10001"
        ),
        Engine::MySql => format!(
            "SELECT TABLE_SCHEMA,TABLE_NAME,TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA IN ({placeholders}) ORDER BY TABLE_SCHEMA,TABLE_NAME LIMIT 10001"
        ),
        Engine::Sqlite => unreachable!(),
    };
    let params: Vec<_> = names.into_iter().map(Cell::Text).collect();
    let rows = bound_results(conn, &sql, &params, 10001).await?;
    ensure!(
        !rows.truncated && rows.rows.len() <= 10000,
        "Completion relation count exceeds 10000"
    );
    Ok(CompletionCatalog {
        search_path: path,
        tables: rows
            .rows
            .into_iter()
            .map(|r| TableInfo {
                table: TableRef {
                    schema: text(&r[0]),
                    name: text(&r[1]),
                },
                kind: text(&r[2]),
            })
            .collect(),
    })
}
pub(crate) async fn column_names(
    conn: &mut Conn,
    engine: Engine,
    table: &TableRef,
) -> Result<Vec<String>> {
    match conn {
        Conn::Sqlite(c) => c.column_names(table).await,
        Conn::Pg(_) => {
            let rows=bound_results(conn,"SELECT a.attname FROM pg_catalog.pg_attribute a WHERE a.attrelid=pg_catalog.to_regclass($1::text) AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum LIMIT 2001",&[Cell::Text(qualified_table(engine,table))],2001).await?;
            ensure!(
                !rows.truncated && rows.rows.len() <= 2000,
                "Completion columns exceed 2000"
            );
            Ok(rows.rows.iter().map(|r| text(&r[0])).collect())
        }
        Conn::My(c) => adapters::mysql::column_names(c, table).await,
        Conn::Closed => bail!("Connection closed; reconnect required"),
    }
}
