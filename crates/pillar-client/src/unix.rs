//! Port of packages/client/src/unix.ts (pi v0.84.3): Unix-domain
//! socket transport with path length validation and a pending-byte
//! limit, over std::os::unix::net.
//!
//! divergences: the Node event-loop write queue (promise tail chaining
//! and drain bookkeeping) becomes synchronous blocking writes — the
//! pending-byte limit then only guards against callers queueing more
//! than the limit before a flush; the connect callback fan-out
//! (onData/onClose/onError) is the host's responsibility.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use pillar_protocol::framing::DEFAULT_MAX_FRAME_LENGTH;

use crate::connection::ByteTransport;

/// Maximum Unix socket path length in bytes (upstream
/// `MAX_UNIX_SOCKET_PATH_BYTES`): 107 on Linux, 103 elsewhere.
pub const MAX_UNIX_SOCKET_PATH_BYTES: usize = if cfg!(target_os = "linux") { 107 } else { 103 };

/// Unix transport options (upstream `UnixTransportOptions`).
#[derive(Debug, Clone)]
pub struct UnixTransportOptions {
    pub path: String,
    pub max_pending_bytes: Option<usize>,
}

/// Validate transport options (upstream the factory's synchronous
/// validation) and return the resolved pending limit.
pub fn validate_unix_transport_options(options: &UnixTransportOptions) -> Result<usize, String> {
    if options.path.is_empty() {
        return Err("Unix transport path must not be empty".to_string());
    }
    if options.path.len() > MAX_UNIX_SOCKET_PATH_BYTES {
        return Err(format!(
            "Unix transport path is too long; maximum is {MAX_UNIX_SOCKET_PATH_BYTES} UTF-8 bytes"
        ));
    }
    let max_pending_bytes = options
        .max_pending_bytes
        .unwrap_or(DEFAULT_MAX_FRAME_LENGTH * 4);
    if max_pending_bytes == 0 {
        return Err("Unix transport maxPendingBytes must be a positive safe integer".to_string());
    }
    Ok(max_pending_bytes)
}

/// A Unix-domain socket transport (upstream `UnixByteTransport`).
#[derive(Debug)]
pub struct UnixByteTransport {
    socket: Option<UnixStream>,
    max_pending_bytes: usize,
    closed: bool,
    pending_bytes: usize,
}

impl UnixByteTransport {
    /// Connect and wrap the socket (upstream the connect event branch
    /// of connectUnixSocket).
    pub fn connect(options: &UnixTransportOptions) -> Result<Self, String> {
        let max_pending_bytes = validate_unix_transport_options(options)?;
        let socket = UnixStream::connect(&options.path)
            .map_err(|error| format!("Unix transport connect failed: {error}"))?;
        Ok(Self {
            socket: Some(socket),
            max_pending_bytes,
            closed: false,
            pending_bytes: 0,
        })
    }

    fn mark_closed(&mut self) {
        self.closed = true;
        self.socket = None;
    }
}

impl ByteTransport for UnixByteTransport {
    fn send(&mut self, chunk: &[u8]) -> Result<(), String> {
        if self.closed {
            return Err("Unix transport is closed".to_string());
        }
        if self.pending_bytes + chunk.len() > self.max_pending_bytes {
            return Err("Unix transport exceeded its pending byte limit".to_string());
        }
        self.pending_bytes += chunk.len();
        let result = match &mut self.socket {
            Some(socket) => socket
                .write_all(chunk)
                .and_then(|_| socket.flush())
                .map_err(|error| format!("Unix transport write failed: {error}")),
            None => Err("Unix transport is closed".to_string()),
        };
        self.pending_bytes = self.pending_bytes.saturating_sub(chunk.len());
        if result.is_err() {
            self.mark_closed();
        }
        result
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.mark_closed();
    }
}

/// Read inbound bytes into `buffer` (host event-loop helper; upstream
/// the socket 'data' event). Returns the number of bytes read, 0 at
/// EOF.
pub fn read_inbound(
    transport: &mut UnixByteTransport,
    buffer: &mut [u8],
) -> std::io::Result<usize> {
    transport
        .socket
        .as_mut()
        .map(|socket| socket.read(buffer))
        .unwrap_or(Ok(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(path: &str) -> UnixTransportOptions {
        UnixTransportOptions {
            path: path.to_string(),
            max_pending_bytes: None,
        }
    }

    // --- option validation ------------------------------------------------------------------------

    #[test]
    fn empty_path_rejected() {
        assert_eq!(
            validate_unix_transport_options(&options("")).unwrap_err(),
            "Unix transport path must not be empty"
        );
    }

    #[test]
    fn oversized_path_rejected() {
        let long_path = "/".repeat(MAX_UNIX_SOCKET_PATH_BYTES + 1);
        let error = validate_unix_transport_options(&options(&long_path)).unwrap_err();
        assert!(error.contains("too long"), "{error}");
        assert!(error.contains(&MAX_UNIX_SOCKET_PATH_BYTES.to_string()));
    }

    #[test]
    fn path_at_limit_is_accepted() {
        let path = "/".repeat(MAX_UNIX_SOCKET_PATH_BYTES);
        assert!(validate_unix_transport_options(&options(&path)).is_ok());
    }

    #[test]
    fn zero_pending_bytes_rejected() {
        let options = UnixTransportOptions {
            path: "/tmp/x".to_string(),
            max_pending_bytes: Some(0),
        };
        assert_eq!(
            validate_unix_transport_options(&options).unwrap_err(),
            "Unix transport maxPendingBytes must be a positive safe integer"
        );
    }

    #[test]
    fn default_pending_bytes_is_four_frames() {
        let resolved = validate_unix_transport_options(&options("/tmp/x")).unwrap();
        assert_eq!(resolved, DEFAULT_MAX_FRAME_LENGTH * 4);
    }

    // --- socket transport ---------------------------------------------------------------------------

    #[test]
    fn connect_to_missing_socket_fails() {
        let error =
            UnixByteTransport::connect(&options("/tmp/definitely-not-here-pi-socket")).unwrap_err();
        assert!(error.contains("connect failed"), "{error}");
    }

    #[test]
    fn send_after_close_fails() {
        let dir = std::env::temp_dir().join(format!("pillar-client-unix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let mut transport = UnixByteTransport::connect(&options(path.to_str().unwrap())).unwrap();
        transport.close();
        transport.close(); // idempotent
        assert_eq!(
            transport.send(b"x"),
            Err("Unix transport is closed".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn send_and_receive_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("pillar-client-unix-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 5];
            std::io::Read::read_exact(&mut stream, &mut buffer).unwrap();
            buffer
        });
        let mut transport = UnixByteTransport::connect(&options(path.to_str().unwrap())).unwrap();
        transport.send(b"hello").unwrap();
        assert_eq!(server.join().unwrap(), *b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_limit_rejects_oversized_writes() {
        let dir =
            std::env::temp_dir().join(format!("pillar-client-unix-pl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pl.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let mut transport = UnixByteTransport::connect(&UnixTransportOptions {
            path: path.to_str().unwrap().to_string(),
            max_pending_bytes: Some(4),
        })
        .unwrap();
        transport.send(b"abcd").unwrap();
        // The write completed synchronously, so the pending counter is
        // released; the limit applies per queued write.
        assert_eq!(
            transport.send(b"abcde"),
            Err("Unix transport exceeded its pending byte limit".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_inbound_on_closed_transport_returns_eof() {
        let mut transport = UnixByteTransport {
            socket: None,
            max_pending_bytes: 100,
            closed: true,
            pending_bytes: 0,
        };
        let mut buffer = [0u8; 10];
        assert_eq!(read_inbound(&mut transport, &mut buffer).unwrap(), 0);
    }
}
