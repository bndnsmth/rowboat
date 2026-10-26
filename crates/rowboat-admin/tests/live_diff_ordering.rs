use anyhow::{Context, Result, ensure};
use rowboat_admin::live_diff::*;
use rowboat_core::{ConnectionProfile, Database, Engine, sql::quote_ident};
use std::time::{SystemTime, UNIX_EPOCH};

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
            "root"
        }
        .into(),
        ssl_mode: "disable".into(),
        read_only: false,
        ..Default::default()
    }
}
async fn query(db: &Database, sql: &str) -> Result<()> {
    db.query(sql, 100).await?;
    Ok(())
}
async fn apply(db: &Database, comparison: &LiveComparison) -> Result<()> {
    ensure!(
        !comparison.is_blocked(),
        "Plan blocked: {:?}",
        comparison.blockers
    );
    for (i, statement) in comparison.plan.statements.iter().enumerate() {
        query(db, &statement.sql).await.with_context(|| {
            format!(
                "Reviewed ordering-regression statement {i}: {}",
                statement.reason
            )
        })?;
    }
    Ok(())
}
async fn exercise(engine: Engine, tag: &str) -> Result<()> {
    let db = Database::connect(&profile(engine), "rowboat-test").await?;
    let mapping = NamespaceMapping {
        source: format!("{tag}s"),
        destination: format!("{tag}d"),
    };
    let s = quote_ident(engine, &mapping.source);
    let d = quote_ident(engine, &mapping.destination);
    query(&db, &format!("CREATE SCHEMA {s}; CREATE SCHEMA {d}")).await?;
    let options = CompareOptions {
        drop_destination_only: true,
    };
    if engine == Engine::Postgres {
        for ns in [&s, &d] {
            query(&db,&format!("CREATE TABLE {ns}.a(id INTEGER NOT NULL,value INTEGER); CREATE TABLE {ns}.z(id INTEGER NOT NULL,value INTEGER)")).await?;
        }
        query(&db,&format!("CREATE INDEX moved ON {s}.a(id); ALTER TABLE {s}.a ADD CONSTRAINT unique_moved UNIQUE(value); ALTER TABLE {s}.a ADD CONSTRAINT primary_moved PRIMARY KEY(id)")).await?;
        query(&db,&format!("CREATE INDEX moved ON {d}.z(id); ALTER TABLE {d}.z ADD CONSTRAINT unique_moved UNIQUE(value); ALTER TABLE {d}.z ADD CONSTRAINT primary_moved PRIMARY KEY(id)")).await?;
        let additive = compare_live(&db, &db, &mapping, CompareOptions::default()).await?;
        ensure!(
            additive.is_blocked() && additive.plan.is_empty(),
            "Preserving destination-only conflicting index names must block"
        );
        let comparison = compare_live(&db, &db, &mapping, options).await?;
        let first_create = comparison
            .plan
            .statements
            .iter()
            .position(|s| s.sql.contains("CREATE INDEX") || s.sql.contains(" ADD CONSTRAINT"))
            .context("Missing key/index creates")?;
        let last_drop = comparison
            .plan
            .statements
            .iter()
            .rposition(|s| s.sql.contains("DROP INDEX") || s.sql.contains(" DROP CONSTRAINT"))
            .context("Missing key/index drops")?;
        ensure!(
            last_drop < first_create,
            "Schema-wide obsolete keys were not all dropped before creation"
        );
        apply(&db, &comparison).await?;
        let equal = compare_live(&db, &db, &mapping, options).await?;
        ensure!(
            !equal.is_blocked() && equal.plan.is_empty(),
            "Index/constraint name transfer did not converge"
        );
        query(&db,&format!("CREATE TABLE {s}.subset_parent(id INTEGER,tenant INTEGER,PRIMARY KEY(id,tenant)); CREATE TABLE {s}.subset_child(parent_id INTEGER,tenant INTEGER,CONSTRAINT subset_fk FOREIGN KEY(parent_id,tenant) REFERENCES {s}.subset_parent(id,tenant) ON DELETE SET NULL (parent_id))")).await?;
        let blocked = compare_live(&db, &db, &mapping, options).await?;
        ensure!(
            blocked.is_blocked()
                && blocked.plan.is_empty()
                && blocked
                    .blockers
                    .iter()
                    .any(|b| b.contains("delete-column subset")),
            "SET NULL column subset was silently widened"
        );
    } else {
        query(&db,&format!("CREATE TABLE {s}.unique_auto(id INTEGER NOT NULL AUTO_INCREMENT,amount INTEGER,CONSTRAINT id_unique UNIQUE(id)); CREATE TABLE {s}.index_auto(id INTEGER NOT NULL AUTO_INCREMENT,amount INTEGER,KEY id_support(id))")).await?;
        let comparison = compare_live(&db, &db, &mapping, options).await?;
        ensure!(
            comparison
                .plan
                .statements
                .iter()
                .filter(|s| s.sql.starts_with("CREATE TABLE"))
                .all(|s| s.sql.contains("UNIQUE") || s.sql.contains("KEY")),
            "AUTO_INCREMENT support key missing from CREATE TABLE"
        );
        apply(&db, &comparison).await?;
        query(
            &db,
            &format!(
                "INSERT INTO {d}.unique_auto () VALUES (); INSERT INTO {d}.index_auto () VALUES ()"
            ),
        )
        .await?;
        let equal = compare_live(&db, &db, &mapping, options).await?;
        ensure!(
            !equal.is_blocked() && equal.plan.is_empty(),
            "Secondary-key AUTO_INCREMENT tables did not converge"
        );
        query(&db,&format!("ALTER TABLE {s}.unique_auto ADD CONSTRAINT shared_check CHECK(amount > 0); ALTER TABLE {d}.index_auto ADD CONSTRAINT shared_check CHECK(amount > 0)")).await?;
        let preserved = compare_live(&db, &db, &mapping, CompareOptions::default()).await?;
        ensure!(
            preserved.is_blocked()
                && preserved.plan.is_empty()
                && preserved
                    .blockers
                    .iter()
                    .any(|b| b.contains("schema-wide MySQL check name shared_check collides")),
            "Preserved MySQL schema-wide check collision was missed"
        );
        apply(&db, &compare_live(&db, &db, &mapping, options).await?).await?;
        let equal = compare_live(&db, &db, &mapping, options).await?;
        ensure!(
            !equal.is_blocked() && equal.plan.is_empty(),
            "MySQL schema-wide check name transfer did not converge"
        );
        query(&db,&format!("ALTER TABLE {s}.unique_auto RENAME INDEX id_unique TO replacement_unique; ALTER TABLE {s}.index_auto RENAME INDEX id_support TO replacement_support")).await?;
        let blocked = compare_live(&db, &db, &mapping, options).await?;
        ensure!(
            blocked.is_blocked()
                && blocked.plan.is_empty()
                && blocked
                    .blockers
                    .iter()
                    .any(|b| b.contains("AUTO_INCREMENT")),
            "Unsafe staged support-key replacement must block"
        );
    }
    let suffix = if engine == Engine::Postgres {
        " CASCADE"
    } else {
        ""
    };
    query(
        &db,
        &format!("DROP SCHEMA {s}{suffix}; DROP SCHEMA {d}{suffix}"),
    )
    .await?;
    Ok(())
}
async fn run(engine: Engine) -> Result<()> {
    let tag = format!(
        "klo{:x}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    exercise(engine, &tag).await.with_context(|| {
        format!("Ordering fixture prefix {tag}; failures retain namespaces for inspection")
    })
}
#[tokio::test]
#[ignore = "Requires parent-owned PostgreSQL fixture on 127.0.0.1:15435"]
async fn postgres_schema_wide_key_transfer_and_subset_blocker() -> Result<()> {
    run(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Requires parent-owned MySQL fixture on 127.0.0.1:13309"]
async fn mysql_atomic_secondary_auto_increment_keys() -> Result<()> {
    run(Engine::MySql).await
}
