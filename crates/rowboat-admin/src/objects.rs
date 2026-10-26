//! Live database-object browsing and administrative inspection.
use crate::ddl::DdlPlan;
use anyhow::{Context, Result, bail, ensure};
use rowboat_core::{Cell, Engine, QueryResult, QuerySession};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectKind {
    Function,
    Procedure,
    Trigger,
    View,
    MaterializedView,
    Role,
    Sequence,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseObject {
    pub kind: ObjectKind,
    pub schema: Option<String>,
    pub name: String,
    pub definition: Option<String>,
    pub owner: Option<String>,
    pub details: Option<String>,
}

/// Columns are normalized to schema_name, object_name, definition, owner, details.
/// Permission failures are returned, never replaced with fake/empty catalogs.
pub fn object_sql(engine: Engine, kind: ObjectKind) -> Result<String> {
    crate::providers::object_sql(engine, kind)
}

pub(crate) fn cell_text(row: &[Cell], index: usize) -> Result<Option<String>> {
    match row.get(index) {
        Some(Cell::Text(s)) => Ok(Some(s.clone())),
        Some(Cell::Null) => Ok(None),
        // MySQL exposes some information_schema expressions (notably view
        // definitions) with binary metadata. These fixed catalog queries have
        // a textual contract; decode strictly, never lossy-convert user data.
        Some(Cell::Binary(bytes)) => Ok(Some(
            String::from_utf8(bytes.clone())
                .with_context(|| format!("Catalog column {index} is not valid UTF-8"))?,
        )),
        None => bail!("Incomplete catalog result"),
    }
}

/// For fixed textual administrative queries only, never arbitrary user data.
pub(crate) fn text_results(mut results: Vec<QueryResult>) -> Result<Vec<QueryResult>> {
    for result in &mut results {
        for row in &mut result.rows {
            for index in 0..row.len() {
                row[index] = cell_text(row, index)?.map(Cell::Text).unwrap_or(Cell::Null);
            }
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textual_catalog_binary_metadata_is_decoded_without_loss() {
        assert_eq!(
            cell_text(&[Cell::Binary("SELECT '日本語'".as_bytes().to_vec())], 0).unwrap(),
            Some("SELECT '日本語'".into())
        );
        assert!(cell_text(&[Cell::Binary(vec![0xff])], 0).is_err());
        assert_eq!(cell_text(&[Cell::Null], 0).unwrap(), None);
    }
}
pub async fn browse_objects(
    db: &impl QuerySession,
    kind: ObjectKind,
) -> Result<Vec<DatabaseObject>> {
    browse_objects_on(db, kind).await
}
pub(crate) async fn browse_objects_on(
    db: &impl rowboat_core::QuerySession,
    kind: ObjectKind,
) -> Result<Vec<DatabaseObject>> {
    let results = db.query(&object_sql(db.engine(), kind)?, 100_000).await?;
    let mut objects = Vec::new();
    for result in results {
        ensure!(
            !result.truncated,
            "Object catalog exceeded 100000 rows; narrow inspection with a catalog query"
        );
        for row in result.rows {
            objects.push(DatabaseObject {
                kind,
                schema: cell_text(&row, 0)?,
                name: cell_text(&row, 1)?
                    .ok_or_else(|| anyhow::anyhow!("Catalog object has no name"))?,
                definition: cell_text(&row, 2)?,
                owner: cell_text(&row, 3)?,
                details: cell_text(&row, 4)?,
            });
        }
    }
    Ok(objects)
}

pub fn connection_info_sql(engine: Engine) -> Result<&'static str> {
    crate::providers::connection_info_sql(engine)
}
pub async fn connection_info(db: &impl QuerySession) -> Result<Vec<QueryResult>> {
    text_results(db.query(connection_info_sql(db.engine())?, 10).await?)
}
pub fn server_activity_sql(engine: Engine) -> Result<&'static str> {
    crate::providers::server_activity_sql(engine)
}
pub async fn server_activity(db: &impl QuerySession) -> Result<Vec<QueryResult>> {
    text_results(db.query(server_activity_sql(db.engine())?, 10_000).await?)
}
pub fn namespaces_sql(engine: Engine) -> Result<&'static str> {
    crate::providers::namespaces_sql(engine)
}
pub async fn namespaces(db: &impl QuerySession) -> Result<Vec<QueryResult>> {
    text_results(db.query(namespaces_sql(db.engine())?, 10_000).await?)
}
pub async fn schemas(db: &impl QuerySession) -> Result<Vec<QueryResult>> {
    let sql = crate::providers::schemas_sql(db.engine())?;
    text_results(db.query(sql, 10_000).await?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NamespaceOperation {
    CreateDatabase { name: String },
    RenameDatabase { name: String, new_name: String },
    DropDatabase { name: String },
    CreateSchema { name: String },
    RenameSchema { name: String, new_name: String },
    DropSchema { name: String, cascade: bool },
}
pub fn namespace_sql(engine: Engine, op: &NamespaceOperation) -> Result<DdlPlan> {
    crate::providers::namespaces::compile(engine, op)
}
