//! Explicit local disposable fixture access, used only by ignored GUI executor regressions.
use rowboat_core::{ConnectionProfile, Database, Engine, TableRef, sql::quote_ident};
pub async fn fixture(engine: Engine) -> anyhow::Result<(Database, ConnectionProfile, String)> {
    fixture_prefix(engine, "pass5_gui").await
}
pub async fn fixture_prefix(
    engine: Engine,
    prefix: &str,
) -> anyhow::Result<(Database, ConnectionProfile, String)> {
    let profile = ConnectionProfile {
        engine,
        host: "127.0.0.1".into(),
        port: if engine == Engine::Postgres {
            15435
        } else {
            13309
        },
        database: "rowboat".into(),
        username: if engine == Engine::Postgres {
            "postgres"
        } else {
            "root"
        }
        .into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    };
    let db = Database::connect(&profile, "rowboat-test").await?;
    let schema = format!(
        "{prefix}_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    db.query(
        &format!("CREATE SCHEMA {}", quote_ident(engine, &schema)),
        0,
    )
    .await?;
    Ok((db, profile, schema))
}
pub fn table(schema: &str, name: &str) -> TableRef {
    TableRef {
        schema: schema.into(),
        name: name.into(),
    }
}
pub async fn cleanup(db: &Database, schema: &str) -> anyhow::Result<()> {
    if db.in_transaction().await? {
        db.rollback().await?;
    }
    db.query(
        &format!(
            "DROP SCHEMA {}{}",
            quote_ident(db.engine(), schema),
            if db.engine() == Engine::Postgres {
                " CASCADE"
            } else {
                ""
            }
        ),
        0,
    )
    .await?;
    Ok(())
}
