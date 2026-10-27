//! Only explicit local disposable Rowboat servers; no saved profiles or user databases.
use anyhow::{Result, ensure};
use rowboat_core::{ConnectionProfile, Database, DatabaseSelection, Engine, sql::quote_ident};

fn profile(engine: Engine) -> ConnectionProfile {
    ConnectionProfile {
        engine,
        host: "127.0.0.1".into(),
        database: "rowboat".into(),
        username: if engine == Engine::Postgres {
            "postgres"
        } else {
            "root"
        }
        .into(),
        port: if engine == Engine::Postgres {
            15435
        } else {
            13309
        },
        ssl_mode: "disable".into(),
        ..Default::default()
    }
}

#[test]
fn database_and_namespace_capabilities_are_independent() {
    assert_eq!(
        Engine::Postgres.descriptor().database_selection,
        DatabaseSelection::Reconnect
    );
    assert_eq!(
        Engine::Postgres.descriptor().namespaces,
        rowboat_core::NamespaceBehavior::Schemas
    );
    assert_eq!(
        Engine::MySql.descriptor().database_selection,
        DatabaseSelection::InSession
    );
    assert_eq!(
        Engine::Sqlite.descriptor().database_selection,
        DatabaseSelection::Unsupported
    );
}

#[tokio::test]
#[ignore = "Requires explicit disposable PostgreSQL on 15435"]
async fn postgres_database_catalog_filters_permissions_templates_and_disabled_databases()
-> Result<()> {
    let admin = Database::connect(&profile(Engine::Postgres), "rowboat-test").await?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let available = format!("rb_catalog_{}_quote'雪", &id[..12]);
    let denied = format!("rb_catalog_{}_denied", &id[..12]);
    let disabled = format!("rb_catalog_{}_disabled", &id[..12]);
    let template = format!("rb_catalog_{}_template", &id[..12]);
    let role = format!("rb_catalog_{}_reader", &id[..12]);
    let q = |name: &str| quote_ident(Engine::Postgres, name);
    let result = async {
        admin
            .query(
                &format!("CREATE ROLE {} LOGIN PASSWORD 'rowboat-test'", q(&role)),
                0,
            )
            .await?;
        for name in [&available, &denied, &disabled, &template] {
            admin
                .query(&format!("CREATE DATABASE {}", q(name)), 0)
                .await?;
        }
        admin
            .query(
                &format!("REVOKE CONNECT ON DATABASE {} FROM PUBLIC", q(&denied)),
                0,
            )
            .await?;
        admin
            .query(
                &format!("ALTER DATABASE {} ALLOW_CONNECTIONS false", q(&disabled)),
                0,
            )
            .await?;
        admin
            .query(
                &format!("ALTER DATABASE {} IS_TEMPLATE true", q(&template)),
                0,
            )
            .await?;
        let mut reader_profile = profile(Engine::Postgres);
        reader_profile.username = role.clone();
        reader_profile.read_only = true;
        let reader = Database::connect(&reader_profile, "rowboat-test").await?;
        reader.query("BEGIN", 0).await?;
        let catalog = reader.database_catalog().await?;
        ensure!(catalog.database == "rowboat");
        ensure!(catalog.databases.contains(&available));
        for name in [&denied, &disabled, &template] {
            ensure!(
                !catalog.databases.contains(name),
                "Inaccessible database offered"
            );
        }
        ensure!(
            reader.in_transaction().await?,
            "Discovery ended the caller transaction"
        );
        ensure!(reader.namespace_catalog(None).await?.database == "rowboat");
        reader.rollback().await?;
        ensure!(
            reader
                .query("SHOW default_transaction_read_only", 1)
                .await?[0]
                .rows[0][0]
                .display()
                == "on"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    for name in [&available, &denied, &disabled, &template] {
        let _ = admin
            .query(&format!("ALTER DATABASE {} IS_TEMPLATE false", q(name)), 0)
            .await;
        let _ = admin
            .query(
                &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", q(name)),
                0,
            )
            .await;
    }
    let _ = admin
        .query(&format!("DROP ROLE IF EXISTS {}", q(&role)), 0)
        .await;
    result
}

#[tokio::test]
#[ignore = "Requires explicit disposable MySQL on 13309"]
async fn mysql_database_discovery_does_not_switch_context() -> Result<()> {
    let db = Database::connect(&profile(Engine::MySql), "rowboat-test").await?;
    let name = format!(
        "rb_catalog_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    db.query(
        &format!("CREATE DATABASE {}", quote_ident(Engine::MySql, &name)),
        0,
    )
    .await?;
    let result = async {
        let catalog = db.database_catalog().await?;
        ensure!(catalog.database == "rowboat" && catalog.databases.contains(&name));
        ensure!(db.namespace_catalog(None).await?.database == "rowboat");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let _ = db
        .query(
            &format!("DROP DATABASE {}", quote_ident(Engine::MySql, &name)),
            0,
        )
        .await;
    result
}
