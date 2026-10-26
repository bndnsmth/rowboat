//! Fresh, locked engine evidence for query editing. No plan/result policy here.
use super::super::*;
use sqlx::{Executor, Row};
#[derive(Debug, Clone, Default)]
pub(crate) struct ExecutionEvidence(Option<super::sqlite::NativeSnapshot>);
impl ExecutionEvidence {
    pub(crate) fn preview(
        &self,
        target: &Target,
        result: &QueryResult,
        changes: &[RowChange],
    ) -> Result<Option<String>> {
        let Some(native) = &self.0 else {
            return Ok(None);
        };
        let table = qualified_table(Engine::Sqlite, &target.table);
        let mut statements = Vec::new();
        for change in changes {
            let RowChange::Update { original, values } = change else {
                bail!("Query results support updates of captured rows only")
            };
            let cells: Vec<_> = result
                .columns
                .iter()
                .map(|c| {
                    original
                        .iter()
                        .find(|(name, _)| name == &c.name)
                        .map(|(_, v)| v.clone())
                        .context("Missing captured column")
                })
                .collect::<Result<_>>()?;
            let mut matches = result.rows.iter().enumerate().filter(|(_, r)| *r == &cells);
            let index = matches
                .next()
                .context("Original differs from captured result")?
                .0;
            ensure!(matches.next().is_none(), "Ambiguous displayed originals");
            let row = native
                .rows
                .get(index)
                .context("Native storage evidence unavailable")?;
            ensure!(
                row.len() == result.columns.len(),
                "Native storage evidence incomplete"
            );
            let assignments = values
                .iter()
                .map(|(name, value)| {
                    format!(
                        "{} = {}",
                        quote_ident(Engine::Sqlite, name),
                        crate::sql::literal(Engine::Sqlite, value)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let predicates = result
                .columns
                .iter()
                .zip(row)
                .map(|(col, value)| {
                    let name = quote_ident(Engine::Sqlite, &col.name);
                    format!(
                        "typeof({name}) = '{}' AND {name} COLLATE BINARY IS {}",
                        value.storage(),
                        value.literal()
                    )
                })
                .collect::<Vec<_>>()
                .join(" AND ");
            statements.push(format!(
                "UPDATE {table} SET {assignments} WHERE {predicates};"
            ));
        }
        Ok(Some(statements.join("\n")))
    }
    pub(crate) fn validate(
        &self,
        target: &Target,
        result: &QueryResult,
        mapping: &[QueryEditColumn],
    ) -> Result<()> {
        if let Some(native) = &self.0 {
            ensure!(
                native.complete
                    && native.rows.len() == result.rows.len()
                    && native.origins.len() == mapping.len(),
                "Native SQLite result proof incomplete"
            );
            ensure!(
                native
                    .origins
                    .iter()
                    .zip(mapping)
                    .all(
                        |((schema, table, column), mapped)| schema == &target.table.schema
                            && table == &target.table.name
                            && column == &mapped.base_name
                    ),
                "Native SQLite column origins do not prove this projection"
            );
            ensure!(
                target
                    .columns
                    .iter()
                    .filter(|c| c.column.primary_key)
                    .all(|c| !c.column.nullable),
                "SQLite editing requires non-nullable primary keys"
            );
        }
        Ok(())
    }
}
pub(crate) fn take_execution_evidence(conn: &mut Conn) -> ExecutionEvidence {
    ExecutionEvidence(match conn {
        Conn::Sqlite(c) => Some(std::mem::take(&mut c.last)),
        _ => None,
    })
}
pub(crate) fn requires_native_originals(conn: &Conn) -> bool {
    matches!(conn, Conn::Sqlite(_))
}
pub(crate) async fn apply_native(
    conn: &mut Conn,
    table: &TableRef,
    changes: &[RowChange],
    capture: Option<(&Target, &QueryResult, &ExecutionEvidence)>,
    cancel: super::EditCancellation<'_>,
) -> Result<u64> {
    let Conn::Sqlite(c) = conn else {
        bail!("Native original-value editing is unavailable")
    };
    let proof = if let Some((target, result, evidence)) = capture {
        Some(super::sqlite::BrowseProof {
            table: table.clone(),
            identity: target.identity.clone(),
            columns: result.columns.clone(),
            rows: result.rows.clone(),
            native: evidence
                .0
                .clone()
                .context("SQLite execution storage evidence unavailable")?,
        })
    } else {
        c.browse_proof.clone()
    };
    c.apply(table, changes, proof.as_ref(), cancel).await
}
#[derive(Debug, Clone)]
pub(crate) struct BaseColumn {
    pub column: Column,
    pub editable: bool,
    pub star_visible: bool,
}
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub table: TableRef,
    pub identity: Vec<Cell>,
    pub signature: Vec<Vec<Cell>>,
    pub columns: Vec<BaseColumn>,
}

pub(crate) async fn context(conn: &mut Conn) -> Result<(LexMode, Option<String>)> {
    match conn {
        Conn::Sqlite(_) => Ok((LexMode::default(), None)),
        Conn::Pg(c) => {
            let row = c.fetch_one("SHOW standard_conforming_strings").await?;
            ensure!(
                row.try_get::<String, _>(0)? == "on",
                "Query editing requires standard_conforming_strings=on"
            );
            Ok((LexMode::default(), None))
        }
        Conn::My(c) => {
            let row = c
                .fetch_one("SELECT @@session.autocommit, @@session.sql_mode, DATABASE()")
                .await?;
            ensure!(
                row.try_get::<i64, _>(0)? == 1,
                "Query-result editing requires MySQL autocommit=1"
            );
            let mode: String = row.try_get(1)?;
            Ok((
                LexMode {
                    mysql_ansi_quotes: mode.split(',').any(|m| m == "ANSI_QUOTES"),
                    mysql_no_backslash_escapes: mode
                        .split(',')
                        .any(|m| m == "NO_BACKSLASH_ESCAPES"),
                },
                row.try_get::<Option<String>, _>(2)?,
            ))
        }
        Conn::Closed => bail!("Connection closed; reconnect required"),
    }
}
pub(crate) async fn resolve(
    conn: &mut Conn,
    engine: Engine,
    requested: &TableRef,
) -> Result<TableRef> {
    match engine {
        Engine::Postgres => Ok(resolve_pg(conn, requested).await?.0),
        Engine::MySql => resolve_mysql(conn, requested).await,
        Engine::Sqlite => {
            let Conn::Sqlite(c) = conn else {
                bail!("Connection closed")
            };
            Ok(c.resolve(requested).await?.0)
        }
    }
}
pub(crate) async fn lock(
    conn: &mut Conn,
    engine: Engine,
    table: &TableRef,
    write: bool,
) -> Result<()> {
    let qualified = qualified_table(engine, table);
    match engine {
        Engine::Postgres => {
            execute(
                conn,
                &format!(
                    "LOCK TABLE {qualified} IN {} MODE",
                    if write {
                        "ROW EXCLUSIVE"
                    } else {
                        "ACCESS SHARE"
                    }
                ),
            )
            .await
        }
        Engine::MySql => execute(conn, &format!("SELECT 1 FROM {qualified} LIMIT 0")).await,
        Engine::Sqlite => Ok(()),
    }
}
pub(crate) async fn inspect_target(
    conn: &mut Conn,
    engine: Engine,
    requested: &TableRef,
) -> Result<Target> {
    match engine {
        Engine::Postgres => inspect_pg(conn, requested).await,
        Engine::MySql => inspect_mysql(conn, requested).await,
        Engine::Sqlite => {
            let Conn::Sqlite(c) = conn else {
                bail!("Connection closed")
            };
            let target = c.inspect(requested).await?;
            Ok(Target {
                table: target.table,
                identity: target.identity,
                signature: target.signature,
                columns: target
                    .columns
                    .into_iter()
                    .zip(target.editable)
                    .map(|(column, editable)| BaseColumn {
                        column,
                        editable,
                        star_visible: true,
                    })
                    .collect(),
            })
        }
    }
}
async fn resolve_pg(conn: &mut Conn, requested: &TableRef) -> Result<(TableRef, Vec<Cell>)> {
    let reference = qualified_table(Engine::Postgres, requested);
    let meta=bound_results(conn,"SELECT current_database(), (SELECT oid::text FROM pg_catalog.pg_database WHERE datname=current_database()), n.nspname, c.relname, c.oid::text, c.relfilenode::text, c.relkind::text, c.relhassubclass::text, c.relispartition::text, c.relpersistence::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE c.oid=pg_catalog.to_regclass($1::text)",&[Cell::Text(reference)],1).await?;
    let row = meta
        .rows
        .first()
        .context("Source relation could not be resolved at execution time")?;
    ensure!(
        text(&row[6]) == "r" && !truth(&row[7]) && !truth(&row[8]) && text(&row[9]) != "t",
        "Only permanent ordinary PostgreSQL base tables without inheritance are editable; views, partitions and temporary tables are read-only"
    );
    Ok((
        TableRef {
            schema: text(&row[2]),
            name: text(&row[3]),
        },
        row.clone(),
    ))
}
async fn inspect_pg(conn: &mut Conn, requested: &TableRef) -> Result<Target> {
    let (table, row) = resolve_pg(conn, requested).await?;
    let rows=bound_results(conn,"SELECT a.attname, pg_catalog.format_type(a.atttypid,a.atttypmod), a.attnotnull::text, EXISTS(SELECT 1 FROM pg_catalog.pg_index i WHERE i.indrelid=a.attrelid AND i.indisprimary AND a.attnum=ANY(i.indkey))::text, pg_catalog.pg_get_expr(d.adbin,d.adrelid), a.attgenerated::text, a.attidentity::text, a.attnum::text, a.atttypid::text, a.atttypmod::text, a.attcollation::text FROM pg_catalog.pg_attribute a LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid=$1::oid AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum LIMIT 1025",&[row[4].clone()],1025).await?;
    ensure!(
        !rows.rows.is_empty() && rows.rows.len() <= 1024 && !rows.truncated,
        "Source column metadata unavailable or exceeds 1024 columns"
    );
    let columns = rows
        .rows
        .iter()
        .map(|r| BaseColumn {
            column: Column {
                name: text(&r[0]),
                data_type: text(&r[1]),
                nullable: !truth(&r[2]),
                primary_key: truth(&r[3]),
                default_value: optional_text(&r[4]),
            },
            editable: text(&r[5]).is_empty() && text(&r[6]) != "a",
            star_visible: true,
        })
        .collect();
    Ok(Target {
        table,
        identity: row,
        signature: rows.rows,
        columns,
    })
}
async fn resolve_mysql(conn: &mut Conn, requested: &TableRef) -> Result<TableRef> {
    ensure!(!requested.schema.is_empty(), "No current MySQL database");
    let params = [
        Cell::Text(requested.schema.clone()),
        Cell::Text(requested.name.clone()),
    ];
    let meta=bound_results(conn,"SELECT TABLE_SCHEMA,TABLE_NAME,TABLE_TYPE,ENGINE FROM information_schema.TABLES WHERE TABLE_SCHEMA=? AND TABLE_NAME=?",&params,1).await?;
    let row = meta
        .rows
        .first()
        .context("Source table metadata unavailable")?;
    ensure!(
        text(&row[2]) == "BASE TABLE" && text(&row[3]).eq_ignore_ascii_case("InnoDB"),
        "Only permanent MySQL InnoDB base tables are editable; views and other engines are read-only"
    );
    Ok(TableRef {
        schema: text(&row[0]),
        name: text(&row[1]),
    })
}
async fn inspect_mysql(conn: &mut Conn, requested: &TableRef) -> Result<Target> {
    ensure!(
        !requested.schema.is_empty(),
        "MySQL source inspection requires a resolved database"
    );
    let ddl = raw_results(
        conn,
        &format!(
            "SHOW CREATE TABLE {}",
            qualified_table(Engine::MySql, requested)
        ),
        1,
    )
    .await?;
    let definition = ddl
        .first()
        .and_then(|r| r.rows.first())
        .and_then(|r| r.get(1))
        .map(text)
        .context("Effective MySQL source definition unavailable")?;
    ensure!(
        definition.starts_with("CREATE TABLE "),
        "Temporary or shadowed MySQL tables are read-only"
    );
    // Cache qualified catalog SQL, never metadata; preserves Pass11 RTT budget.
    let rows=bound_results(conn,"SELECT t.TABLE_SCHEMA,t.TABLE_NAME,t.TABLE_TYPE,t.ENGINE,CAST(i.TABLE_ID AS CHAR),c.COLUMN_NAME,c.COLUMN_TYPE,c.IS_NULLABLE,c.COLUMN_KEY,c.COLUMN_DEFAULT,c.EXTRA,c.GENERATION_EXPRESSION,c.ORDINAL_POSITION,c.COLLATION_NAME,c.CHARACTER_SET_NAME FROM information_schema.TABLES t JOIN information_schema.COLUMNS c ON c.TABLE_SCHEMA=t.TABLE_SCHEMA AND c.TABLE_NAME=t.TABLE_NAME JOIN information_schema.INNODB_TABLES i ON i.NAME=? WHERE t.TABLE_SCHEMA=? AND t.TABLE_NAME=? AND c.TABLE_SCHEMA=? AND c.TABLE_NAME=? ORDER BY c.ORDINAL_POSITION LIMIT 1025",&[Cell::Text(format!("{}/{}",requested.schema,requested.name)),Cell::Text(requested.schema.clone()),Cell::Text(requested.name.clone()),Cell::Text(requested.schema.clone()),Cell::Text(requested.name.clone())],1025).await?;
    ensure!(
        !rows.rows.is_empty() && rows.rows.len() <= 1024 && !rows.truncated,
        "Stable MySQL source metadata unavailable, ambiguous, or exceeds 1024 columns (encoded names/partitions unsupported)"
    );
    let identity = rows.rows[0][..5].to_vec();
    ensure!(
        text(&identity[2]) == "BASE TABLE" && text(&identity[3]).eq_ignore_ascii_case("InnoDB"),
        "Only permanent MySQL InnoDB base tables are editable"
    );
    let mut names = HashSet::new();
    ensure!(
        rows.rows
            .iter()
            .all(|r| r[..5] == identity && names.insert(text(&r[5]))),
        "Stable MySQL source identity or columns are ambiguous"
    );
    let table = TableRef {
        schema: text(&identity[0]),
        name: text(&identity[1]),
    };
    let signature: Vec<_> = rows.rows.into_iter().map(|r| r[5..].to_vec()).collect();
    let columns = signature
        .iter()
        .map(|r| BaseColumn {
            column: Column {
                name: text(&r[0]),
                data_type: text(&r[1]),
                nullable: text(&r[2]) == "YES",
                primary_key: text(&r[3]) == "PRI",
                default_value: optional_text(&r[4]),
            },
            editable: text(&r[6]).is_empty() && !text(&r[5]).contains("GENERATED"),
            star_visible: !text(&r[5]).contains("INVISIBLE"),
        })
        .collect();
    Ok(Target {
        table,
        identity,
        signature,
        columns,
    })
}
