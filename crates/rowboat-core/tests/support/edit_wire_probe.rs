//! Count exact plain SQL submissions to the explicit task-owned local fixtures.
use anyhow::{Result, ensure};
use rowboat_core::Engine;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};

pub struct WireProbe {
    pub port: u16,
    queries: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}
impl Drop for WireProbe {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl WireProbe {
    pub fn count(&self, sql: &str) -> usize {
        self.queries
            .lock()
            .unwrap()
            .iter()
            .filter(|q| *q == sql)
            .count()
    }
    pub async fn new(engine: Engine) -> Result<Self> {
        ensure!(engine != Engine::Sqlite);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let queries = Arc::new(Mutex::new(Vec::new()));
        let shared = queries.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((client, _)) = accepted else { break };
                        let queries = shared.clone();
                        connections.spawn(async move {
                            client.set_nodelay(true)?;
                            let server = TcpStream::connect(("127.0.0.1", if engine == Engine::Postgres { 15435 } else { 13309 })).await?;
                            server.set_nodelay(true)?;
                            let (mut client_read, mut client_write) = client.into_split();
                            let (mut server_read, mut server_write) = server.into_split();
                            let outgoing = async {
                                let mut startup = engine == Engine::Postgres;
                                loop {
                                    let mut header = vec![0; if engine == Engine::Postgres && !startup { 5 } else { 4 }];
                                    client_read.read_exact(&mut header).await?;
                                    let length = if engine == Engine::Postgres {
                                        let offset = usize::from(!startup);
                                        (u32::from_be_bytes(header[offset..offset+4].try_into().unwrap()) as usize)
                                            .checked_sub(4).ok_or_else(|| anyhow::anyhow!("Invalid frame"))?
                                    } else {
                                        usize::from(header[0]) | usize::from(header[1]) << 8 | usize::from(header[2]) << 16
                                    };
                                    ensure!(length <= 1024*1024);
                                    let mut body = vec![0; length];
                                    client_read.read_exact(&mut body).await?;
                                    let query = if engine == Engine::MySql && body.first() == Some(&0x03) {
                                        Some(&body[1..])
                                    } else if engine == Engine::Postgres && !startup && header[0] == b'Q' {
                                        body.strip_suffix(&[0])
                                    } else { None };
                                    if let Some(query) = query {
                                        queries.lock().unwrap().push(String::from_utf8(query.to_vec())?);
                                    }
                                    startup = false;
                                    server_write.write_all(&header).await?;
                                    server_write.write_all(&body).await?;
                                }
                                #[allow(unreachable_code)]
                                Ok::<(), anyhow::Error>(())
                            };
                            tokio::select! {
                                result = outgoing => result?,
                                result = tokio::io::copy(&mut server_read, &mut client_write) => { result?; }
                            }
                            Ok::<(), anyhow::Error>(())
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self {
            port,
            queries,
            task,
        })
    }
}
