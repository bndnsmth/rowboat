//! Actual session-path catalog for the inline provider. No buffer SQL is executed.
use rowboat_core::{Database, TableInfo};
use rowboat_services::CatalogTable;

pub(crate) async fn catalog_relations(
    db: &Database,
    extra: Vec<TableInfo>,
    alive: impl Fn() -> bool,
) -> anyhow::Result<(Vec<String>, Vec<CatalogTable>)> {
    anyhow::ensure!(alive(), "Obsolete completion request");
    let namespaces = extra
        .into_iter()
        .map(|t| t.table.schema)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let catalog = db.completion_catalog(&namespaces).await?;
    anyhow::ensure!(alive(), "Obsolete completion request");
    Ok(service_relations(catalog))
}

pub(crate) fn service_relations(
    catalog: rowboat_core::CompletionCatalog,
) -> (Vec<String>, Vec<CatalogTable>) {
    (
        catalog.search_path,
        catalog
            .tables
            .into_iter()
            .map(|t| CatalogTable {
                table: t.table,
                columns: Vec::new(),
            })
            .collect(),
    )
}
/// Completion consumes names only, not indexes, relationships or CREATE text.
pub(crate) async fn column_names(
    db: &Database,
    table: &rowboat_core::TableRef,
) -> anyhow::Result<Vec<String>> {
    db.completion_column_names(table).await
}
#[cfg(test)]
async fn catalog(
    db: &Database,
    tables: Vec<TableInfo>,
    sql: &str,
) -> anyhow::Result<rowboat_services::CompletionCatalog> {
    let (names, mut tables) = catalog_relations(db, tables, || true).await?;
    for table in tables
        .iter_mut()
        .filter(|t| sql.to_lowercase().contains(&t.table.name.to_lowercase()))
        .take(32)
    {
        if let Ok(columns) = column_names(db, &table.table).await {
            table.columns = columns;
        }
    }
    let mut catalog = rowboat_services::CompletionCatalog::new(db.engine(), tables)?;
    catalog.set_search_path(Some(names))?;
    Ok(catalog)
}
#[cfg(test)]
mod tests {
    #[tokio::test]
    #[ignore = "Requires disposable PostgreSQL on 15435"]
    async fn pass6_completion_full_path_shadow_and_fresh_set() -> anyhow::Result<()> {
        use rowboat_core::{Engine, TableInfo, TableRef, sql::quote_ident};
        let (db, _, schema) = crate::pass5_tests::fixture(Engine::Postgres).await?;
        let result=async{
            let q=quote_ident(Engine::Postgres,&schema);
            db.query(&format!("CREATE TABLE {q}.shadow(later_column INT); CREATE TEMP TABLE shadow(first_column INT); SET search_path TO {q}"),0).await?;
            let tables=vec![TableInfo{table:TableRef{schema:schema.clone(),name:"shadow".into()},kind:"BASE TABLE".into()}];
            let sql="SELECT s. FROM shadow s";
            let catalog=super::catalog(&db,tables.clone(),sql).await?;
            let items=rowboat_services::complete_sql(sql,9,&catalog,100)?.items;
            anyhow::ensure!(items.iter().any(|i|i.label.contains("first_column")));
            anyhow::ensure!(!items.iter().any(|i|i.label.contains("later_column")));
            db.query(&format!("DROP TABLE pg_temp.shadow; SET search_path TO {q},pg_catalog"),0).await?;
            let catalog=super::catalog(&db,tables,sql).await?;
            anyhow::ensure!(rowboat_services::complete_sql(sql,9,&catalog,100)?.items.iter().any(|i|i.label.contains("later_column")));
            anyhow::Ok(())
        }.await;
        crate::pass5_tests::cleanup(&db, &schema).await?;
        result
    }
    #[tokio::test]
    #[ignore = "Requires disposable MySQL on 13309"]
    async fn pass6_completion_fresh_use_ignores_stale_explorer_scope() -> anyhow::Result<()> {
        use rowboat_core::{Engine, sql::quote_ident};
        let (db, _, schema) = crate::pass5_tests::fixture(Engine::MySql).await?;
        let result = async {
            let q = quote_ident(Engine::MySql, &schema);
            db.query(
                &format!("CREATE TABLE {q}.shadow(current_column INT); USE {q}"),
                0,
            )
            .await?;
            let sql = "SELECT s. FROM shadow s";
            let catalog = super::catalog(&db, vec![], sql).await?;
            anyhow::ensure!(
                rowboat_services::complete_sql(sql, 9, &catalog, 100)?
                    .items
                    .iter()
                    .any(|i| i.label.contains("current_column"))
            );
            db.query("USE rowboat", 0).await?;
            let catalog = super::catalog(&db, vec![], sql).await?;
            anyhow::ensure!(
                !rowboat_services::complete_sql(sql, 9, &catalog, 100)?
                    .items
                    .iter()
                    .any(|i| i.label.contains("current_column"))
            );
            anyhow::Ok(())
        }
        .await;
        crate::pass5_tests::cleanup(&db, &schema).await?;
        result
    }
}
