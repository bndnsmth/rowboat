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
async fn execute(db: &Database, sql: &str) -> Result<()> {
    db.query(sql, 100).await?;
    Ok(())
}
async fn exercise(engine: Engine, tag: &str) -> Result<()> {
    let source = Database::connect(&profile(engine), "rowboat-test").await?;
    let destination = Database::connect(&profile(engine), "rowboat-test").await?;
    // Read-only preflight catches catalog version/syntax problems before this
    // run creates any objects. Existing fixture objects are never modified.
    snapshot_schema(
        &source,
        if engine == Engine::Postgres {
            "public"
        } else {
            "rowboat"
        },
    )
    .await
    .context("Catalog preflight")?;
    let mapping = NamespaceMapping {
        source: format!("{tag}s"),
        destination: format!("{tag}d"),
    };
    let q = |s: &str| quote_ident(engine, s);
    execute(&source, &format!("CREATE SCHEMA {}", q(&mapping.source))).await?;
    execute(
        &destination,
        &format!("CREATE SCHEMA {}", q(&mapping.destination)),
    )
    .await?;
    for (db, schema, is_source) in [
        (&source, &mapping.source, true),
        (&destination, &mapping.destination, false),
    ] {
        let ns = q(schema);
        let width = if is_source { 120 } else { 40 };
        let number = if is_source { "BIGINT" } else { "INTEGER" };
        let decimal = if is_source {
            "DECIMAL(12,2)"
        } else {
            "DECIMAL(8,2)"
        };
        let label_default = if is_source { "hello''s" } else { "old" };
        let suffix = if engine == Engine::MySql {
            " UNSIGNED"
        } else {
            ""
        };
        let auto = if engine == Engine::MySql {
            " AUTO_INCREMENT COMMENT 'owner''s key'"
        } else {
            ""
        };
        execute(db,&format!("CREATE TABLE {ns}.parent (id {number}{suffix} NOT NULL{auto}, code VARCHAR({width}) NOT NULL DEFAULT 'parent', CONSTRAINT parent_pk PRIMARY KEY(id), CONSTRAINT parent_unique UNIQUE(code))")).await?;
        let collation = if engine == Engine::MySql && is_source {
            " CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
        } else {
            ""
        };
        let extra = if is_source {
            ", note VARCHAR(80) DEFAULT 'memo'"
        } else {
            ", destination_only VARCHAR(80) DEFAULT 'retained'"
        };
        execute(db,&format!("CREATE TABLE {ns}.child (id INTEGER NOT NULL, parent_id {number}{suffix} NULL, label VARCHAR({width}){collation} NOT NULL DEFAULT '{label_default}', quantity {decimal} NOT NULL DEFAULT 0.00{extra}, CONSTRAINT child_pk PRIMARY KEY(id), CONSTRAINT child_quantity CHECK (quantity >= 0), CONSTRAINT child_fk FOREIGN KEY(parent_id) REFERENCES {ns}.parent(id) ON UPDATE CASCADE ON DELETE SET NULL)")).await?;
        execute(
            db,
            &format!("CREATE INDEX child_label ON {ns}.child(label)"),
        )
        .await?;
        if is_source {
            execute(db,&format!("CREATE TABLE {ns}.new_table (id INTEGER NOT NULL, stamp DATE DEFAULT (CURRENT_DATE), CONSTRAINT new_table_pk PRIMARY KEY(id))")).await?;
        } else {
            execute(
                db,
                &format!("CREATE TABLE {ns}.destination_only_table (id INTEGER)"),
            )
            .await?;
        }
    }
    let source_snapshot = snapshot_schema(&source, &mapping.source).await?;
    let destination_snapshot = snapshot_schema(&destination, &mapping.destination).await?;
    let additive = compare_snapshots(
        &source_snapshot,
        &destination_snapshot,
        &mapping,
        CompareOptions::default(),
    )?;
    ensure!(
        !additive.is_blocked(),
        "Additive comparison blocked: {:?}",
        additive.blockers
    );
    ensure!(
        additive
            .objects
            .iter()
            .any(|o| o.name == "destination_only" && o.change == ChangeKind::Preserve),
        "Destination-only column wasn't explicitly preserved"
    );
    ensure!(
        additive
            .objects
            .iter()
            .any(|o| o.name == "destination_only_table" && o.change == ChangeKind::Preserve),
        "Destination-only table wasn't explicitly preserved"
    );
    ensure!(
        !additive
            .plan
            .statements
            .iter()
            .any(|s| s.sql.contains("DROP TABLE") || s.sql.contains("DROP COLUMN")),
        "Default comparison dropped destination-only objects"
    );
    let options = CompareOptions {
        drop_destination_only: true,
    };
    let comparison = compare_live(&source, &destination, &mapping, options).await?;
    ensure!(
        !comparison.is_blocked(),
        "Comparison blocked: {:?}",
        comparison.blockers
    );
    ensure!(
        !comparison.plan.is_empty(),
        "Different schemas unexpectedly equal"
    );
    ensure!(
        destination_is_current(&destination, &comparison).await?,
        "Fresh snapshot is stale"
    );
    let original = serde_json::to_value(comparison.original_destination())?;
    execute(
        &destination,
        &format!(
            "ALTER TABLE {}.child ADD COLUMN stale_probe INTEGER",
            q(&mapping.destination)
        ),
    )
    .await?;
    ensure!(
        !destination_is_current(&destination, &comparison).await?,
        "Concurrent destination DDL was not detected"
    );
    ensure!(
        serde_json::to_value(comparison.original_destination())? == original,
        "Original destination snapshot mutated"
    );
    execute(
        &destination,
        &format!(
            "ALTER TABLE {}.child DROP COLUMN stale_probe",
            q(&mapping.destination)
        ),
    )
    .await?;
    let comparison = compare_live(&source, &destination, &mapping, options).await?;
    ensure!(
        !comparison.is_blocked(),
        "Fresh comparison blocked: {:?}",
        comparison.blockers
    );
    ensure!(
        destination_is_current(&destination, &comparison).await?,
        "Fresh destination isn't current"
    );
    for (i, statement) in comparison.plan.statements.iter().enumerate() {
        execute(&destination, &statement.sql)
            .await
            .with_context(|| {
                format!("Applying reviewed test statement {i}: {}", statement.reason)
            })?;
    }
    let equal = compare_live(&source, &destination, &mapping, options).await?;
    ensure!(
        !equal.is_blocked(),
        "Applied schema unexpectedly blocked: {:?}",
        equal.blockers
    );
    ensure!(
        equal.plan.is_empty(),
        "Schemas differ after applying complete plan: {}\n{:?}",
        equal.plan.sql(),
        equal.objects
    );
    ensure!(
        equal
            .objects
            .iter()
            .all(|o| o.change == ChangeKind::Unchanged),
        "Non-equal object summaries after synchronization"
    );
    let encoded = serde_json::to_string(&comparison)?;
    ensure!(
        !encoded.contains("rowboat-test")
            && !encoded.contains("ssl_key")
            && !encoded.contains("identity_file"),
        "Report leaked connection credentials or key paths"
    );
    // Unmodeled destination columns must prevent every executable statement,
    // even if the source would otherwise request their removal.
    execute(&destination,&format!("ALTER TABLE {}.child ADD COLUMN generated_probe INTEGER GENERATED ALWAYS AS (id + 1) STORED",q(&mapping.destination))).await?;
    let blocked = compare_live(&source, &destination, &mapping, options).await?;
    ensure!(
        blocked.is_blocked() && blocked.plan.is_empty(),
        "Generated destination attribute produced unsafe SQL"
    );
    ensure!(
        blocked
            .blockers
            .iter()
            .any(|b| b.to_lowercase().contains("generated")),
        "Missing generated-column blocker"
    );
    execute(
        &destination,
        &format!(
            "ALTER TABLE {}.child DROP COLUMN generated_probe",
            q(&mapping.destination)
        ),
    )
    .await?;
    if engine == Engine::Postgres {
        execute(&source,&format!("CREATE TYPE {}.mood AS ENUM ('calm','busy'); CREATE TABLE {}.custom_type_table (value {}.mood DEFAULT 'calm')",q(&mapping.source),q(&mapping.source),q(&mapping.source))).await?;
        let blocked = compare_live(&source, &destination, &mapping, options).await?;
        ensure!(
            blocked.is_blocked() && blocked.plan.is_empty(),
            "Custom type migration was guessed"
        );
        execute(&source,&format!("DROP TABLE {}.custom_type_table; DROP TYPE {}.mood; CREATE TABLE {}.identity_table (id BIGINT GENERATED ALWAYS AS IDENTITY)",q(&mapping.source),q(&mapping.source),q(&mapping.source))).await?;
        let blocked = compare_live(&source, &destination, &mapping, options).await?;
        ensure!(
            blocked.is_blocked() && blocked.plan.is_empty(),
            "Identity/sequence migration was guessed"
        );
        execute(
            &source,
            &format!("DROP TABLE {}.identity_table", q(&mapping.source)),
        )
        .await?;
    } else {
        execute(
            &source,
            &format!(
                "CREATE INDEX prefix_probe ON {}.child(label(5))",
                q(&mapping.source)
            ),
        )
        .await?;
        let blocked = compare_live(&source, &destination, &mapping, options).await?;
        ensure!(
            blocked.is_blocked() && blocked.plan.is_empty(),
            "Prefix index was silently simplified"
        );
        execute(
            &source,
            &format!("DROP INDEX prefix_probe ON {}.child", q(&mapping.source)),
        )
        .await?;
    }
    let cascade = if engine == Engine::Postgres {
        " CASCADE"
    } else {
        ""
    };
    execute(
        &source,
        &format!("DROP SCHEMA {}{cascade}", q(&mapping.source)),
    )
    .await?;
    execute(
        &destination,
        &format!("DROP SCHEMA {}{cascade}", q(&mapping.destination)),
    )
    .await?;
    Ok(())
}
async fn run(engine: Engine) -> Result<()> {
    let tag = format!(
        "pass12_admin_{:x}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    exercise(engine, &tag).await.with_context(|| {
        format!(
            "Live-diff disposable prefix {tag}; failed-run namespaces are retained for inspection"
        )
    })
}
#[tokio::test]
#[ignore = "Requires parent-owned disposable PostgreSQL 127.0.0.1:15435"]
async fn postgres_live_schema_comparison() -> Result<()> {
    run(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Requires parent-owned disposable MySQL 127.0.0.1:13309"]
async fn mysql_live_schema_comparison() -> Result<()> {
    run(Engine::MySql).await
}
