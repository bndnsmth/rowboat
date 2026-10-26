//! Uses only explicit local disposable Rowboat servers. Owns only uniquely
//! prefixed namespaces/users; never starts/stops containers or discovers profiles.
use anyhow::{Result, ensure};
use rowboat_core::{ConnectionProfile, Database, Engine, QueryEvent, sql::quote_ident};
use std::time::Duration;
use tokio::time::timeout;

fn profile(engine: Engine, read_only: bool) -> ConnectionProfile {
    ConnectionProfile {
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
        read_only,
        ..Default::default()
    }
}
async fn connect(engine: Engine, read_only: bool) -> Result<Database> {
    Database::connect(&profile(engine, read_only), "rowboat-test").await
}
async fn scalar(db: &Database, sql: &str) -> Result<String> {
    Ok(db.query(sql, 1).await?[0].rows[0][0].display())
}
fn prefix() -> String {
    format!(
        "pass12_core_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}
async fn create(db: &Database, names: &[String]) -> Result<()> {
    for name in names {
        let kind = if db.engine() == Engine::Postgres {
            "SCHEMA"
        } else {
            "DATABASE"
        };
        db.query(
            &format!("CREATE {kind} {}", quote_ident(db.engine(), name)),
            0,
        )
        .await?;
    }
    Ok(())
}
async fn cleanup(db: &Database, names: &[String]) -> Result<()> {
    let mut error = None;
    for name in names {
        let sql = match db.engine() {
            Engine::Sqlite => unreachable!("network fixture test"),
            Engine::Postgres => format!("DROP SCHEMA {} CASCADE", quote_ident(db.engine(), name)),
            Engine::MySql => format!("DROP DATABASE {}", quote_ident(db.engine(), name)),
        };
        if let Err(e) = db.query(&sql, 0).await {
            error = Some(e);
        }
    }
    error.map_or(Ok(()), Err)
}

#[tokio::test]
#[ignore = "explicit Podman PostgreSQL15435/MySQL13309 fixtures"]
async fn both_engines_scope_empty_quoted_and_independent_workspace_contexts() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let admin = connect(engine, false).await?;
        let p = prefix();
        let names = [format!("{p}_a`'\""), format!("{p}_b"), format!("{p}_empty")];
        create(&admin, &names).await?;
        let result: Result<()> = async {
            for (i, name) in names[..2].iter().enumerate() {
                let q = quote_ident(engine, name);
                admin
                    .query(
                        &format!("CREATE TABLE {q}.only_{i} (id INT PRIMARY KEY)"),
                        0,
                    )
                    .await?;
                admin
                    .query(
                        &format!("CREATE VIEW {q}.view_{i} AS SELECT id FROM {q}.only_{i}"),
                        0,
                    )
                    .await?;
            }
            let first = connect(engine, false).await?;
            let second = connect(engine, false).await?;
            if engine == Engine::MySql {
                first.select_database(&names[0]).await?;
                second.select_database(&names[1]).await?;
            } else {
                first
                    .query(
                        &format!("SET search_path TO {}", quote_ident(engine, &names[0])),
                        0,
                    )
                    .await?;
                second
                    .query(
                        &format!("SET search_path TO {}", quote_ident(engine, &names[1])),
                        0,
                    )
                    .await?;
                ensure!(first.select_database(&names[1]).await.is_err());
            }
            let before = first.namespace_catalog(None).await?;
            ensure!(
                before.database
                    == if engine == Engine::MySql {
                        names[0].as_str()
                    } else {
                        "rowboat"
                    }
            );
            ensure!(
                before.current_schema.as_deref()
                    == if engine == Engine::Postgres {
                        Some(names[0].as_str())
                    } else {
                        None
                    }
            );
            ensure!(names.iter().all(|n| before.namespaces.contains(n)));
            ensure!(
                before.tables.len() == 2
                    && before.tables.iter().all(|t| t.table.schema == names[0])
            );
            ensure!(before.tables.iter().any(|t| t.table.name == "only_0"));
            ensure!(before.tables.iter().any(|t| t.kind == "VIEW"));
            let scoped = first.namespace_catalog(Some(&names[1])).await?;
            ensure!(
                scoped.database == before.database
                    && scoped.current_schema == before.current_schema
            );
            ensure!(
                scoped.tables.len() == 2
                    && scoped.tables.iter().all(|t| t.table.schema == names[1])
            );
            ensure!(
                first
                    .namespace_catalog(Some(&names[2]))
                    .await?
                    .tables
                    .is_empty()
            );
            for missing in [
                format!("{p}_missing"),
                format!("{p}' OR 1=1 --"),
                String::new(),
            ] {
                ensure!(first.namespace_catalog(Some(&missing)).await.is_err());
            }
            ensure!(
                first
                    .namespace_catalog(None)
                    .await?
                    .tables
                    .iter()
                    .all(|t| t.table.schema == names[0])
            );
            ensure!(
                second
                    .namespace_catalog(None)
                    .await?
                    .tables
                    .iter()
                    .all(|t| t.table.schema == names[1])
            );
            let system = if engine == Engine::Postgres {
                "pg_catalog"
            } else {
                "information_schema"
            };
            let system_snapshot = first.namespace_catalog(Some(system)).await?;
            ensure!(system_snapshot.namespaces.iter().any(|n| n == system));
            ensure!(
                !system_snapshot.tables.is_empty()
                    && system_snapshot
                        .tables
                        .iter()
                        .all(|t| t.table.schema == system)
            );
            if engine == Engine::MySql {
                // Editor USE must be observed from the server, not profile/cache.
                first
                    .query(&format!("USE {}", quote_ident(engine, &names[1])), 0)
                    .await?;
                ensure!(first.namespace_catalog(None).await?.database == names[1]);
                first.select_database(system).await?;
                ensure!(first.namespace_catalog(None).await?.database == system);
            } else {
                first.query("SET search_path TO pg_catalog", 0).await?;
                let current_system = first.namespace_catalog(None).await?;
                ensure!(current_system.current_schema.as_deref() == Some(system));
                ensure!(current_system.namespaces.iter().any(|n| n == system));
                first.query("SET search_path TO ''", 0).await?;
                let no_schema = first.namespace_catalog(None).await?;
                ensure!(no_schema.current_schema.is_none() && no_schema.tables.is_empty());
            }
            Ok(())
        }
        .await;
        let cleaned = cleanup(&admin, &names).await;
        result?;
        cleaned?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "explicit Podman PostgreSQL15435/MySQL13309 fixtures"]
async fn both_engines_preserve_readonly_settings() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let admin = connect(engine, false).await?;
        let names = [prefix()];
        create(&admin, &names).await?;
        let result: Result<()> = async {
            let target = format!("{}.guarded", quote_ident(engine, &names[0]));
            admin.query(&format!("CREATE TABLE {target} (id INT PRIMARY KEY)"), 0).await?;
            let readonly = connect(engine, true).await?;
            let settings = match engine {
                Engine::Sqlite => unreachable!("network fixture test"),
                Engine::Postgres => "SELECT current_setting('default_transaction_read_only') || ':' || current_setting('search_path')",
                Engine::MySql => "SELECT CONCAT(@@session.transaction_read_only, ':', @@session.autocommit, ':', @@session.sql_mode)",
            };
            let before = scalar(&readonly, settings).await?;
            if engine == Engine::MySql {
                readonly.select_database(&names[0]).await?;
                ensure!(readonly.namespace_catalog(None).await?.database == names[0]);
            }
            ensure!(readonly.namespace_catalog(Some(&names[0])).await?.tables.len() == 1);
            ensure!(scalar(&readonly, settings).await? == before);
            ensure!(!readonly.in_transaction().await?);
            ensure!(readonly.query(&format!("INSERT INTO {target} VALUES (1)"), 0).await.is_err());
            ensure!(scalar(&admin, &format!("SELECT count(*) FROM {target}")).await? == "0");
            Ok(())
        }.await;
        let cleaned = cleanup(&admin, &names).await;
        result?;
        cleaned?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "explicit Podman MySQL13309 fixture"]
async fn mysql_switch_rejects_busy_transactions_and_preserves_failed_context() -> Result<()> {
    let admin = connect(Engine::MySql, false).await?;
    let p = prefix();
    let names = [format!("{p}_a"), format!("{p}_b")];
    create(&admin, &names).await?;
    let result: Result<()> = async {
        let db = connect(Engine::MySql, false).await?;
        db.select_database(&names[0]).await?;
        db.query("CREATE TABLE pending (id INT PRIMARY KEY) ENGINE=InnoDB", 0)
            .await?;
        db.query("START TRANSACTION; INSERT INTO pending VALUES (1)", 0)
            .await?;
        let error = db.select_database(&names[1]).await.unwrap_err();
        ensure!(error.to_string().contains("transaction"), "{error:#}");
        ensure!(db.in_transaction().await?);
        ensure!(scalar(&db, "SELECT count(*) FROM pending").await? == "1");
        ensure!(
            scalar(
                &admin,
                &format!(
                    "SELECT count(*) FROM {}.pending",
                    quote_ident(Engine::MySql, &names[0])
                )
            )
            .await?
                == "0"
        );
        db.rollback().await?;
        ensure!(scalar(&db, "SELECT count(*) FROM pending").await? == "0");
        db.query("SET autocommit=0", 0).await?;
        ensure!(
            db.select_database(&names[1])
                .await
                .unwrap_err()
                .to_string()
                .contains("autocommit=1")
        );
        ensure!(scalar(&db, "SELECT @@session.autocommit").await? == "0");
        db.query("ROLLBACK; SET autocommit=1", 0).await?;
        for invalid in [
            String::new(),
            "bad\0name".into(),
            "bad/name".into(),
            "bad.name".into(),
            format!("{p}_missing"),
            format!("{p}`; COMMIT; USE `{p}_missing"),
        ] {
            ensure!(db.select_database(&invalid).await.is_err());
            ensure!(scalar(&db, "SELECT DATABASE()").await? == names[0]);
            ensure!(db.namespace_catalog(None).await?.database == names[0]);
        }
        let operation = db.administrative_operation(None).await?;
        ensure!(
            timeout(Duration::from_secs(1), db.select_database(&names[1]))
                .await?
                .unwrap_err()
                .to_string()
                .contains("busy")
        );
        operation.finish().await?;
        let mut stream = db
            .query_stream("SELECT 1; SELECT SLEEP(0.4)", Default::default())
            .await?;
        ensure!(matches!(
            stream.recv().await,
            Some(QueryEvent::Schema { .. })
        ));
        ensure!(
            db.select_database(&names[1])
                .await
                .unwrap_err()
                .to_string()
                .contains("busy")
        );
        while let Some(event) = stream.recv().await {
            if let QueryEvent::Error { message, .. } = event {
                anyhow::bail!(message);
            }
        }
        db.select_database(&names[1]).await?;
        ensure!(db.namespace_catalog(None).await?.database == names[1]);
        Ok(())
    }
    .await;
    let cleaned = cleanup(&admin, &names).await;
    result?;
    cleaned
}

#[tokio::test]
#[ignore = "explicit Podman MySQL13309 fixture"]
async fn mysql_permissions_and_unknown_transaction_state_fail_closed() -> Result<()> {
    let admin = connect(Engine::MySql, false).await?;
    let p = prefix();
    let names = [format!("{p}_allowed"), format!("{p}_denied")];
    create(&admin, &names).await?;
    admin
        .query(
            &format!("CREATE USER '{p}'@'%' IDENTIFIED BY 'rowboat-test'"),
            0,
        )
        .await?;
    let result: Result<()> = async {
        admin
            .query(
                &format!(
                    "GRANT SELECT ON {}.* TO '{p}'@'%'",
                    quote_ident(Engine::MySql, &names[0])
                ),
                0,
            )
            .await?;
        let mut restricted = profile(Engine::MySql, false);
        restricted.username = p.clone();
        restricted.database = names[0].clone();
        let db = Database::connect(&restricted, "rowboat-test").await?;
        let snapshot = db.namespace_catalog(None).await?;
        ensure!(
            snapshot.namespaces.contains(&names[0]) && !snapshot.namespaces.contains(&names[1])
        );
        ensure!(db.select_database(&names[1]).await.is_err());
        ensure!(db.namespace_catalog(None).await?.database == names[0]);
        ensure!(db.namespace_catalog(Some(&names[1])).await.is_err());
        let id = scalar(&db, "SELECT CONNECTION_ID()").await?;
        ensure!(id.chars().all(|c| c.is_ascii_digit()));
        admin.query(&format!("KILL CONNECTION {id}"), 0).await?;
        ensure!(db.select_database(&names[0]).await.is_err());
        let retired = db.namespace_catalog(None).await.unwrap_err();
        ensure!(
            format!("{retired:#}").contains("Connection closed"),
            "{retired:#}"
        );
        Ok(())
    }
    .await;
    let dropped_user = admin.query(&format!("DROP USER '{p}'@'%'"), 0).await;
    let cleaned = cleanup(&admin, &names).await;
    result?;
    dropped_user?;
    cleaned
}
