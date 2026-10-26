//! Engine-specific value binding and canonical wire projections. Shared change
//! validation and SQL statement assembly remain outside the driver boundary.
use crate::{Cell, Column, Engine, sql::quote_ident};
pub(crate) fn parameter(
    engine: Engine,
    col: &Column,
    value: &Cell,
    params: &mut Vec<Cell>,
) -> String {
    params.push(value.clone());
    match engine {
        Engine::MySql | Engine::Sqlite => "?".into(),
        Engine::Postgres => match value {
            Cell::Binary(_) => format!("${}::bytea", params.len()),
            _ => format!("CAST(${}::text AS {})", params.len(), col.data_type),
        },
    }
}
pub(crate) fn browse_projection(engine: Engine, column: &Column) -> String {
    let name = quote_ident(engine, &column.name);
    match engine {
        Engine::Postgres if column.data_type != "bytea" => format!("{name}::text AS {name}"),
        Engine::MySql if !mysql_binary_type(&column.data_type) => {
            format!("CAST({name} AS CHAR) AS {name}")
        }
        _ => name,
    }
}
pub(crate) fn original_predicate(engine: Engine, name: &str, value: &Cell, index: usize) -> String {
    match engine {
        Engine::Postgres if matches!(value, Cell::Binary(_)) => {
            format!("{name} IS NOT DISTINCT FROM ${index}::bytea")
        }
        Engine::Postgres => format!(
            "{name}::text COLLATE pg_catalog.\"C\" IS NOT DISTINCT FROM ${index}::text COLLATE pg_catalog.\"C\""
        ),
        Engine::MySql => format!("CAST({name} AS BINARY) <=> CAST(? AS BINARY)"),
        Engine::Sqlite => format!("{name} IS ?"),
    }
}
pub(crate) fn default_insert(engine: Engine, table: &str) -> String {
    match engine {
        Engine::Postgres | Engine::Sqlite => format!("INSERT INTO {table} DEFAULT VALUES"),
        Engine::MySql => format!("INSERT INTO {table} () VALUES ()"),
    }
}
fn mysql_binary_type(ty: &str) -> bool {
    let ty = ty.to_ascii_lowercase();
    [
        "binary",
        "varbinary",
        "tinyblob",
        "blob",
        "mediumblob",
        "longblob",
        "bit",
        "geometry",
        "point",
        "linestring",
        "polygon",
        "multipoint",
        "multilinestring",
        "multipolygon",
        "geometrycollection",
    ]
    .iter()
    .any(|prefix| ty == *prefix || ty.starts_with(&format!("{prefix}(")))
}
