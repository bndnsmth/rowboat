//! Engine structure descriptions and definitions.
use super::super::*;
pub(crate) async fn structure(
    conn: &mut Conn,
    engine: Engine,
    table: &TableRef,
) -> Result<Structure> {
    if let Conn::Sqlite(c) = conn {
        return c.structure(table).await;
    }
    let params = [
        Cell::Text(table.schema.clone()),
        Cell::Text(table.name.clone()),
    ];
    let (column_sql, index_sql, fk_sql) = match engine {
        Engine::Postgres => (PG_COLUMNS, PG_INDEXES, PG_KEYS),
        Engine::MySql => (MY_COLUMNS, MY_INDEXES, MY_KEYS),
        Engine::Sqlite => bail!("Connection closed"),
    };
    let rows = bound_results(conn, column_sql, &params, MAX_ROWS)
        .await
        .context("Loading columns")?;
    ensure!(
        !rows.rows.is_empty(),
        "Table or view does not exist, is not visible, or has no columns"
    );
    let columns = rows
        .rows
        .iter()
        .map(|r| Column {
            name: text(&r[0]),
            data_type: text(&r[1]),
            nullable: truth(&r[2]),
            primary_key: truth(&r[3]),
            default_value: optional_text(&r[4]),
        })
        .collect();
    let rows = bound_results(conn, index_sql, &params, MAX_ROWS)
        .await
        .context("Loading indexes")?;
    let indexes = rows
        .rows
        .iter()
        .map(|r| IndexInfo {
            name: text(&r[0]),
            columns: text(&r[1]),
            unique: truth(&r[2]),
            definition: text(&r[3]),
        })
        .collect();
    let rows = bound_results(conn, fk_sql, &params, MAX_ROWS)
        .await
        .context("Loading foreign keys")?;
    let foreign_keys = rows
        .rows
        .iter()
        .map(|r| ForeignKeyInfo {
            name: text(&r[0]),
            columns: text(&r[1]),
            referenced_table: text(&r[2]),
            referenced_columns: text(&r[3]),
            on_update: text(&r[4]),
            on_delete: text(&r[5]),
        })
        .collect();
    let definition = match engine {
        Engine::Postgres => {
            let result = bound_results(conn, PG_DEFINITION, &params, 1)
                .await
                .context("Loading PostgreSQL definition")?;
            result.rows.first().map(|r| text(&r[0])).unwrap_or_default()
        }
        Engine::MySql => {
            let results = raw_results(
                conn,
                &format!("SHOW CREATE TABLE {}", qualified_table(engine, table)),
                1,
            )
            .await
            .context("Loading SHOW CREATE definition")?;
            results
                .first()
                .and_then(|r| r.rows.first())
                .and_then(|r| r.get(1))
                .map(text)
                .unwrap_or_default()
        }
        Engine::Sqlite => unreachable!(),
    };
    Ok(Structure {
        columns,
        indexes,
        foreign_keys,
        definition,
    })
}
const PG_COLUMNS: &str = r#"SELECT a.attname, pg_catalog.format_type(a.atttypid,a.atttypmod),
            NOT a.attnotnull, EXISTS(SELECT 1 FROM pg_index i WHERE i.indrelid=c.oid AND i.indisprimary AND a.attnum=ANY(i.indkey)),
            pg_get_expr(d.adbin,d.adrelid)
            FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
            JOIN pg_attribute a ON a.attrelid=c.oid
            LEFT JOIN pg_attrdef d ON d.adrelid=c.oid AND d.adnum=a.attnum
            WHERE n.nspname=COALESCE(NULLIF($1::text,''),current_schema()) AND c.relname=$2::text
            AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum"#;
const MY_COLUMNS: &str = "SELECT COLUMN_NAME,COLUMN_TYPE,IS_NULLABLE,IF(COLUMN_KEY='PRI','1','0'),COLUMN_DEFAULT FROM information_schema.COLUMNS WHERE TABLE_SCHEMA=COALESCE(NULLIF(?,''),DATABASE()) AND TABLE_NAME=? ORDER BY ORDINAL_POSITION";
const PG_INDEXES: &str = r#"SELECT ic.relname,
            array_to_string(ARRAY(SELECT pg_get_indexdef(i.indexrelid,k,true) FROM generate_series(1,i.indnkeyatts) k),', '),
            i.indisunique,pg_get_indexdef(i.indexrelid)
            FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_class ic ON ic.oid=i.indexrelid
            WHERE n.nspname=COALESCE(NULLIF($1::text,''),current_schema()) AND c.relname=$2::text ORDER BY ic.relname"#;
const MY_INDEXES: &str = "SELECT INDEX_NAME,GROUP_CONCAT(COALESCE(COLUMN_NAME,'<expression>') ORDER BY SEQ_IN_INDEX SEPARATOR ', '),CAST(1-MAX(NON_UNIQUE) AS CHAR),CONCAT(IF(MAX(NON_UNIQUE)=0,'UNIQUE ',' '),MAX(INDEX_TYPE),' INDEX ',INDEX_NAME,' (',GROUP_CONCAT(CONCAT('`',REPLACE(COALESCE(COLUMN_NAME,'<expression>'),'`','``'),'`',IF(SUB_PART IS NULL,'',CONCAT('(',SUB_PART,')'))) ORDER BY SEQ_IN_INDEX SEPARATOR ', '),')') FROM information_schema.STATISTICS WHERE TABLE_SCHEMA=COALESCE(NULLIF(?,''),DATABASE()) AND TABLE_NAME=? GROUP BY INDEX_NAME ORDER BY INDEX_NAME";
const PG_KEYS: &str = r#"SELECT con.conname,
            array_to_string(ARRAY(SELECT quote_ident(a.attname) FROM unnest(con.conkey) WITH ORDINALITY k(num,ord) JOIN pg_attribute a ON a.attrelid=con.conrelid AND a.attnum=k.num ORDER BY k.ord),', '),
            quote_ident(rn.nspname)||'.'||quote_ident(rc.relname),
            array_to_string(ARRAY(SELECT quote_ident(a.attname) FROM unnest(con.confkey) WITH ORDINALITY k(num,ord) JOIN pg_attribute a ON a.attrelid=con.confrelid AND a.attnum=k.num ORDER BY k.ord),', '),
            CASE con.confupdtype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' END,
            CASE con.confdeltype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' END
            FROM pg_constraint con JOIN pg_class c ON c.oid=con.conrelid JOIN pg_namespace n ON n.oid=c.relnamespace
            JOIN pg_class rc ON rc.oid=con.confrelid JOIN pg_namespace rn ON rn.oid=rc.relnamespace
            WHERE con.contype='f' AND n.nspname=COALESCE(NULLIF($1::text,''),current_schema()) AND c.relname=$2::text ORDER BY con.conname"#;
const MY_KEYS: &str = "SELECT k.CONSTRAINT_NAME,GROUP_CONCAT(k.COLUMN_NAME ORDER BY k.ORDINAL_POSITION SEPARATOR ', '),CONCAT(MAX(k.REFERENCED_TABLE_SCHEMA),'.',MAX(k.REFERENCED_TABLE_NAME)),GROUP_CONCAT(k.REFERENCED_COLUMN_NAME ORDER BY k.ORDINAL_POSITION SEPARATOR ', '),MAX(r.UPDATE_RULE),MAX(r.DELETE_RULE) FROM information_schema.KEY_COLUMN_USAGE k JOIN information_schema.REFERENTIAL_CONSTRAINTS r ON r.CONSTRAINT_SCHEMA=k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME=k.CONSTRAINT_NAME AND r.TABLE_NAME=k.TABLE_NAME WHERE k.TABLE_SCHEMA=COALESCE(NULLIF(?,''),DATABASE()) AND k.TABLE_NAME=? AND k.REFERENCED_TABLE_NAME IS NOT NULL GROUP BY k.CONSTRAINT_NAME ORDER BY k.CONSTRAINT_NAME";
const PG_DEFINITION: &str = r#"SELECT CASE WHEN c.relkind IN ('v','m') THEN
                'CREATE '||CASE WHEN c.relkind='m' THEN 'MATERIALIZED ' ELSE '' END||'VIEW '||quote_ident(n.nspname)||'.'||quote_ident(c.relname)||' AS '||pg_get_viewdef(c.oid,true)||';'
                ELSE 'CREATE TABLE '||quote_ident(n.nspname)||'.'||quote_ident(c.relname)||E' (\n' ||
                array_to_string(ARRAY(SELECT '  '||quote_ident(a.attname)||' '||format_type(a.atttypid,a.atttypmod)||
                    CASE WHEN a.attidentity='a' THEN ' GENERATED ALWAYS AS IDENTITY' WHEN a.attidentity='d' THEN ' GENERATED BY DEFAULT AS IDENTITY'
                    WHEN a.attgenerated='s' THEN ' GENERATED ALWAYS AS ('||pg_get_expr(d.adbin,d.adrelid)||') STORED'
                    WHEN d.adbin IS NOT NULL THEN ' DEFAULT '||pg_get_expr(d.adbin,d.adrelid) ELSE '' END||
                    CASE WHEN a.attnotnull THEN ' NOT NULL' ELSE '' END
                    FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum
                    WHERE a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum),E',\n')||
                COALESCE((SELECT E',\n'||string_agg('  CONSTRAINT '||quote_ident(con.conname)||' '||pg_get_constraintdef(con.oid,true),E',\n' ORDER BY con.conname) FROM pg_constraint con WHERE con.conrelid=c.oid AND con.contype IN ('p','u','f','c','x')),'')||E'\n);'||
                COALESCE((SELECT E'\n'||string_agg(pg_get_indexdef(i.indexrelid)||';',E'\n' ORDER BY i.indexrelid) FROM pg_index i WHERE i.indrelid=c.oid AND NOT EXISTS(SELECT 1 FROM pg_constraint con WHERE con.conindid=i.indexrelid)), '') END
                FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
                WHERE n.nspname=COALESCE(NULLIF($1::text,''),current_schema()) AND c.relname=$2::text"#;
