use rowboat_core::{
    Cell, ConnectionProfile, Database, Engine, QueryEvent, QueryStreamOptions, SqliteFileOptions,
    SqliteOpenMode,
    execution::{self, RunScope},
};

#[test]
fn pass22_boundaries_preserve_dialect_quotes_comments_and_unicode() {
    let fixtures = [
        (
            Engine::Postgres,
            "-- lead\nSELECT 'é;🙂', E'a\\\';b'; /* outer /* nested ; */ */ SELECT $tag$x;y$tag$; SELECT \"semi;colon\";",
            3,
        ),
        (
            Engine::Postgres,
            "SELECT E'one'\n'continued\\\';still'; SELECT 2",
            2,
        ),
        (
            Engine::Postgres,
            "CREATE RULE r AS ON INSERT TO x DO (INSERT INTO y VALUES(1); INSERT INTO y VALUES(2);); SELECT 3;",
            2,
        ),
        (
            Engine::MySql,
            "# comment;\nSELECT 'a;''b', `odd;name`; -- comment ;\n SELECT 2;",
            2,
        ),
        (Engine::MySql, "SELECT 1--notcomment;SELECT 2", 2),
        (
            Engine::Sqlite,
            "--withoutspace;\nSELECT [semi;name], `other;name`, 'value;'; /* ; */ SELECT 2;",
            2,
        ),
    ];
    for (engine, sql, count) in fixtures {
        let ranges = execution::statement_ranges(engine, sql).unwrap();
        assert_eq!(ranges.len(), count, "{engine:?}: {sql}");
        for range in ranges {
            assert!(sql.is_char_boundary(range.start) && sql.is_char_boundary(range.end));
            assert_eq!(
                execution::execution_range(engine, sql, 0..0, range.start, RunScope::Current)
                    .unwrap(),
                range
            );
        }
    }
}
#[test]
fn pass22_sqlite_trigger_native_completeness_keeps_entire_body() {
    let sql = "CREATE TEMP TRIGGER t AFTER INSERT ON x BEGIN INSERT INTO y VALUES(CASE WHEN new.x=1 THEN ';' ELSE 'END' END); UPDATE y SET x=';'; END; SELECT 8;";
    let ranges = execution::statement_ranges(Engine::Sqlite, sql).unwrap();
    assert_eq!(ranges.len(), 2);
    assert!(sql[ranges[0].clone()].ends_with("END;"));
    assert_eq!(&sql[ranges[1].clone()], "SELECT 8;");
    assert!(
        execution::statement_ranges(
            Engine::Sqlite,
            "CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT 1;"
        )
        .is_err()
    );
}
#[test]
fn pass22_current_never_broadens_ambiguous_or_client_script_input() {
    for (engine, sql) in [
        (
            Engine::MySql,
            "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$",
        ),
        (
            Engine::MySql,
            "CREATE DEFINER='root'@'localhost' PROCEDURE p() BEGIN SELECT 1; SELECT 2; END; SELECT 9;",
        ),
        (Engine::MySql, "SELECT 'a\\'; SELECT 9;"),
        (Engine::MySql, "/*!50000 SELECT 1; SELECT 2 */;"),
        (Engine::MySql, "BEGIN NOT ATOMIC SELECT 1; SELECT 2; END;"),
        (
            Engine::Postgres,
            "CREATE FUNCTION f() RETURNS void LANGUAGE SQL BEGIN ATOMIC SELECT 1; SELECT 2; END;",
        ),
        (Engine::Postgres, "SELECT 'unterminated"),
        (Engine::Postgres, "DO $x$BEGIN RAISE NOTICE 'x'; END;"),
        (Engine::Postgres, "\\i other.sql"),
        (Engine::Postgres, "COPY x FROM STDIN;"),
        (Engine::Postgres, "SELECT (1; SELECT 2;"),
        (Engine::Sqlite, ".read other.sql"),
    ] {
        assert!(
            execution::execution_range(engine, sql, 0..0, 0, RunScope::Current).is_err(),
            "{sql}"
        );
        assert_eq!(
            execution::execution_range(engine, sql, 0..0, 0, RunScope::All).unwrap(),
            0..sql.len()
        );
        assert_eq!(
            execution::execution_range(engine, sql, 0..sql.len(), 0, RunScope::Selection).unwrap(),
            0..sql.len()
        );
    }
}
#[test]
fn pass22_scope_selection_whitespace_edges_and_character_coordinates() {
    let sql = "SELECT 'é';\n  SELECT '🙂';  ";
    let ranges = execution::statement_ranges(Engine::Postgres, sql).unwrap();
    assert_eq!(
        execution::execution_range(
            Engine::Postgres,
            sql,
            0..0,
            ranges[0].end,
            RunScope::Current
        )
        .unwrap(),
        ranges[1]
    );
    assert_eq!(
        execution::execution_range(Engine::Postgres, sql, 0..0, sql.len(), RunScope::Current)
            .unwrap(),
        ranges[1]
    );
    assert_eq!(
        execution::execution_range(Engine::Postgres, sql, ranges[0].clone(), 0, RunScope::All)
            .unwrap(),
        0..sql.len()
    );
    assert_eq!(
        execution::execution_range(
            Engine::Postgres,
            sql,
            ranges[0].clone(),
            0,
            RunScope::SelectionOrAll
        )
        .unwrap(),
        ranges[0]
    );
    assert!(
        execution::execution_range(Engine::Postgres, sql, 0..0, 0, RunScope::Selection).is_err()
    );
    assert!(
        execution::statement_ranges(Engine::Postgres, " -- trivia only\n ; /* comment */")
            .unwrap()
            .is_empty()
    );
    assert_eq!(execution::character_position("é🙂x", 3), Some(6));
    assert_eq!(execution::character_position("é🙂x", 4), Some(7));
    assert_eq!(execution::character_position("é🙂x", 0), None);
    assert_eq!(execution::character_position("é🙂x", 5), None);
}

async fn stream_failure(
    db: &Database,
    sql: &str,
) -> anyhow::Result<(String, Option<execution::ErrorPosition>, usize)> {
    let mut stream = db.query_stream(sql, QueryStreamOptions::default()).await?;
    let mut ended = 0;
    while let Some(event) = stream.recv().await {
        match event {
            QueryEvent::ResultEnd { .. } => ended += 1,
            QueryEvent::Error { message, .. } => {
                let position = stream.error_position();
                stream.close().await?;
                return Ok((message, position, ended));
            }
            _ => {}
        }
    }
    anyhow::bail!("Expected a server error")
}

#[tokio::test]
async fn pass22_sqlite_current_trigger_and_later_error_exact_offset() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("execution.sqlite");
    let profile = ConnectionProfile {
        engine: Engine::Sqlite,
        file: Some(SqliteFileOptions {
            path: path.clone(),
            mode: SqliteOpenMode::CreateIfMissing,
        }),
        ..Default::default()
    };
    let db = Database::connect(&profile, "").await?;
    db.query("CREATE TABLE x(v TEXT); CREATE TABLE y(v TEXT)", 10)
        .await?;
    let sql = "CREATE TRIGGER t AFTER INSERT ON x BEGIN INSERT INTO y VALUES(CASE WHEN new.v='é' THEN ';' ELSE 'END' END); INSERT INTO y VALUES('second'); END; INSERT INTO x VALUES('é'); SELECT * FROM y;";
    let ranges = execution::statement_ranges(Engine::Sqlite, sql)?;
    for range in ranges {
        db.query(&sql[range], 10).await?;
    }
    let rows = db.query("SELECT v FROM y", 10).await?;
    assert_eq!(
        rows[0].rows,
        vec![
            vec![Cell::Text(";".into())],
            vec![Cell::Text("second".into())]
        ]
    );
    let sql = "SELECT 'é🙂';\nSELECT 42 + FROM x;";
    let (message, position, ended) = stream_failure(&db, sql).await?;
    assert!(message.contains("syntax"), "{message}");
    assert_eq!(ended, 1);
    assert_eq!(position.unwrap().byte_offset, sql.find("FROM").unwrap());
    assert_eq!(
        db.query("SELECT COUNT(*) FROM y", 1).await?[0].rows[0][0],
        Cell::Text("2".into())
    );
    drop(db);
    std::fs::remove_file(path)?;
    Ok(())
}

fn network(engine: Engine) -> ConnectionProfile {
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
            "rowboat"
        }
        .into(),
        database: "rowboat".into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    }
}
#[tokio::test]
#[ignore = "Explicit disposable parent-owned PostgreSQL15435/MySQL13309 only"]
async fn pass22_live_postgres_positions_and_mysql_no_guessed_offsets() -> anyhow::Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let db = Database::connect(&network(engine), "rowboat-test").await?;
        let name = format!("pass22_exec_{}", uuid::Uuid::new_v4().simple());
        db.query(&format!("CREATE TEMPORARY TABLE {name}(v INT)"), 1)
            .await?;
        let sql = format!(
            "INSERT INTO {name} VALUES(1); INSERT INTO {name} VALUES(2); SELECT v FROM {name} ORDER BY v;"
        );
        let ranges = execution::statement_ranges(engine, &sql)?;
        let range =
            execution::execution_range(engine, &sql, 0..0, ranges[1].start, RunScope::Current)?;
        db.query(&sql[range], 10).await?;
        assert_eq!(
            db.query(&format!("SELECT v FROM {name}"), 10).await?[0].rows,
            vec![vec![Cell::Text("2".into())]]
        );
        let sql = "SELECT 'é🙂'; SELECT 42 + FROM nowhere;";
        let (_, position, _) = stream_failure(&db, sql).await?;
        if engine == Engine::Postgres {
            assert_eq!(position.unwrap().byte_offset, sql.find("FROM").unwrap());
            let sql = "DO $body$BEGIN EXECUTE 'SELECT 2 + FROM nowhere'; END$body$;";
            let (_, position, _) = stream_failure(&db, sql).await?;
            assert!(
                position.is_none(),
                "internal function SQL must not map onto the editor batch"
            );
        } else {
            assert!(position.is_none());
            let routine = format!("pass22_routine_{}", uuid::Uuid::new_v4().simple());
            let definition =
                format!("CREATE PROCEDURE {routine}() BEGIN SELECT 'a;b'; SELECT 22; END");
            assert!(
                execution::execution_range(engine, &definition, 0..0, 0, RunScope::Current)
                    .is_err()
            );
            let selected = execution::execution_range(
                engine,
                &definition,
                0..definition.len(),
                0,
                RunScope::Selection,
            )?;
            db.query(&definition[selected], 10).await?;
            let values = db.query(&format!("CALL {routine}()"), 10).await?;
            assert_eq!(values[0].rows[0][0], Cell::Text("a;b".into()));
            assert_eq!(values[1].rows[0][0], Cell::Text("22".into()));
            db.query(&format!("DROP PROCEDURE {routine}"), 1).await?;
        }
        db.query(&format!("DROP TABLE {name}"), 1).await?;
    }
    Ok(())
}
