//! Independent live sessions are the foundation of GUI connection workspaces.
use anyhow::{Result, ensure};
use rowboat_core::{Cell, ConnectionProfile, Database, Engine};
use std::time::Duration;

fn profile(engine: Engine) -> ConnectionProfile {
    ConnectionProfile {
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
            "rowboat"
        }
        .into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    }
}

async fn scalar(db: &Database, sql: &str) -> Result<String> {
    let results = db.query(sql, 1).await?;
    match &results[0].rows[0][0] {
        Cell::Text(text) => Ok(text.clone()),
        other => anyhow::bail!("Expected text scalar, got {other:?}"),
    }
}

async fn independent(engine: Engine) -> Result<()> {
    let p = profile(engine);
    let (a, b) = tokio::try_join!(
        Database::connect(&p, "rowboat-test"),
        Database::connect(&p, "rowboat-test")
    )?;
    let id_sql = if engine == Engine::Postgres {
        "SELECT pg_backend_pid()"
    } else {
        "SELECT CONNECTION_ID()"
    };
    let id_a: u64 = scalar(&a, id_sql).await?.parse()?;
    let id_b: u64 = scalar(&b, id_sql).await?.parse()?;
    ensure!(
        id_a != id_b,
        "Workspaces must have distinct server sessions"
    );
    // Identically named temporary tables must resolve against their own session.
    for (db, value) in [(&a, 11), (&b, 22)] {
        db.query(
            "CREATE TEMPORARY TABLE rowboat_workspace_session (id INTEGER PRIMARY KEY, value INTEGER)",
            0,
        )
        .await?;
        db.query(
            &format!("INSERT INTO rowboat_workspace_session VALUES (1,{value})"),
            0,
        )
        .await?;
    }
    a.begin().await?;
    b.begin().await?;
    a.query(
        "UPDATE rowboat_workspace_session SET value=111 WHERE id=1",
        0,
    )
    .await?;
    b.query(
        "UPDATE rowboat_workspace_session SET value=222 WHERE id=1",
        0,
    )
    .await?;
    ensure!(a.in_transaction().await? && b.in_transaction().await?);
    a.rollback().await?;
    ensure!(
        !a.in_transaction().await? && b.in_transaction().await?,
        "Rollback crossed session ownership"
    );
    ensure!(scalar(&a, "SELECT value FROM rowboat_workspace_session").await? == "11");
    ensure!(scalar(&b, "SELECT value FROM rowboat_workspace_session").await? == "222");

    let sleeper = a.clone();
    let sql = if engine == Engine::Postgres {
        "SELECT pg_sleep(30) /* rowboat_workspace_busy */"
    } else {
        "SELECT SLEEP(30) /* rowboat_workspace_busy */"
    };
    let running = tokio::spawn(async move { sleeper.query(sql, 1).await });
    // Observe actual server execution rather than assuming a scheduler delay.
    let active = if engine == Engine::Postgres {
        format!(
            "SELECT count(*) FROM pg_stat_activity WHERE pid={id_a} AND state='active' AND query LIKE '%rowboat_workspace_busy%'"
        )
    } else {
        format!(
            "SELECT count(*) FROM information_schema.processlist WHERE id={id_a} AND info LIKE '%rowboat_workspace_busy%'"
        )
    };
    let ready = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            // PostgreSQL statistics snapshots persist within a transaction.
            if engine == Engine::Postgres {
                b.query("SELECT pg_stat_clear_snapshot()", 0).await?;
            }
            if scalar(&b, &active).await? == "1" {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    if !matches!(&ready, Ok(Ok(()))) {
        a.cancel().await?;
        let _ = running.await;
        anyhow::bail!("Could not observe the first workspace's running query: {ready:?}");
    }
    ensure!(
        scalar(&b, "SELECT value FROM rowboat_workspace_session").await? == "222",
        "A busy workspace blocked or retargeted another"
    );
    a.cancel().await?;
    let cancelled = tokio::time::timeout(Duration::from_secs(5), running).await??;
    ensure!(cancelled.is_err(), "Expected query cancellation");
    ensure!(
        b.in_transaction().await?,
        "Cancelling workspace A affected B's transaction"
    );
    b.commit().await?;
    ensure!(scalar(&b, "SELECT value FROM rowboat_workspace_session").await? == "222");
    ensure!(scalar(&a, "SELECT value FROM rowboat_workspace_session").await? == "11");
    // Connections own the temporary tables; dropping them requires no external cleanup.
    Ok(())
}

#[tokio::test]
#[ignore = "Requires task-owned Podman PostgreSQL on 15435"]
async fn postgres_workspace_sessions_are_independent() -> Result<()> {
    independent(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "Requires task-owned Podman MySQL on 13309"]
async fn mysql_workspace_sessions_are_independent() -> Result<()> {
    independent(Engine::MySql).await
}
