//! Narrow WAN-shaped latency/count regression on explicit disposable endpoints.
//! Proxy binds an OS-selected loopback port; no container or OS network changes.
use anyhow::{Result, ensure};
use rowboat_core::{
    ConnectionProfile, Database, Engine, QueryEvent, QueryResult, QueryTimings, sql::quote_ident,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
    time::sleep,
};

#[derive(Default)]
struct Counts {
    armed: AtomicBool,
    pending: AtomicBool,
    exchanges: AtomicUsize,
    delayed: AtomicUsize,
    queries: AtomicUsize,
    prepares: AtomicUsize,
    executes: AtomicUsize,
    closes: AtomicUsize,
}
impl Counts {
    fn reset(&self) {
        for count in [
            &self.exchanges,
            &self.delayed,
            &self.queries,
            &self.prepares,
            &self.executes,
            &self.closes,
        ] {
            count.store(0, Ordering::SeqCst);
        }
        self.armed.store(true, Ordering::SeqCst);
    }
}
struct DelayProxy {
    port: u16,
    counts: Arc<Counts>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for DelayProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl DelayProxy {
    async fn new(engine: Engine, delay: Duration) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let counts = Arc::new(Counts::default());
        let shared = counts.clone();
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let Ok((client,_))=accepted else {break};let counts=shared.clone();
                        tasks.spawn(async move {
                            client.set_nodelay(true)?;
                            let server=TcpStream::connect(("127.0.0.1",if engine==Engine::Postgres {15435} else {13309})).await?;
                            server.set_nodelay(true)?;
                            let (mut cr,mut cw)=client.into_split();let (mut sr,mut sw)=server.into_split();
                            let outgoing=async {
                                let mut startup=engine==Engine::Postgres;let mut batch=Vec::new();
                                loop {
                                    let mut header=vec![0u8;if engine==Engine::Postgres&&!startup {5} else {4}];
                                    if let Err(e)=cr.read_exact(&mut header).await {if e.kind()==std::io::ErrorKind::UnexpectedEof {return Ok::<(),anyhow::Error>(());}return Err(e.into());}
                                    let length=if engine==Engine::Postgres {let offset=usize::from(!startup);(u32::from_be_bytes(header[offset..offset+4].try_into().unwrap()) as usize).checked_sub(4).ok_or_else(||anyhow::anyhow!("Invalid PG frame"))?}else{usize::from(header[0])|usize::from(header[1])<<8|usize::from(header[2])<<16};
                                    ensure!(length<=1024*1024,"Test client frame unexpectedly large");
                                    let mut body=vec![0;length];cr.read_exact(&mut body).await?;
                                    let was_startup=startup;startup=false;
                                    let armed=counts.armed.load(Ordering::SeqCst);
                                    let mut exchange=false;
                                    if armed&&!was_startup {
                                        if engine==Engine::MySql {
                                            match body.first() {
                                                Some(0x03)=>{counts.queries.fetch_add(1,Ordering::SeqCst);exchange=true;},
                                                Some(0x16)=>{counts.prepares.fetch_add(1,Ordering::SeqCst);exchange=true;},
                                                Some(0x17)=>{counts.executes.fetch_add(1,Ordering::SeqCst);exchange=true;},
                                                Some(0x19)=>{counts.closes.fetch_add(1,Ordering::SeqCst);},
                                                Some(0x0e)=>exchange=true,
                                                _=>{}
                                            }
                                        }else{
                                            match header[0] {b'Q'=>{counts.queries.fetch_add(1,Ordering::SeqCst);},b'P'=>{counts.prepares.fetch_add(1,Ordering::SeqCst);},b'E'=>{counts.executes.fetch_add(1,Ordering::SeqCst);},b'C'=>{counts.closes.fetch_add(1,Ordering::SeqCst);},_=>{}}
                                            exchange=matches!(header[0],b'Q'|b'S'|b'H');
                                        }
                                    }
                                    batch.extend_from_slice(&header);batch.extend_from_slice(&body);
                                    // PG extended-protocol groups flush at Sync/Flush;
                                    // buffer their frames so early ParseComplete cannot
                                    // escape the one-delay-per-response-group model.
                                    if armed&&engine==Engine::Postgres&&!exchange&&matches!(header[0],b'P'|b'D'|b'B'|b'E'|b'C') {continue;}
                                    if exchange {counts.exchanges.fetch_add(1,Ordering::SeqCst);counts.pending.store(true,Ordering::SeqCst);}
                                    sw.write_all(&batch).await?;batch.clear();
                                }
                            };
                            let incoming=async {
                                let mut buffer=vec![0;64*1024];
                                loop {let n=sr.read(&mut buffer).await?;if n==0 {return Ok::<(),anyhow::Error>(());}
                                    if counts.pending.swap(false,Ordering::SeqCst) {counts.delayed.fetch_add(1,Ordering::SeqCst);sleep(delay).await;}
                                    cw.write_all(&buffer[..n]).await?;
                                }
                            };
                            tokio::select! {result=outgoing=>result?,result=incoming=>result?};Ok::<(),anyhow::Error>(())
                        });
                    },
                    completed=tasks.join_next(),if !tasks.is_empty()=>{if let Some(Ok(Err(error)))=completed {eprintln!("pass11 proxy: {error:#}");}}
                }
            }
        });
        Ok(Self { port, counts, task })
    }
}
fn profile(engine: Engine, port: u16) -> ConnectionProfile {
    ConnectionProfile {
        engine,
        host: "127.0.0.1".into(),
        port,
        username: if engine == Engine::Postgres {
            "postgres"
        } else {
            "root"
        }
        .into(),
        database: "rowboat".into(),
        ssl_mode: "disable".into(),
        ..Default::default()
    }
}
async fn measure(db: &Database, sql: &str, edit: bool) -> Result<(QueryTimings, u128, u64)> {
    let start = Instant::now();
    let (mut stream, capture) = if edit {
        let (s, c) = db
            .query_stream_with_edit_source(sql, Default::default())
            .await?;
        (s, Some(c))
    } else {
        (db.query_stream(sql, Default::default()).await?, None)
    };
    let mut result = QueryResult::default();
    let mut first = 0;
    let mut complete = false;
    while let Some(event) = stream.recv().await {
        match event {
            QueryEvent::Schema { columns, .. } => {
                first = start.elapsed().as_millis();
                result.columns = columns;
            }
            QueryEvent::Rows { rows, .. } => result.rows.extend(rows),
            QueryEvent::ResultEnd {
                rows_affected,
                elapsed_ms,
                truncated,
                ..
            } => {
                result.rows_affected = rows_affected;
                result.elapsed_ms = elapsed_ms;
                result.truncated = truncated;
            }
            QueryEvent::Complete { .. } => complete = true,
            QueryEvent::Error { message, .. } => anyhow::bail!(message),
        }
    }
    ensure!(complete && result.rows.len() == 10);
    let timings = stream.timings().expect("terminal timings");
    if let Some(capture) = capture {
        ensure!(capture.finish().await.plan(&result)?.result().rows.len() == 10);
    }
    let sum = timings.queue_wait_ms
        + timings.source_capture_ms
        + timings.execution_ms
        + timings.cleanup_ms;
    ensure!(sum <= timings.total_ms && timings.total_ms - sum <= 4);
    Ok((timings, first, result.elapsed_ms))
}

#[tokio::test]
#[ignore = "explicit PostgreSQL15435/MySQL13309, loopback protocol-aware delay proxy"]
async fn high_rtt_capture_counts_and_phase_times() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let admin = Database::connect(
            &profile(
                engine,
                if engine == Engine::Postgres {
                    15435
                } else {
                    13309
                },
            ),
            "rowboat-test",
        )
        .await?;
        let name = format!(
            "pass12_core_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        let quoted = quote_ident(engine, &name);
        admin
            .query(
                &format!(
                    "CREATE {} {quoted}",
                    if engine == Engine::Postgres {
                        "SCHEMA"
                    } else {
                        "DATABASE"
                    }
                ),
                0,
            )
            .await?;
        let result:Result<()>=async {
            admin.query(&format!("CREATE TABLE {quoted}.records (id INT PRIMARY KEY,payload VARCHAR(40))"),0).await?;
            let values=(1..=10).map(|i|format!("({i},'value')")).collect::<Vec<_>>().join(",");admin.query(&format!("INSERT INTO {quoted}.records VALUES {values}"),0).await?;
            let sql=format!("SELECT id,payload FROM {quoted}.records ORDER BY id LIMIT 10");
            let proxy=DelayProxy::new(engine,Duration::from_millis(60)).await?;
            for edit in [false,true] {
                proxy.counts.armed.store(false,Ordering::SeqCst);
                let db=Database::connect(&profile(engine,proxy.port),"rowboat-test").await?;
                for temperature in ["cold","hot"] {
                    proxy.counts.reset();
                    let (timings,first,result_ms)=measure(&db,&sql,edit).await?;
                    let counts=&proxy.counts;let exchanges=counts.exchanges.load(Ordering::SeqCst);let delayed=counts.delayed.load(Ordering::SeqCst);
                    eprintln!("PASS11 {engine:?} edit={edit} {temperature}: first_schema={first} result={result_ms} phases={timings:?} exchanges={exchanges} delayed={delayed} query={} prepare={} execute={} no_reply_close={}",counts.queries.load(Ordering::SeqCst),counts.prepares.load(Ordering::SeqCst),counts.executes.load(Ordering::SeqCst),counts.closes.load(Ordering::SeqCst));
                    ensure!(exchanges==delayed,"Every measured protocol exchange must receive exactly one injected delay");
                    ensure!(timings.total_ms>=60*delayed as u64);
                    if !edit {ensure!(exchanges==1);}
                    else {
                        let budget = match (engine, temperature) {
                            (Engine::Sqlite,_) => unreachable!("network fixture test"),
                            (Engine::MySql, "cold") => 14,
                            (Engine::MySql, _) => 12,
                            (Engine::Postgres, "cold") => 13,
                            (Engine::Postgres, _) => 11,
                        };
                        ensure!(exchanges <= budget, "Provenance exchanges regressed: {exchanges} > {budget}");
                        if engine == Engine::MySql {ensure!(timings.total_ms < 1500, "Fast SELECT exceeded WAN capture wall-time budget: {timings:?}");}
                    }
                }
                if edit {
                    proxy.counts.reset();
                    let sql=format!("SELECT id+0 AS id,payload FROM {quoted}.records ORDER BY id LIMIT 10");
                    let (mut stream,capture)=db.query_stream_with_edit_source(&sql,Default::default()).await?;
                    let mut row_count=0;let mut complete=false;
                    while let Some(event)=stream.recv().await {match event {
                        QueryEvent::Rows {rows,..}=>row_count+=rows.len(),
                        QueryEvent::Complete {..}=>complete=true,
                        QueryEvent::Error {message,..}=>anyhow::bail!(message),
                        _=>{}
                    }}
                    ensure!(complete&&row_count==10&&capture.finish().await.read_only_reason().is_some());
                    ensure!(proxy.counts.exchanges.load(Ordering::SeqCst)==1,"Unsupported SELECT should not perform source probes");
                    eprintln!("PASS11 {engine:?} unsupported expression: {:?}, exchanges=1",stream.timings().unwrap());
                }
                proxy.counts.armed.store(false,Ordering::SeqCst);
            }
            Ok(())
        }.await;
        let cleanup = admin
            .query(
                &match engine {
                    Engine::Sqlite => unreachable!("network fixture test"),
                    Engine::Postgres => format!("DROP SCHEMA {quoted} CASCADE"),
                    Engine::MySql => format!("DROP DATABASE {quoted}"),
                },
                0,
            )
            .await;
        result?;
        cleanup?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "explicit PostgreSQL15435/MySQL13309 fixtures"]
async fn timings_distinguish_session_queue_wait_from_query_execution() -> Result<()> {
    for engine in [Engine::Postgres, Engine::MySql] {
        let db = Database::connect(
            &profile(
                engine,
                if engine == Engine::Postgres {
                    15435
                } else {
                    13309
                },
            ),
            "rowboat-test",
        )
        .await?;
        let operation = db.administrative_operation(None).await?;
        let mut stream = db.query_stream("SELECT 42", Default::default()).await?;
        ensure!(stream.timings().is_none());
        sleep(Duration::from_millis(80)).await;
        operation.finish().await?;
        while let Some(event) = stream.recv().await {
            if let QueryEvent::Error { message, .. } = event {
                anyhow::bail!(message);
            }
        }
        let timings = stream.timings().expect("completed timings");
        ensure!(
            timings.queue_wait_ms >= 75 && timings.source_capture_ms == 0,
            "{timings:?}"
        );
        ensure!(timings.total_ms >= timings.queue_wait_ms);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "explicit MySQL13309 fixture; fresh context/mode and cached catalog regression"]
async fn mysql_hot_catalog_uses_actual_database_and_lexical_mode_after_editor_sql() -> Result<()> {
    let engine = Engine::MySql;
    let db = Database::connect(&profile(engine, 13309), "rowboat-test").await?;
    let prefix = format!(
        "pass12_core_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let names = [format!("{prefix}_a"), format!("{prefix}_b")];
    for (index, name) in names.iter().enumerate() {
        let q = quote_ident(engine, name);
        db.query(&format!("CREATE DATABASE {q}; CREATE TABLE {q}.records (id INT PRIMARY KEY,payload VARCHAR(40)); INSERT INTO {q}.records VALUES({},'context')",index+1),0).await?;
    }
    let result: Result<()> = async {
        // Changing ANSI_QUOTES makes this eligible although the connection's
        // initial lexical mode rejected the double-quoted projection.
        db.query(
            "SET SESSION sql_mode=CONCAT(@@session.sql_mode, ',ANSI_QUOTES,NO_BACKSLASH_ESCAPES')",
            0,
        )
        .await?;
        for index in [0usize, 1, 0] {
            db.query(&format!("USE {}", quote_ident(engine, &names[index])), 0)
                .await?;
            let (mut stream, capture) = db
                .query_stream_with_edit_source(
                    "SELECT \"id\" AS \"key\",payload FROM records",
                    Default::default(),
                )
                .await?;
            let mut result = QueryResult::default();
            let mut complete = false;
            while let Some(event) = stream.recv().await {
                match event {
                    QueryEvent::Schema { columns, .. } => result.columns = columns,
                    QueryEvent::Rows { rows, .. } => result.rows.extend(rows),
                    QueryEvent::ResultEnd {
                        rows_affected,
                        elapsed_ms,
                        truncated,
                        ..
                    } => {
                        result.rows_affected = rows_affected;
                        result.elapsed_ms = elapsed_ms;
                        result.truncated = truncated;
                    }
                    QueryEvent::Complete { .. } => complete = true,
                    QueryEvent::Error { message, .. } => anyhow::bail!(message),
                }
            }
            ensure!(complete && result.rows[0][0].display() == (index + 1).to_string());
            let plan = capture.finish().await.plan(&result)?;
            ensure!(plan.table().schema == names[index] && plan.columns()[0].base_name == "id");
            ensure!(!db.in_transaction().await?);
        }
        Ok(())
    }
    .await;
    let admin = Database::connect(&profile(engine, 13309), "rowboat-test").await?;
    for name in names {
        admin
            .query(&format!("DROP DATABASE {}", quote_ident(engine, &name)), 0)
            .await?;
    }
    result
}

#[tokio::test]
#[ignore = "explicit MySQL13309 fixture and loopback counted proxy"]
async fn completion_snapshot_reuse_is_zero_exchange_and_names_are_one_statement() -> Result<()> {
    let engine = Engine::MySql;
    let proxy = DelayProxy::new(engine, Duration::from_millis(5)).await?;
    let db = Database::connect(&profile(engine, proxy.port), "rowboat-test").await?;
    let namespace = format!("pass12_core_{}", uuid::Uuid::new_v4().simple());
    let q = quote_ident(engine, &namespace);
    db.query(&format!("CREATE DATABASE {q}; CREATE TABLE {q}.records (id INTEGER PRIMARY KEY,payload VARCHAR(40))"),0).await?;
    let result: Result<()> = async {
        db.select_database(&namespace).await?;
        let snapshot = db.namespace_catalog(None).await?;
        proxy.counts.reset();
        let reused = db.completion_catalog_from_snapshot(&snapshot).await?;
        ensure!(reused.search_path == vec![namespace.clone()] && reused.tables.len() == 1);
        ensure!(
            proxy.counts.exchanges.load(Ordering::SeqCst) == 0,
            "Snapshot reuse dispatched SQL"
        );
        let mut tampered = snapshot.clone();
        tampered.tables.clear();
        ensure!(
            db.completion_catalog_from_snapshot(&tampered)
                .await
                .is_err()
        );
        let other = Database::connect(&profile(engine, 13309), "rowboat-test").await?;
        ensure!(
            other
                .completion_catalog_from_snapshot(&snapshot)
                .await
                .is_err()
        );
        proxy.counts.reset();
        let names = db
            .completion_column_names(&snapshot.tables[0].table)
            .await?;
        ensure!(names == vec!["id", "payload"]);
        ensure!(
            proxy.counts.prepares.load(Ordering::SeqCst) == 1
                && proxy.counts.executes.load(Ordering::SeqCst) == 1
                && proxy.counts.queries.load(Ordering::SeqCst) == 0,
            "Names-only lookup fetched additional structure"
        );
        proxy.counts.reset();
        db.completion_catalog_from_snapshot(&snapshot).await?;
        ensure!(proxy.counts.exchanges.load(Ordering::SeqCst) == 0);
        db.query("SELECT 1", 1).await?;
        ensure!(
            db.completion_catalog_from_snapshot(&snapshot)
                .await
                .is_err(),
            "Intervening execution must invalidate snapshot provenance"
        );
        Ok(())
    }
    .await;
    proxy.counts.armed.store(false, Ordering::SeqCst);
    let admin = Database::connect(&profile(engine, 13309), "rowboat-test").await?;
    admin.query(&format!("DROP DATABASE {q}"), 0).await?;
    result
}
