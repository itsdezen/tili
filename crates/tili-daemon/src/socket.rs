use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::net::UnixStream;

use tili_ipc::{Command, Response};

/// Binds the daemon's Unix socket, removing a stale socket file left behind
/// by an unclean previous shutdown (a plain `bind` fails with `AddrInUse`
/// otherwise, since the path still exists as a file).
pub fn bind() -> io::Result<UnixListener> {
    let path = tili_ipc::default_socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    UnixListener::bind(&path)
}

/// Reads one length-prefixed JSON `Command` from a connection.
pub async fn read_command(stream: &mut UnixStream) -> io::Result<Command> {
    let mut len_buf = [0_u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes one length-prefixed JSON `Response` to a connection.
pub async fn write_response(stream: &mut UnixStream, response: &Response) -> io::Result<()> {
    let payload = serde_json::to_vec(response)?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "response too large"))?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&payload).await?;
    Ok(())
}
