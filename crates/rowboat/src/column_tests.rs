#[tokio::test]
#[ignore = "Requires disposable MySQL on 13309"]
async fn gui_pass5_generated_column_full_snapshot_apply_and_stale_guard() -> anyhow::Result<()> {
    use crate::pass5_tests::{cleanup, fixture, table};
    use rowboat_admin::ddl;
    use rowboat_core::{Cell, Engine, sql::qualified_table};
    let (db, _, schema) = fixture(Engine::MySql).await?;
    let result=async {
        let table=table(&schema,"generated_rows");let sql=qualified_table(Engine::MySql,&table);
        db.query(&format!("CREATE TABLE {sql}(id INT PRIMARY KEY,g INT GENERATED ALWAYS AS (id+1) VIRTUAL,choice ENUM('a','b') NOT NULL DEFAULT 'a'); INSERT INTO {sql}(id) VALUES(5)"),0).await?;
        let snapshot=ddl::inspect_mysql_column(&db,&table,"g").await?;let mut desired=snapshot.definition().clone();desired.generation.as_mut().unwrap().expression="id + 2".into();
        let edit=ddl::plan_mysql_column_edit(&snapshot,desired)?;
        super::apply_column(&db,&edit).await?;
        let rows=db.query(&format!("SELECT g,choice FROM {sql}"),2).await?;
        anyhow::ensure!(rows[0].rows[0]==vec![Cell::Text("7".into()),Cell::Text("a".into())]);
        anyhow::ensure!(super::apply_column(&db,&edit).await.is_err(),"Stale full-table snapshot accepted");
        let snapshot=ddl::inspect_mysql_column(&db,&table,"choice").await?;let mut desired=snapshot.definition().clone();desired.specialized_type=Some(ddl::MySqlSpecialType::Enum(vec!["a".into(),"b".into(),"c".into()]));
        let edit=ddl::plan_mysql_column_edit(&snapshot,desired)?;
        db.begin().await?;anyhow::ensure!(super::apply_column(&db,&edit).await.is_err());db.rollback().await?;
        super::apply_column(&db,&edit).await?;anyhow::Ok(())
    }.await;
    cleanup(&db, &schema).await?;
    result
}
