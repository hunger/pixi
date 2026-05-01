//! Varlink IPC server/client for pixi, built on top of the [`zlink`] crate.
//!
//! This crate defines a small `dev.prefix.pixi.Echo` Varlink interface and
//! exposes both ends of a connection:
//!
//! * [`serve`] runs the server on a Unix domain socket. When started by
//!   systemd with socket activation, it picks up the inherited socket
//!   automatically; otherwise it binds `socket_path`.
//! * [`take_socket_activation_listener`] and [`serve_on`] are the lower-level
//!   primitives that [`serve`] is built from.
//! * [`connect`] returns a [`zlink::Connection`] on which the proxy methods
//!   from [`EchoProxy`] are available.

use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use zlink::{ReplyError, Server, introspect, proxy, service, unix};

/// Reverse-DNS interface name shared between the server and the client.
pub const INTERFACE: &str = "dev.prefix.pixi.Echo";

/// A single ping reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingReply {
    /// The message echoed back from the server.
    pub message: String,
}

/// One progress notification, emitted on a streaming RPC reply alongside the
/// final result.
///
/// `task_id` is opaque and is only required to be unique within a single
/// streaming call. `parent` lets a server build a tree of sub-tasks (e.g. a
/// solve nested inside an install).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressEvent {
    /// Stream-local identifier for the task this event belongs to.
    pub task_id: u64,
    /// Optional parent task, for nested progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    /// What happened.
    pub kind: ProgressKind,
}

/// The kind of [`ProgressEvent`] being reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgressKind {
    /// A new task has begun.
    Started {
        /// Human-readable label.
        label: String,
        /// Total work units, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
    },
    /// The task has advanced; `current` is cumulative work units done.
    Advance {
        /// Cumulative work units completed so far.
        current: u64,
    },
    /// A free-form status update.
    Message {
        /// Status text.
        text: String,
    },
    /// The task is finished.
    Finished,
}

/// One element of [`EchoProxy::long_ping`]'s reply stream.
///
/// Intermediate replies carry [`Progress`](Self::Progress) events; the final
/// reply carries [`Result`](Self::Result) and ends the stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LongPingReply {
    /// A progress notification.
    Progress {
        /// The notification.
        event: ProgressEvent,
    },
    /// The terminating reply, carrying the same payload as [`PingReply`].
    Result {
        /// The echoed message.
        reply: PingReply,
    },
}

/// Errors that the echo service can return to a peer.
#[derive(Debug, Clone, PartialEq, ReplyError, introspect::ReplyError)]
#[zlink(interface = "dev.prefix.pixi.Echo")]
pub enum EchoError {
    /// The caller sent an empty message.
    EmptyMessage,
}

/// Client-side proxy. Implemented for [`zlink::Connection`] by the
/// `#[proxy]` macro, so callers can simply do `conn.ping("hi").await?`.
#[proxy("dev.prefix.pixi.Echo")]
pub trait EchoProxy {
    /// Echoes `message` back to the caller.
    async fn ping(&mut self, message: &str) -> zlink::Result<Result<PingReply, EchoError>>;

    /// Like [`ping`](Self::ping), but streams a [`ProgressEvent`] for each
    /// character of the message before delivering the terminating
    /// [`LongPingReply::Result`]. Demonstrates how a long-running RPC carries
    /// its own progress notifications.
    #[zlink(more)]
    async fn long_ping(
        &mut self,
        message: String,
    ) -> zlink::Result<impl futures::Stream<Item = zlink::Result<Result<LongPingReply, EchoError>>>>;
}

/// The server-side service implementation.
#[derive(Debug, Default)]
pub struct EchoService;

#[service(interface = "dev.prefix.pixi.Echo")]
impl EchoService {
    async fn ping(&mut self, message: String) -> Result<PingReply, EchoError> {
        if message.is_empty() {
            Err(EchoError::EmptyMessage)
        } else {
            Ok(PingReply { message })
        }
    }

    #[zlink(more)]
    async fn long_ping(
        &self,
        more: bool,
        message: String,
    ) -> impl futures::Stream<Item = zlink::Reply<LongPingReply>> + Unpin {
        let total = message.chars().count() as u64;
        let task_id = 1;
        let mut replies: Vec<LongPingReply> = Vec::with_capacity(total as usize + 3);
        replies.push(LongPingReply::Progress {
            event: ProgressEvent {
                task_id,
                parent: None,
                kind: ProgressKind::Started {
                    label: format!("echoing {total} character(s)"),
                    total: Some(total),
                },
            },
        });
        for i in 1..=total {
            replies.push(LongPingReply::Progress {
                event: ProgressEvent {
                    task_id,
                    parent: None,
                    kind: ProgressKind::Advance { current: i },
                },
            });
        }
        replies.push(LongPingReply::Progress {
            event: ProgressEvent {
                task_id,
                parent: None,
                kind: ProgressKind::Finished,
            },
        });
        replies.push(LongPingReply::Result {
            reply: PingReply { message },
        });

        // Honour the varlink `more` flag: if the caller didn't ask for a
        // stream, emit only the terminating result.
        if !more {
            let final_reply = replies.pop().expect("Result is always pushed last");
            replies.clear();
            replies.push(final_reply);
        }

        let n = replies.len();
        futures::stream::iter(
            replies
                .into_iter()
                .enumerate()
                .map(move |(i, v)| zlink::Reply::new(Some(v)).set_continues(Some(i + 1 < n))),
        )
    }
}

/// Serve the echo interface, preferring a systemd-passed socket when
/// available and otherwise binding `socket_path` ourselves.
///
/// When binding, any pre-existing socket file at `socket_path` is removed
/// first so that a stale file from a previous run does not block the bind.
pub async fn serve(socket_path: PathBuf) -> Result<(), Error> {
    let listener = match take_socket_activation_listener()? {
        Some(listener) => listener,
        None => {
            let _ = tokio::fs::remove_file(&socket_path).await;
            unix::bind(&socket_path)?
        }
    };
    serve_on(listener).await
}

/// Serve the echo interface on a pre-built listener.
pub async fn serve_on(listener: unix::Listener) -> Result<(), Error> {
    let server = Server::new(listener, EchoService);
    server.run().await?;
    Ok(())
}

/// Connect to a server previously started with [`serve`].
pub async fn connect(socket_path: &Path) -> Result<unix::Connection, Error> {
    Ok(unix::connect(socket_path).await?)
}

/// First file descriptor passed by systemd via socket activation.
const SD_LISTEN_FDS_START: i32 = 3;

/// Tracks whether we have already converted the inherited FD into a listener.
/// Prevents a double-`from_raw_fd` (which would close FD 3 twice on drop).
static ACTIVATION_LISTENER_TAKEN: AtomicBool = AtomicBool::new(false);

/// Returns the listener inherited from systemd via the
/// [socket activation protocol], if this process was started that way.
///
/// Reads `LISTEN_PID` and `LISTEN_FDS` from the environment; returns
/// `Ok(None)` when no activation is in effect, and an error when the
/// variables are present but inconsistent (wrong PID, non-numeric values,
/// or anything other than exactly one inherited socket).
///
/// Calling this more than once yields the listener at most once: subsequent
/// calls return an error so a stray caller cannot accidentally close the
/// inherited file descriptor.
///
/// [socket activation protocol]: https://www.freedesktop.org/software/systemd/man/sd_listen_fds.html
pub fn take_socket_activation_listener() -> Result<Option<unix::Listener>, Error> {
    let pid = match std::env::var("LISTEN_PID") {
        Ok(v) => v,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(e) => {
            return Err(Error::SocketActivation(format!(
                "LISTEN_PID is not valid UTF-8: {e}"
            )));
        }
    };
    let pid: u32 = pid
        .parse()
        .map_err(|e| Error::SocketActivation(format!("LISTEN_PID is not a number: {e}")))?;
    if pid != std::process::id() {
        return Err(Error::SocketActivation(format!(
            "LISTEN_PID {pid} does not match current PID {}",
            std::process::id()
        )));
    }

    let count: u32 = std::env::var("LISTEN_FDS")
        .map_err(|_| Error::SocketActivation("LISTEN_PID set but LISTEN_FDS missing".into()))?
        .parse()
        .map_err(|e| Error::SocketActivation(format!("LISTEN_FDS is not a number: {e}")))?;

    if count == 0 {
        return Ok(None);
    }
    if count != 1 {
        return Err(Error::SocketActivation(format!(
            "expected exactly one inherited socket, got {count}"
        )));
    }

    if ACTIVATION_LISTENER_TAKEN.swap(true, Ordering::SeqCst) {
        return Err(Error::SocketActivation(
            "socket-activation listener has already been taken".into(),
        ));
    }

    // SAFETY: systemd guarantees that FD `SD_LISTEN_FDS_START` is open and refers to a listening
    // socket whenever `LISTEN_PID` matches our PID and `LISTEN_FDS == 1`. The atomic guard above
    // ensures we wrap that FD in an `OwnedFd` at most once per process.
    let fd = unsafe { OwnedFd::from_raw_fd(SD_LISTEN_FDS_START) };
    Ok(Some(unix::Listener::try_from(fd)?))
}

/// Errors produced by [`serve`], [`connect`], and friends.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying I/O failure while binding or talking to the socket.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Error reported by the zlink runtime.
    #[error(transparent)]
    Zlink(#[from] zlink::Error),

    /// The systemd socket-activation environment was malformed.
    #[error("systemd socket activation: {0}")]
    SocketActivation(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use tokio::task::LocalSet;

    /// Serializes tests that read or write the systemd activation env vars,
    /// since `LISTEN_PID` / `LISTEN_FDS` are process-wide.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// `Server::run` cannot be sent across threads (see its doc comment and
    /// rust-lang/rust#100013), so we drive both ends from a `LocalSet` and
    /// race them with `tokio::select!`.
    // Held across `.await`, but `ENV_LOCK` is only contended with another
    // test that doesn't await, so a `std::sync::Mutex` is fine here.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn round_trip_ping() {
        // serve() reads LISTEN_PID / LISTEN_FDS, so coordinate with the
        // env-mutating test to avoid a flake.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("echo.varlink");

        LocalSet::new()
            .run_until(async move {
                let client_socket = socket.clone();
                let client = async move {
                    // Wait for the socket file to appear before connecting.
                    for _ in 0..50 {
                        if tokio::fs::try_exists(&client_socket).await.unwrap_or(false) {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }

                    let mut conn = connect(&client_socket).await.expect("connect");
                    let reply = conn.ping("hello").await.expect("call").expect("reply");
                    assert_eq!(reply.message, "hello");

                    let err = conn.ping("").await.expect("call").expect_err("error");
                    assert_eq!(err, EchoError::EmptyMessage);

                    // Streaming long_ping: collect all replies and split into
                    // progress events vs. final result.
                    use futures::StreamExt;
                    let stream = conn.long_ping("hi".into()).await.expect("subscribe");
                    let mut stream = std::pin::pin!(stream);
                    let mut events = Vec::new();
                    let mut result = None;
                    while let Some(item) = stream.next().await {
                        match item.expect("transport").expect("ok reply") {
                            LongPingReply::Progress { event } => events.push(event),
                            LongPingReply::Result { reply } => {
                                assert!(result.is_none(), "Result emitted twice");
                                result = Some(reply);
                            }
                        }
                    }
                    let result = result.expect("stream ended without a Result");
                    assert_eq!(result.message, "hi");
                    assert_eq!(events.len(), 4, "Started, 2× Advance, Finished");
                    assert!(matches!(
                        events.first().unwrap().kind,
                        ProgressKind::Started { total: Some(2), .. }
                    ));
                    assert!(matches!(
                        events.last().unwrap().kind,
                        ProgressKind::Finished
                    ));
                };

                tokio::select! {
                    res = serve(socket) => panic!("server exited: {res:?}"),
                    () = client => {}
                }
            })
            .await;
    }

    /// Walks through the env-variable branches of
    /// [`take_socket_activation_listener`] sequentially. We can't split these
    /// into separate `#[test]`s because `LISTEN_PID` / `LISTEN_FDS` are
    /// process-wide and tests run in parallel.
    ///
    /// We deliberately stay in the *error / None* branches so we never reach
    /// the `OwnedFd::from_raw_fd(3)` line — there is no real listener on FD 3
    /// in a test process, and tripping the atomic guard would also affect any
    /// later test in the same binary.
    #[test]
    fn socket_activation_env_branches() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // SAFETY: ENV_LOCK serializes us against any other test in this
        // binary that touches these env vars, and we restore them before
        // returning.
        unsafe {
            std::env::remove_var("LISTEN_PID");
            std::env::remove_var("LISTEN_FDS");
        }

        // Nothing set → no activation.
        assert!(matches!(take_socket_activation_listener(), Ok(None)));

        // LISTEN_PID present but garbage → error.
        unsafe { std::env::set_var("LISTEN_PID", "abc") };
        match take_socket_activation_listener() {
            Err(Error::SocketActivation(msg)) => assert!(msg.contains("not a number")),
            other => panic!("expected non-numeric LISTEN_PID error, got {other:?}"),
        }

        // LISTEN_PID matches us but LISTEN_FDS missing → error.
        let pid = std::process::id().to_string();
        unsafe { std::env::set_var("LISTEN_PID", &pid) };
        match take_socket_activation_listener() {
            Err(Error::SocketActivation(msg)) => assert!(msg.contains("LISTEN_FDS missing")),
            other => panic!("expected missing-LISTEN_FDS error, got {other:?}"),
        }

        // LISTEN_PID set to something that isn't us → error.
        unsafe {
            std::env::set_var("LISTEN_PID", "1");
            std::env::set_var("LISTEN_FDS", "1");
        }
        match take_socket_activation_listener() {
            Err(Error::SocketActivation(msg)) => assert!(msg.contains("does not match")),
            other => panic!("expected PID mismatch, got {other:?}"),
        }

        // Right PID, LISTEN_FDS=0 → not really activated, return None.
        unsafe {
            std::env::set_var("LISTEN_PID", &pid);
            std::env::set_var("LISTEN_FDS", "0");
        }
        assert!(matches!(take_socket_activation_listener(), Ok(None)));

        // Right PID, multiple FDs → we only support one.
        unsafe { std::env::set_var("LISTEN_FDS", "2") };
        match take_socket_activation_listener() {
            Err(Error::SocketActivation(msg)) => assert!(msg.contains("exactly one")),
            other => panic!("expected too-many-fds error, got {other:?}"),
        }

        // Right PID, garbage LISTEN_FDS → error.
        unsafe { std::env::set_var("LISTEN_FDS", "not-a-number") };
        assert!(matches!(
            take_socket_activation_listener(),
            Err(Error::SocketActivation(_))
        ));

        unsafe {
            std::env::remove_var("LISTEN_PID");
            std::env::remove_var("LISTEN_FDS");
        }
    }
}
