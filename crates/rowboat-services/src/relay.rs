//! An operation-owned loopback TCP → SSH Unix socket byte relay. TLS is opaque.
use anyhow::{Context, Result, ensure};
use std::{net::SocketAddr, os::unix::fs::FileTypeExt, path::Path};
use tokio::{
    net::{TcpListener, UnixStream},
    task::{JoinHandle, JoinSet},
};

pub(crate) struct TcpRelay {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl TcpRelay {
    pub(crate) async fn bind(socket: &Path) -> Result<Self> {
        ensure!(socket.is_absolute(), "SSH socket path must be absolute");
        ensure!(
            std::fs::metadata(socket)
                .context("SSH socket is unavailable; keep its Tunnel alive")?
                .file_type()
                .is_socket(),
            "SSH transport is not a Unix socket"
        );
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let socket = socket.to_owned();
        let task = tokio::spawn(async move {
            // JoinSet aborts ALL in-flight byte pumps when the owning accept task
            // is cancelled. No detached pumps survive a completed operation.
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let Ok((mut tcp, _)) = result else { break };
                        if connections.len() >= 16 { continue; }
                        let socket = socket.clone();
                        connections.spawn(async move {
                            if let Ok(mut unix) = UnixStream::connect(socket).await {
                                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut unix).await;
                            }
                        });
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self { address, task })
    }

    pub(crate) fn port(&self) -> u16 {
        self.address.port()
    }
}

impl Drop for TcpRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpStream, UnixListener},
    };

    #[tokio::test]
    async fn dropping_relay_closes_listener_and_active_bidirectional_pumps() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("ssh.sock");
        let listener = UnixListener::bind(&path)?;
        let relay = TcpRelay::bind(&path).await?;
        let address = relay.address;
        let mut client = TcpStream::connect(address).await?;
        let (mut server, _) = listener.accept().await?;
        client.write_all(b"opaque TLS bytes\0\xff").await?;
        let mut request = [0; 18];
        server.read_exact(&mut request).await?;
        assert_eq!(&request, b"opaque TLS bytes\0\xff");
        server.write_all(b"reply").await?;
        let mut reply = [0; 5];
        client.read_exact(&mut reply).await?;
        assert_eq!(&reply, b"reply");
        drop(relay);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while TcpStream::connect(address).await.is_ok() {
                tokio::task::yield_now().await;
            }
            let mut byte = [0];
            assert_eq!(server.read(&mut byte).await?, 0);
            assert_eq!(client.read(&mut byte).await?, 0);
            Ok::<_, std::io::Error>(())
        })
        .await??;
        Ok(())
    }
}
