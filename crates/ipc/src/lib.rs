//! # kanodesk-ipc
//!
//! Local IPC between KanoDesk processes on one machine (02 §2): the host
//! (server) process and the user-facing processes such as the connection
//! manager.
//!
//! - Transport: named pipes on Windows (`\\.\pipe\kanodesk-<session>-<role>`,
//!   one name per Windows session), Unix sockets elsewhere.
//! - Framing: a 4-byte little-endian length, then a postcard-encoded message
//!   (at most [`MAX_MESSAGE`] bytes).
//! - Access: the server refuses remote clients and clients from another
//!   Windows session (`GetNamedPipeClientProcessId` + `ProcessIdToSessionId`;
//!   other users on a Unix socket); the first pipe instance is created
//!   exclusively so nobody can squat the name once the server runs.
//! - [`cm`]: the messages between the host and the connection manager.
//! - [`control`]: the messages between the host and the main window.
//!
//! Specified by:
//! - `docs/02-architecture.md`
//!
//! Status: transport, connection-manager and main-window messages.

pub mod cm;
pub mod control;

use std::io;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &["docs/02-architecture.md"];

/// Largest message accepted.
pub const MAX_MESSAGE: usize = 1 << 20;

/// IPC failures.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// Pipe or socket I/O.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// A message that does not decode, or is too large.
    #[error("bad message: {0}")]
    Codec(String),
}

/// A byte stream an IPC connection runs over.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// Sends messages on one connection.
pub struct Sender {
    inner: WriteHalf<Box<dyn Stream>>,
}

/// Receives messages on one connection.
pub struct Receiver {
    inner: ReadHalf<Box<dyn Stream>>,
}

/// One IPC connection.
pub struct Connection {
    stream: Box<dyn Stream>,
}

impl Connection {
    /// Wraps any byte stream (tests use in-memory pipes).
    pub fn new(stream: impl Stream + 'static) -> Connection {
        Connection {
            stream: Box::new(stream),
        }
    }

    /// Splits into independent send and receive halves.
    pub fn split(self) -> (Sender, Receiver) {
        let (r, w) = tokio::io::split(self.stream);
        (Sender { inner: w }, Receiver { inner: r })
    }
}

/// Encodes one framed message.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, IpcError> {
    let body = postcard::to_stdvec(msg).map_err(|e| IpcError::Codec(e.to_string()))?;
    if body.len() > MAX_MESSAGE {
        return Err(IpcError::Codec(format!(
            "{} bytes is too large",
            body.len()
        )));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

impl Sender {
    /// Sends one message.
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), IpcError> {
        let frame = encode(msg)?;
        self.inner.write_all(&frame).await?;
        self.inner.flush().await?;
        Ok(())
    }
}

impl Receiver {
    /// Receives one message; `None` when the peer closed the connection
    /// cleanly between messages.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>, IpcError> {
        let mut len = [0u8; 4];
        match self.inner.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_le_bytes(len) as usize;
        if len > MAX_MESSAGE {
            return Err(IpcError::Codec(format!("{len} bytes is too large")));
        }
        let mut body = vec![0u8; len];
        self.inner.read_exact(&mut body).await?;
        postcard::from_bytes(&body)
            .map(Some)
            .map_err(|e| IpcError::Codec(e.to_string()))
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use windows::Win32::Foundation::{ERROR_PIPE_BUSY, HANDLE};
    use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;

    use super::Connection;

    /// The Windows session a process runs in.
    pub fn session_of(pid: u32) -> io::Result<u32> {
        let mut session = 0u32;
        // SAFETY: valid out pointer.
        unsafe { ProcessIdToSessionId(pid, &mut session) }.map_err(io::Error::other)?;
        Ok(session)
    }

    /// The pipe name for `role` in this process's Windows session.
    pub fn endpoint_name(role: &str) -> String {
        let session = session_of(std::process::id()).unwrap_or(0);
        format!(r"\\.\pipe\kanodesk-{session}-{role}")
    }

    /// Accepts clients on a named pipe.
    pub struct Listener {
        name: String,
        next: NamedPipeServer,
        session: u32,
    }

    impl Listener {
        /// Creates the pipe; fails if another process already owns the name.
        pub fn bind(name: &str) -> io::Result<Listener> {
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(name)?;
            Ok(Listener {
                name: name.to_owned(),
                next,
                session: session_of(std::process::id())?,
            })
        }

        /// Waits for the next client from this Windows session.
        pub async fn accept(&mut self) -> io::Result<Connection> {
            loop {
                self.next.connect().await?;
                let fresh = ServerOptions::new()
                    .reject_remote_clients(true)
                    .create(&self.name)?;
                let server = std::mem::replace(&mut self.next, fresh);
                let mut pid = 0u32;
                // SAFETY: a connected pipe handle; valid out pointer.
                let ok = unsafe {
                    GetNamedPipeClientProcessId(HANDLE(server.as_raw_handle()), &mut pid)
                }
                .is_ok();
                if ok && session_of(pid).ok() == Some(self.session) {
                    return Ok(Connection::new(server));
                }
                // Another session's process: drop it and keep listening.
            }
        }
    }

    /// Connects to a named pipe, waiting briefly while all instances are busy.
    pub async fn connect(name: &str) -> io::Result<Connection> {
        for _ in 0..50 {
            match ClientOptions::new().open(name) {
                Ok(client) => return Ok(Connection::new(client)),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "pipe busy"))
    }
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::path::PathBuf;

    use tokio::net::{UnixListener, UnixStream};

    use super::Connection;

    fn uid() -> u32 {
        // SAFETY: getuid never fails.
        unsafe { libc_getuid() }
    }

    unsafe extern "C" {
        #[link_name = "getuid"]
        fn libc_getuid() -> u32;
    }

    /// The socket path for `role` for this user.
    pub fn endpoint_name(role: &str) -> String {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        dir.join(format!("kanodesk-{}-{role}.sock", uid()))
            .to_string_lossy()
            .into_owned()
    }

    /// Accepts clients on a Unix socket.
    pub struct Listener {
        inner: UnixListener,
    }

    impl Listener {
        /// Binds the socket (replacing a stale one).
        pub fn bind(name: &str) -> io::Result<Listener> {
            let _ = std::fs::remove_file(name);
            Ok(Listener {
                inner: UnixListener::bind(name)?,
            })
        }

        /// Waits for the next client of this user.
        pub async fn accept(&mut self) -> io::Result<Connection> {
            loop {
                let (stream, _) = self.inner.accept().await?;
                if stream.peer_cred().map(|c| c.uid()).ok() == Some(uid()) {
                    return Ok(Connection::new(stream));
                }
            }
        }
    }

    /// Connects to a Unix socket.
    pub async fn connect(name: &str) -> io::Result<Connection> {
        Ok(Connection::new(UnixStream::connect(name).await?))
    }
}

pub use platform::{Listener, connect, endpoint_name};

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Msg {
        Ping(u32),
        Text(String),
    }

    #[tokio::test]
    async fn messages_round_trip_over_any_stream() {
        let (a, b) = tokio::io::duplex(1024);
        let (mut tx, _) = Connection::new(a).split();
        let (_, mut rx) = Connection::new(b).split();
        tx.send(&Msg::Ping(7)).await.unwrap();
        tx.send(&Msg::Text("hé".into())).await.unwrap();
        drop(tx);
        assert_eq!(rx.recv::<Msg>().await.unwrap(), Some(Msg::Ping(7)));
        assert_eq!(
            rx.recv::<Msg>().await.unwrap(),
            Some(Msg::Text("hé".into()))
        );
        assert_eq!(rx.recv::<Msg>().await.unwrap(), None, "clean close");
    }

    #[tokio::test]
    async fn oversized_and_garbage_frames_are_errors() {
        let (mut a, b) = tokio::io::duplex(64);
        let (_, mut rx) = Connection::new(b).split();
        a.write_all(&(MAX_MESSAGE as u32 + 1).to_le_bytes())
            .await
            .unwrap();
        assert!(matches!(rx.recv::<Msg>().await, Err(IpcError::Codec(_))));

        let (mut a, b) = tokio::io::duplex(64);
        let (_, mut rx) = Connection::new(b).split();
        a.write_all(&[3, 0, 0, 0, 9, 9, 9]).await.unwrap();
        assert!(matches!(rx.recv::<Msg>().await, Err(IpcError::Codec(_))));
        // Truncated frame: an I/O error, not a clean close.
        let (mut a, b) = tokio::io::duplex(64);
        let (_, mut rx) = Connection::new(b).split();
        a.write_all(&[10, 0, 0, 0, 1]).await.unwrap();
        drop(a);
        assert!(matches!(rx.recv::<Msg>().await, Err(IpcError::Io(_))));
        let big = Msg::Text("x".repeat(MAX_MESSAGE + 1));
        assert!(matches!(encode(&big), Err(IpcError::Codec(_))));
    }

    #[tokio::test]
    async fn local_endpoint_serves_this_session_and_is_exclusive() {
        let name = endpoint_name(&format!("test-{}", std::process::id()));
        let mut listener = Listener::bind(&name).unwrap();
        #[cfg(windows)]
        assert!(
            Listener::bind(&name).is_err(),
            "the name cannot be squatted"
        );
        let server = tokio::spawn(async move {
            let (mut tx, mut rx) = listener.accept().await.unwrap().split();
            let got: Msg = rx.recv().await.unwrap().unwrap();
            tx.send(&got).await.unwrap();
            // A second client works too.
            let (mut tx, _) = listener.accept().await.unwrap().split();
            tx.send(&Msg::Ping(2)).await.unwrap();
        });
        let (mut tx, mut rx) = connect(&name).await.unwrap().split();
        tx.send(&Msg::Text("echo".into())).await.unwrap();
        assert_eq!(
            rx.recv::<Msg>().await.unwrap(),
            Some(Msg::Text("echo".into()))
        );
        let (_, mut rx2) = connect(&name).await.unwrap().split();
        assert_eq!(rx2.recv::<Msg>().await.unwrap(), Some(Msg::Ping(2)));
        server.await.unwrap();
        assert!(name.contains("kanodesk-"));
    }

    #[test]
    fn spec_docs_exist() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for doc in SPEC_DOCS {
            assert!(root.join(doc).is_file(), "missing spec doc {doc}");
        }
    }
}
