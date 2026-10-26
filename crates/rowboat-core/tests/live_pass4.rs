//! Explicit task-owned Podman fixtures only. Every schema fixture uses a unique
//! name and is removed even when a Result-based assertion fails.
use anyhow::{Context, Result, ensure};
use rowboat_core::*;
use std::time::{Duration, Instant};

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
async fn connect(engine: Engine) -> Result<Database> {
    Database::connect(&profile(engine), "rowboat-test").await
}
fn text(value: impl ToString) -> Cell {
    Cell::Text(value.to_string())
}
fn leaf(column: &str, op: FilterOp, value: Cell) -> FilterExpr {
    FilterExpr::Leaf(Filter {
        column: column.into(),
        op,
        value,
    })
}
fn row(result: &QueryResult, index: usize) -> Vec<(String, Cell)> {
    result
        .columns
        .iter()
        .zip(&result.rows[index])
        .map(|(c, v)| (c.name.clone(), v.clone()))
        .collect()
}

async fn filters_and_relationships(engine: Engine) -> Result<()> {
    let db = connect(engine).await?;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let schema = if engine == Engine::Postgres {
        "public"
    } else {
        "rowboat"
    };
    let parent = TableRef {
        schema: schema.into(),
        name: format!("p4_parent,\"`{}", &suffix[..10]),
    };
    let child = TableRef {
        schema: schema.into(),
        name: format!("p4_child,\"`{}", &suffix[..10]),
    };
    let p = sql::qualified_table(engine, &parent);
    let c = sql::qualified_table(engine, &child);
    let pt = sql::quote_ident(engine, "tenant,\"");
    let pc = sql::quote_ident(engine, "code`\"");
    let ct = sql::quote_ident(engine, "fk,tenant");
    let cc = sql::quote_ident(engine, "fk`code");
    let fk_name = format!("p4_fk,\"`{}", &suffix[..10]);
    let fk = sql::quote_ident(engine, &fk_name);
    let binary = if engine == Engine::Postgres {
        "bytea"
    } else {
        "varbinary(64)"
    };
    let setup=db.query(&format!("CREATE TABLE {p} ({pt} integer NOT NULL, {pc} varchar(40) NOT NULL, PRIMARY KEY ({pt},{pc})); CREATE TABLE {c} (id integer PRIMARY KEY,{ct} integer NULL,{cc} varchar(40) NULL,label text,payload {binary},amount decimal(20,4),CONSTRAINT {fk} FOREIGN KEY ({ct},{cc}) REFERENCES {p}({pt},{pc})); INSERT INTO {p} VALUES(1,'A'),(1,'B'),(2,'A')"),10).await;
    let result = async {
        setup?;
        let attack = "a%_!'; DROP TABLE important; --";
        db.apply_changes(
            &child,
            &[
                RowChange::Insert {
                    values: vec![
                        ("id".into(), text(1)),
                        ("fk,tenant".into(), text(1)),
                        ("fk`code".into(), text("A")),
                        ("label".into(), text(attack)),
                        ("payload".into(), Cell::Binary(vec![0, 255])),
                        ("amount".into(), text(2)),
                    ],
                },
                RowChange::Insert {
                    values: vec![
                        ("id".into(), text(2)),
                        ("fk,tenant".into(), text(1)),
                        ("fk`code".into(), text("B")),
                        ("label".into(), text("b")),
                        ("payload".into(), Cell::Null),
                        ("amount".into(), text(10)),
                    ],
                },
                RowChange::Insert {
                    values: vec![
                        ("id".into(), text(3)),
                        ("fk,tenant".into(), Cell::Null),
                        ("fk`code".into(), Cell::Null),
                        ("label".into(), Cell::Null),
                        ("amount".into(), text(20)),
                    ],
                },
                RowChange::Insert {
                    values: vec![
                        ("id".into(), text(4)),
                        ("fk,tenant".into(), text(2)),
                        ("fk`code".into(), text("A")),
                        ("label".into(), text("b")),
                        ("payload".into(), Cell::Binary(vec![])),
                        ("amount".into(), Cell::Null),
                    ],
                },
            ],
        )
        .await?;
        let request = BrowseRequest {
            limit: 100,
            ..Default::default()
        };
        let expression = FilterExpr::Or(vec![
            leaf("label", FilterOp::Eq, text(attack)),
            FilterExpr::And(vec![
                leaf("fk,tenant", FilterOp::Eq, text(2)),
                leaf("amount", FilterOp::IsNull, Cell::Null),
            ]),
        ]);
        let flat = BrowseRequest {
            filters: vec![Filter {
                column: "id".into(),
                op: FilterOp::Greater,
                value: text(1),
            }],
            ..request.clone()
        };
        let result = db.browse_filtered(&child, &flat, Some(&expression)).await?;
        ensure!(
            result.rows.len() == 1 && result.rows[0][0] == text(4),
            "flat filters must AND the parenthesized OR expression"
        );
        let serde = serde_json::to_string(&expression)?;
        let decoded: FilterExpr = serde_json::from_str(&serde)?;
        ensure!(
            db.browse_filtered(&child, &request, Some(&decoded))
                .await?
                .rows
                .len()
                == 2,
            "serialized expression changed semantics"
        );
        let ranges = FilterExpr::Or(vec![
            leaf("label", FilterOp::Contains, text("%_!';")),
            FilterExpr::Between {
                column: "amount".into(),
                low: text(10),
                high: text(20),
                negated: false,
            },
        ]);
        ensure!(
            db.browse_filtered(&child, &request, Some(&ranges))
                .await?
                .rows
                .len()
                == 3,
            "range/escaped-pattern OR"
        );
        for negated in [false, true] {
            let set = FilterExpr::In {
                column: "fk,tenant".into(),
                values: vec![Cell::Null, text(2)],
                negated,
            };
            let rows = db.browse_filtered(&child, &request, Some(&set)).await?;
            ensure!(
                rows.rows.len() == 2 && rows.rows[0][0] == text(if negated { 1 } else { 3 }),
                "NULL-aware IN complement"
            );
        }
        let binary = FilterExpr::In {
            column: "payload".into(),
            values: vec![Cell::Binary(vec![0, 255]), Cell::Null],
            negated: false,
        };
        ensure!(
            db.browse_filtered(&child, &request, Some(&binary))
                .await?
                .rows
                .len()
                == 3,
            "binary/NULL membership"
        );
        let no_null = FilterExpr::In {
            column: "fk,tenant".into(),
            values: vec![text(1)],
            negated: true,
        };
        ensure!(
            db.browse_filtered(&child, &request, Some(&no_null))
                .await?
                .rows
                .len()
                == 2,
            "NOT IN without NULL must include NULL rows"
        );
        ensure!(
            db.browse_filtered(
                &child,
                &request,
                Some(&leaf("id; DROP TABLE x", FilterOp::Eq, text(1)))
            )
            .await
            .is_err(),
            "unknown identifier accepted"
        );
        let links = db.relationships(&child).await?;
        ensure!(links.len() == 1, "outgoing relationship count");
        let link = &links[0];
        ensure!(
            link.name == fk_name && link.source == child && link.referenced == parent,
            "quoted relationship identities"
        );
        ensure!(
            link.columns.len() == 2
                && link.columns[0].source == "fk,tenant"
                && link.columns[0].referenced == "tenant,\""
                && link.columns[1].source == "fk`code"
                && link.columns[1].referenced == "code`\"",
            "composite key ordering/quoting"
        );
        ensure!(
            link.columns.iter().all(|c| c.nullable),
            "nullable FK metadata"
        );
        ensure!(
            db.relationships(&parent).await?.len() == 1,
            "incoming relationship absent"
        );
        let all = db.browse(&child, &request).await?;
        let target = relationship_filter(link, RelationshipDirection::Referenced, &row(&all, 0))?
            .context("non-NULL relationship missing")?;
        let parent_row = db
            .browse_filtered(&target.table, &request, Some(&target.filter))
            .await?;
        ensure!(
            parent_row.rows.len() == 1,
            "composite parent lookup must not cross tenant"
        );
        let back = relationship_filter(
            link,
            RelationshipDirection::Referencing,
            &row(&parent_row, 0),
        )?
        .context("parent navigation missing")?;
        ensure!(
            db.browse_filtered(&back.table, &request, Some(&back.filter))
                .await?
                .rows
                .len()
                == 1,
            "composite child lookup"
        );
        ensure!(
            relationship_filter(link, RelationshipDirection::Referenced, &row(&all, 2))?.is_none(),
            "NULL FK must not navigate"
        );
        ensure!(
            relationship_filter(
                link,
                RelationshipDirection::Referenced,
                &[("fk,tenant".into(), text(1))]
            )
            .is_err(),
            "partial composite key must fail"
        );
        Ok(())
    }
    .await;
    db.query(
        &format!("DROP TABLE IF EXISTS {c}; DROP TABLE IF EXISTS {p}"),
        0,
    )
    .await?;
    result
}

fn series(engine: Engine, count: usize, projection: &str) -> String {
    match engine {
        Engine::Sqlite => unreachable!("network fixture test"),
        Engine::Postgres => format!("SELECT {projection} FROM generate_series(1,{count}) AS g"),
        Engine::MySql => format!(
            "WITH RECURSIVE n AS (SELECT 1 AS g UNION ALL SELECT g+1 FROM n WHERE g<{count}) SELECT {projection} FROM n"
        ),
    }
}
fn unlimited() -> QueryStreamOptions {
    QueryStreamOptions {
        row_limit: None,
        byte_limit: None,
        batch_rows: 2,
        batch_bytes: 32 * 1024,
        max_cell_bytes: 16 * 1024,
        channel_capacity: 1,
        ..Default::default()
    }
}

async fn progressive(engine: Engine) -> Result<()> {
    let db = connect(engine).await?;
    let slow = series(
        engine,
        100,
        if engine == Engine::Postgres {
            "g, repeat('x',12000), pg_sleep(0.015)"
        } else {
            "g, repeat('x',12000), sleep(0.015)"
        },
    );
    let started = Instant::now();
    let mut stream = db.query_stream(&slow, unlimited()).await?;
    let mut first = None;
    let mut count = 0;
    let mut schema = false;
    let mut ended = false;
    let mut complete = false;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(5), stream.recv()).await? {
        match event {
            QueryEvent::Schema {
                result_index,
                columns,
                metadata_complete,
            } => {
                ensure!(
                    result_index == 0 && columns.len() == 3 && metadata_complete,
                    "schema event"
                );
                schema = true;
            }
            QueryEvent::Rows { result_index, rows } => {
                ensure!(
                    schema && !ended && result_index == 0 && rows.len() <= 2,
                    "row event ordering/bounds"
                );
                if first.is_none() {
                    first = Some(started.elapsed());
                }
                count += rows.len();
            }
            QueryEvent::ResultEnd {
                rows_seen,
                rows_emitted,
                truncated,
                ..
            } => {
                ensure!(
                    rows_seen == 100 && rows_emitted == 100 && !truncated,
                    "unlimited counts"
                );
                ended = true;
            }
            QueryEvent::Complete { result_count, .. } => {
                ensure!(result_count == 1 && ended, "completion ordering");
                complete = true;
            }
            QueryEvent::Error { message, .. } => anyhow::bail!(message),
        }
    }
    ensure!(complete && count == 100, "stream lost rows/completion");
    ensure!(
        started
            .elapsed()
            .saturating_sub(first.context("no progressive batch")?)
            > Duration::from_millis(400),
        "first batch was buffered until query completion"
    );

    let sql = format!(
        "{}; SELECT * FROM rowboat_pass4_definitely_missing",
        series(engine, 20, "g")
    );
    let mut partial = db.query_stream(&sql, unlimited()).await?;
    let mut rows = 0;
    let mut ends = 0;
    let mut error = false;
    while let Some(event) = partial.recv().await {
        match event {
            QueryEvent::Rows { rows: r, .. } => rows += r.len(),
            QueryEvent::ResultEnd { .. } => ends += 1,
            QueryEvent::Error { cancelled, .. } => {
                ensure!(!cancelled, "server error mislabeled cancellation");
                error = true;
            }
            QueryEvent::Complete { .. } => {
                anyhow::bail!("partial failure reported successful completion")
            }
            _ => {}
        }
    }
    ensure!(
        rows == 20 && ends == 1 && error,
        "partial result boundaries lost"
    );
    ensure!(
        db.query("SELECT 7", 10).await?[0].rows[0][0] == text(7),
        "error left protocol unusable"
    );

    for limit in [0, 3] {
        let mut capped = db
            .query_stream(
                &series(engine, 20, "g"),
                QueryStreamOptions {
                    row_limit: Some(limit),
                    ..unlimited()
                },
            )
            .await?;
        let mut count = 0;
        let mut truncated = false;
        while let Some(event) = capped.recv().await {
            match event {
                QueryEvent::Rows { rows, .. } => count += rows.len(),
                QueryEvent::ResultEnd {
                    rows_seen,
                    rows_emitted,
                    truncated: t,
                    ..
                } => {
                    ensure!(
                        rows_seen == 20 && rows_emitted == limit as u64,
                        "cap counts"
                    );
                    truncated = t;
                }
                QueryEvent::Error { message, .. } => anyhow::bail!(message),
                _ => {}
            }
        }
        ensure!(count == limit && truncated, "zero must not mean unlimited");
    }
    let mut huge = db
        .query_stream(
            "SELECT REPEAT('x',2000)",
            QueryStreamOptions {
                max_cell_bytes: 1000,
                batch_bytes: 4096,
                ..unlimited()
            },
        )
        .await?;
    let mut rejected = false;
    while let Some(event) = huge.recv().await {
        if let QueryEvent::Error { message, cancelled } = event {
            ensure!(
                !cancelled && message.contains("max_cell_bytes"),
                "huge cell error context"
            );
            rejected = true;
        }
    }
    ensure!(rejected, "huge cell was silently retained");
    db.query("SELECT 1", 1).await?;
    Ok(())
}

async fn cancellation_and_readonly(engine: Engine) -> Result<()> {
    let db = connect(engine).await?;
    let sql = series(engine, 500, "g,repeat('x',12000)");
    for drop_receiver in [false, true] {
        let mut stream = db
            .query_stream(
                &sql,
                QueryStreamOptions {
                    batch_rows: 1,
                    ..unlimited()
                },
            )
            .await?;
        while let Some(event) = stream.recv().await {
            if matches!(event, QueryEvent::Rows { .. }) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        if drop_receiver {
            drop(stream);
        } else {
            // Database cancellation must wake a full channel even if no receiver
            // calls recv again. close awaits cleanup rather than aborting it.
            db.cancel().await?;
            tokio::time::timeout(Duration::from_secs(5), stream.close()).await??;
        }
        ensure!(
            tokio::time::timeout(Duration::from_secs(5), db.query("SELECT 9", 10)).await??[0].rows
                [0][0]
                == text(9),
            "cancel/drop corrupted next query"
        );
    }
    let stalled = db
        .query_stream(
            &sql,
            QueryStreamOptions {
                batch_rows: 1,
                stall_timeout_ms: 100,
                ..unlimited()
            },
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::time::timeout(Duration::from_secs(5), stalled.close()).await??;
    db.query("SELECT 1", 10).await?;

    let active = db.clone();
    let ongoing = tokio::spawn(async move {
        active
            .query(
                if engine == Engine::Postgres {
                    "SELECT pg_sleep(0.4)"
                } else {
                    "SELECT sleep(0.4)"
                },
                10,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let queued = db.query_stream("SELECT 100", unlimited()).await?;
    queued.close().await?;
    ensure!(
        ongoing.await?.is_ok(),
        "dropping a queued stream cancelled its predecessor"
    );

    let readonly = Database::connect(
        &ConnectionProfile {
            read_only: true,
            ..profile(engine)
        },
        "rowboat-test",
    )
    .await?;
    readonly.begin().await?;
    let payload = "SELECT 1 -- hidden\r; COMMIT; START TRANSACTION READ WRITE; CREATE TABLE rowboat_p4_must_not_exist(id integer)";
    let payload = if engine == Engine::Postgres {
        payload
    } else {
        "SELECT 1 # hidden\n; COMMIT; START TRANSACTION READ WRITE; CREATE TABLE rowboat_p4_must_not_exist(id integer)"
    };
    let mut guarded = readonly.query_stream(payload, unlimited()).await?;
    ensure!(
        matches!(guarded.recv().await, Some(QueryEvent::Error { .. })),
        "readonly batch was not rejected before events"
    );
    ensure!(
        guarded.recv().await.is_none() && readonly.in_transaction().await?,
        "read-only guard changed caller transaction"
    );
    readonly.rollback().await?;
    let mut empty = readonly
        .query_stream("SELECT 1 AS zero_rows WHERE 1=0", unlimited())
        .await?;
    ensure!(
        matches!(empty.recv().await,Some(QueryEvent::Schema{columns,metadata_complete:true,..}) if columns.len()==1),
        "safe native metadata missing for zero rows"
    );
    while let Some(event) = empty.recv().await {
        if let QueryEvent::Error { message, .. } = event {
            anyhow::bail!(message);
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "Explicit task-owned PostgreSQL on 15435"]
async fn postgres_pass4_filters_and_relationships() -> Result<()> {
    filters_and_relationships(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Explicit task-owned MySQL on 13309"]
async fn mysql_pass4_filters_and_relationships() -> Result<()> {
    filters_and_relationships(Engine::MySql).await
}
#[tokio::test]
#[ignore = "Explicit task-owned PostgreSQL on 15435"]
async fn postgres_pass4_progressive_results() -> Result<()> {
    progressive(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Explicit task-owned MySQL on 13309"]
async fn mysql_pass4_progressive_results() -> Result<()> {
    progressive(Engine::MySql).await
}
#[tokio::test]
#[ignore = "Explicit task-owned PostgreSQL on 15435"]
async fn postgres_pass4_backpressure_cancel_drop_and_guards() -> Result<()> {
    cancellation_and_readonly(Engine::Postgres).await
}
#[tokio::test]
#[ignore = "Explicit task-owned MySQL on 13309"]
async fn mysql_pass4_backpressure_cancel_drop_and_guards() -> Result<()> {
    cancellation_and_readonly(Engine::MySql).await
}
