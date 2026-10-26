//! Semantic facade conformance on explicit network fixtures; unique owned schemas.
use anyhow::{Result, ensure};
use rowboat_core::*;

#[tokio::test]
#[ignore = "explicit PostgreSQL15435/MySQL13309 fixtures"]
async fn network_catalog_browse_edits_and_leased_metadata() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let profile = ConnectionProfile {
            engine,
            host: "127.0.0.1".into(),
            port: if engine == Engine::Postgres {
                15435
            } else {
                13309
            },
            username: if engine == Engine::Postgres {
                "postgres"
            } else {
                "root"
            }
            .into(),
            database: "rowboat".into(),
            ssl_mode: "disable".into(),
            ..Default::default()
        };
        let db = Database::connect(&profile, "rowboat-test").await?;
        let name = format!("pass12_core_{}", uuid::Uuid::new_v4().simple());
        let quoted = sql::quote_ident(engine, &name);
        let kind = if engine == Engine::Postgres {
            "SCHEMA"
        } else {
            "DATABASE"
        };
        db.query(&format!("CREATE {kind} {quoted}"), 0).await?;
        let result:Result<()>=async {
            db.query(&format!("CREATE TABLE {quoted}.parent (id INTEGER PRIMARY KEY); CREATE TABLE {quoted}.child (id INTEGER PRIMARY KEY,parent INTEGER,note VARCHAR(40),FOREIGN KEY(parent) REFERENCES {quoted}.parent(id)); CREATE INDEX note_idx ON {quoted}.child(note); INSERT INTO {quoted}.parent VALUES(1); INSERT INTO {quoted}.child VALUES(1,1,'before'),(2,1,'other')"),0).await?;
            ensure!(!db.server_version().await?.is_empty());
            let table=TableRef {schema:name.clone(),name:"child".into()};let structure=db.structure(&table).await?;ensure!(structure.columns.len()==3&&structure.columns[0].primary_key&&!structure.indexes.is_empty()&&structure.definition.contains("CREATE TABLE"));
            let catalog=db.completion_catalog(std::slice::from_ref(&name)).await?;ensure!(catalog.tables.iter().any(|info|info.table==table));
            ensure!(db.completion_column_names(&table).await?==vec!["id","parent","note"]);
            ensure!(structure.foreign_keys.len()==1&&db.relationships(&table).await?.len()==1);
            let rows=db.browse(&table,&BrowseRequest {sort:Some(("id".into(),true)),limit:1,..Default::default()}).await?;
            let change=RowChange::Update {original:rows.columns.iter().zip(&rows.rows[0]).map(|(c,v)|(c.name.clone(),v.clone())).collect(),values:vec![("note".into(),Cell::Text("reviewed".into()))]};
            ensure!(db.apply_changes(&table,std::slice::from_ref(&change)).await?==1);ensure!(db.apply_changes(&table,&[change]).await.is_err());
            ensure!(!db.in_transaction().await?);
            let lease=db.administrative_operation(None).await?;
            let names=tokio::time::timeout(std::time::Duration::from_secs(5),QuerySession::completion_column_names(&lease,&table)).await??;ensure!(names==vec!["id","parent","note"]);
            ensure!(QuerySession::completion_catalog(&lease,std::slice::from_ref(&name)).await?.tables.iter().any(|info|info.table==table));
            ensure!(lease.finish().await?.reusable);Ok(())
        }.await;
        let admin = Database::connect(&profile, "rowboat-test").await?;
        admin
            .query(
                &format!(
                    "DROP {kind} {quoted}{}",
                    if engine == Engine::Postgres {
                        " CASCADE"
                    } else {
                        ""
                    }
                ),
                0,
            )
            .await?;
        result?;
    }
    Ok(())
}
