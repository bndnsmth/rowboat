//! Explicit disposable Podman fixtures only; each run owns a unique table.
use anyhow::Result;
use rowboat_core::grid::*;
use rowboat_core::*;
use std::time::{SystemTime, UNIX_EPOCH};

fn profile(engine: Engine) -> ConnectionProfile {
    ConnectionProfile {
        name: "Disposable grid integration fixture".into(),
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

fn text(value: impl ToString) -> Cell {
    Cell::Text(value.to_string())
}
fn pos(row: usize, column: usize) -> GridPosition {
    GridPosition { row, column }
}

async fn load(db: &Database, qualified: &str) -> Result<QueryResult> {
    Ok(db
        .query(
            &format!(
                "SELECT label, note, payload, id, tenant FROM {qualified} ORDER BY tenant, id"
            ),
            100,
        )
        .await?
        .remove(0))
}

async fn exercise(engine: Engine) -> Result<()> {
    let profile = profile(engine);
    let db = Database::connect(&profile, "rowboat-test").await?;
    let table = TableRef {
        schema: if engine == Engine::Postgres {
            "public"
        } else {
            "rowboat"
        }
        .into(),
        name: format!(
            "rowboat_grid_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ),
    };
    let qualified = rowboat_core::sql::qualified_table(engine, &table);
    let binary = if engine == Engine::Postgres {
        "BYTEA"
    } else {
        "BLOB"
    };
    db.query(&format!("CREATE TABLE {qualified} (tenant INTEGER NOT NULL, id INTEGER NOT NULL, label VARCHAR(255) NOT NULL, note TEXT NULL, payload {binary} NULL, PRIMARY KEY (tenant, id))"), 100).await?;
    let outcome: Result<()> = async {
        let inserts = (1..=3)
            .map(|id| RowChange::Insert {
                values: vec![
                    ("tenant".into(), text(9)),
                    ("id".into(), text(id)),
                    ("label".into(), text(format!("row {id}"))),
                    ("note".into(), text("original")),
                    ("payload".into(), Cell::Binary(vec![1, 2, 3])),
                ],
            })
            .collect::<Vec<_>>();
        assert_eq!(db.apply_changes(&table, &inserts).await?, 3);
        let structure = db.structure(&table).await?;
        assert_eq!(
            structure.columns.iter().filter(|c| c.primary_key).count(),
            2
        );
        let original = load(&db, &qualified).await?;
        let limits = GridLimits::default();
        let prior = stage_fill(
            &original,
            &structure,
            GridRange::new(pos(0, 0), pos(0, 0))?,
            text("pre-staged"),
            &[],
            limits,
        )?;
        let pasted = parse_tsv(
            "\"雪\t\"\"quoted\"\"\r\nline\"\t\\x00ff0a22\nNULL\t\\x\n\\\\N\t\\N",
            ClipboardDialect::Rowboat,
            limits,
        )?;
        let staged = stage_paste(&original, &structure, pos(0, 1), &pasted, &prior, limits)?;
        assert_eq!(staged.len(), 3);
        for change in &staged {
            let RowChange::Update { original, .. } = change else {
                panic!("expected update")
            };
            assert_eq!(
                original.len(),
                5,
                "full row required for conflict detection"
            );
            assert_eq!(original[0].0, "tenant");
            assert_eq!(original[1].0, "id");
        }
        assert_eq!(
            load(&db, &qualified).await?.rows,
            original.rows,
            "pure staging must not execute SQL"
        );
        assert_eq!(db.apply_changes(&table, &staged).await?, 3);
        let applied = load(&db, &qualified).await?;
        assert_eq!(
            applied.rows[0],
            vec![
                text("pre-staged"),
                text("雪\t\"quoted\"\r\nline"),
                Cell::Binary(vec![0, 255, 10, 34]),
                text(1),
                text(9)
            ]
        );
        assert_eq!(applied.rows[1][1], text("NULL"));
        assert_eq!(applied.rows[1][2], Cell::Binary(vec![]));
        assert_eq!(applied.rows[2][1], text("\\N"));
        assert_eq!(applied.rows[2][2], Cell::Null);
        let all = GridRange::new(pos(2, 4), pos(0, 0))?;
        let clipboard = export_tsv(&applied, all, ClipboardDialect::Rowboat, limits)?;
        assert_eq!(
            parse_tsv(&clipboard, ClipboardDialect::Rowboat, limits)?.rows(),
            applied.rows
        );

        // A late conflict must roll back successful earlier statements in the batch.
        let overwrite = stage_fill(
            &applied,
            &structure,
            GridRange::new(pos(0, 0), pos(2, 0))?,
            text("must roll back"),
            &[],
            limits,
        )?;
        let concurrent = Database::connect(&profile, "rowboat-test").await?;
        let concurrent_edit = stage_fill(
            &applied,
            &structure,
            GridRange::new(pos(2, 1), pos(2, 1))?,
            text("concurrent writer"),
            &[],
            limits,
        )?;
        concurrent.apply_changes(&table, &concurrent_edit).await?;
        let concurrent_rows = load(&db, &qualified).await?;
        assert!(matches!(
            stage_fill(
                &concurrent_rows,
                &structure,
                GridRange::new(pos(0, 0), pos(2, 0))?,
                text("new paste"),
                &overwrite,
                limits
            ),
            Err(GridError::StaleOriginal)
        ));
        assert!(
            db.apply_changes(&table, &overwrite).await.is_err(),
            "database must reject stale full-row originals"
        );
        assert_eq!(
            load(&db, &qualified).await?.rows,
            concurrent_rows.rows,
            "late conflict must roll back earlier updates"
        );

        // Pure grid staging is permission-independent; execution enforces read-only.
        let mut readonly_profile = profile.clone();
        readonly_profile.read_only = true;
        let readonly = Database::connect(&readonly_profile, "rowboat-test").await?;
        let clears = stage_clear(
            &concurrent_rows,
            &structure,
            GridRange::new(pos(0, 1), pos(2, 1))?,
            &[],
            limits,
        )?;
        assert!(readonly.apply_changes(&table, &clears).await.is_err());
        assert_eq!(load(&db, &qualified).await?.rows, concurrent_rows.rows);
        assert_eq!(db.apply_changes(&table, &clears).await?, 3);
        let cleared = load(&db, &qualified).await?;
        assert!(cleared.rows.iter().all(|row| row[1] == Cell::Null));

        // Plain external TSV markers remain literal text through real SQL binding.
        let plain = parse_tsv("\\N\nNULL\n\"\"", ClipboardDialect::Spreadsheet, limits)?;
        let literal = stage_paste(&cleared, &structure, pos(0, 1), &plain, &[], limits)?;
        assert_eq!(db.apply_changes(&table, &literal).await?, 3);
        let final_rows = load(&db, &qualified).await?;
        assert_eq!(
            final_rows
                .rows
                .iter()
                .map(|row| row[1].clone())
                .collect::<Vec<_>>(),
            vec![text("\\N"), text("NULL"), text("")]
        );
        Ok(())
    }
    .await;
    let cleanup = db.query(&format!("DROP TABLE {qualified}"), 100).await;
    outcome?;
    cleanup?;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires explicit task-owned Podman PostgreSQL on 15435"]
async fn grid_postgres_live() -> Result<()> {
    exercise(Engine::Postgres).await
}

#[tokio::test]
#[ignore = "Requires explicit task-owned Podman MySQL on 13309"]
async fn grid_mysql_live() -> Result<()> {
    exercise(Engine::MySql).await
}
