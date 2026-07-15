use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use tili_ipc::{Command, Response};

/// Mirrors `tili-cli`'s own `send()` — duplicated rather than shared, per
/// the precedent already set by `tili_ipc::default_socket_path()`'s doc
/// comment ("each implement this framing directly... since the contract
/// is simpler than a shared sync/async abstraction would be").
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// Longer than `tili-daemon`'s own `WAIT_FOR_CHANGE_TIMEOUT` (30s), so the
/// daemon's own timeout response always has time to arrive before this
/// client-side read timeout would otherwise cut the connection first and
/// get misread as "daemon unreachable."
const WAIT_FOR_CHANGE_TIMEOUT: Duration = Duration::from_secs(35);

/// Sends one length-prefixed JSON `Command` and reads back one
/// length-prefixed JSON `Response`, synchronously, using `SOCKET_TIMEOUT`
/// — for ordinary commands that resolve near-instantly. Called from the
/// menu-click handler thread — never from the main thread, so a slow/hung
/// daemon can't stall the Cocoa event loop.
pub fn send(command: Command) -> io::Result<Response> {
    send_with_timeout(command, SOCKET_TIMEOUT)
}

/// `Command::WaitForChange`, specifically — blocks (on the daemon side)
/// until something changes or its own internal timeout fires, so this
/// needs a client-side read timeout longer than that, not the ordinary
/// `SOCKET_TIMEOUT`. Called from the dedicated background wait thread
/// (see `main.rs`), never the main thread — a blocking read of up to 35s
/// only ever stalls that one thread, not the UI.
pub fn wait_for_change() -> io::Result<Response> {
    send_with_timeout(Command::WaitForChange, WAIT_FOR_CHANGE_TIMEOUT)
}

fn send_with_timeout(command: Command, timeout: Duration) -> io::Result<Response> {
    let mut stream = UnixStream::connect(tili_ipc::default_socket_path())?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;

    let payload = serde_json::to_vec(&command)?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "command too large"))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&payload)?;

    let mut len_buf = [0_u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut response_buf = vec![0_u8; len];
    stream.read_exact(&mut response_buf)?;
    serde_json::from_slice(&response_buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
