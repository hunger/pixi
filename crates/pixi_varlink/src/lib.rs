//! Varlink IPC server/client for pixi, built on top of the [`zlink`] crate.
//!
//! This crate defines a `dev.prefix.pixi.Echo` Varlink interface and
//! exposes both ends of a connection:
//!
//! * [`serve`] runs the server on a Unix domain socket. When started by
//!   systemd with socket activation, it picks up the inherited socket
//!   automatically; otherwise it binds `socket_path`. Echo-only mode.
//! * [`serve_with_install`] adds the `Install` RPC on top of the same
//!   interface, taking exclusive `flock`s on the configured `data` and
//!   `cache` roots so a second daemon against the same directories
//!   refuses to start.
//! * [`take_socket_activation_listener`] and [`serve_on`] are the lower-level
//!   primitives that [`serve`] is built from.
//! * [`connect`] returns an authenticated [`Connection`] by performing the
//!   `Hello` / `Authenticate` handshake against a directory the caller can
//!   write to. The pre-handshake side is represented by an internal
//!   `UnauthorizedConnection` type; together they form a typestate that
//!   prevents business methods from being called on an unauthenticated
//!   connection at compile time.

mod install;
mod reporter_wire;
#[cfg(unix)]
mod wire_reporter;

pub use install::{
    InstallChangeWire, InstallFailure, InstallReply, InstallRequest, PackageChange, SALT_LEN,
    ServerConfig, ServerConfigError, TransactionSummary, env_hash, validate_env_name,
};
pub use reporter_wire::{
    CondaSolveEnvWire, InstallEnvWire, LoggingReporterClient, PixiSolveEnvWire, ReporterCall,
    ReporterClient, TransactionOpWire,
};

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_fd_lock::LockWrite;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use futures::StreamExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, trace, warn};
use uuid::Uuid;
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

/// One element of [`Connection::long_ping`]'s reply stream.
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

/// Server reply to the `Hello` handshake step. The client must write a file
/// `<directory>/pixi-serve-challenge-<challenge>` containing the exact
/// `challenge` string back to disk and then call `Authenticate` to finish
/// the handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloReply {
    /// The challenge token (a UUID). Must round-trip through the filesystem.
    pub challenge: String,
}

/// Server reply to the `Authenticate` handshake step. Empty today; reserved
/// for future fields like a session id or capability set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthReply {}

/// Errors that the echo service can return to a peer.
#[derive(Debug, Clone, PartialEq, ReplyError, introspect::ReplyError)]
#[zlink(interface = "dev.prefix.pixi.Echo")]
pub enum EchoError {
    /// The caller sent an empty message.
    EmptyMessage,
    /// The connection has not yet completed the `Hello` / `Authenticate`
    /// handshake, so no business methods may be called.
    NotAuthenticated,
    /// `Hello` was called more than once on the same connection. A
    /// connection is bound to one directory for its lifetime.
    AlreadyHandshaken,
    /// `Authenticate` was called outside the awaiting-proof state.
    NotAwaitingProof,
    /// The challenge file could not be read or its content didn't match the
    /// issued UUID.
    ChallengeFailed {
        /// Human-readable reason: missing file, wrong content, I/O error.
        reason: String,
    },
    /// The directory sent in `Hello` could not be opened, or the kernel's
    /// canonical path for the resulting FD didn't match what the client
    /// sent — meaning either the path was non-canonical (relative,
    /// `.`/`..` components, trailing slash, etc.) or some component
    /// resolved through a symlink. Send the absolute, symlink-resolved
    /// path the kernel reports back.
    NonCanonicalDirectory {
        /// Human-readable reason: either the path the FD actually points
        /// at vs. what the client sent, or the I/O error from the open.
        reason: String,
    },
}

/// Pre-handshake proxy methods. Private to this crate: external callers
/// should drive the handshake through [`UnauthorizedConnection::hello`] and
/// [`AwaitingProof::authenticate`], not by invoking these directly.
#[proxy("dev.prefix.pixi.Echo")]
trait UnauthProxy {
    async fn hello(&mut self, directory: &str) -> zlink::Result<Result<HelloReply, EchoError>>;

    async fn authenticate(&mut self) -> zlink::Result<Result<AuthReply, EchoError>>;
}

/// Post-handshake proxy methods. Private to this crate: external callers
/// reach these through inherent methods on [`Connection`] so the
/// authenticated typestate can't be bypassed.
#[proxy("dev.prefix.pixi.Echo")]
trait AuthProxy {
    async fn ping(&mut self, message: &str) -> zlink::Result<Result<PingReply, EchoError>>;

    #[zlink(more)]
    async fn long_ping(
        &mut self,
        message: String,
    ) -> zlink::Result<impl futures::Stream<Item = zlink::Result<Result<LongPingReply, EchoError>>>>;

    #[zlink(more)]
    async fn install(
        &mut self,
        request: InstallRequest,
    ) -> zlink::Result<impl futures::Stream<Item = zlink::Result<Result<InstallReply, EchoError>>>>;
}

/// Per-connection auth state, tracked by zlink's connection id.
#[derive(Debug, Clone)]
enum AuthState {
    /// `Hello` has been called and we're awaiting `Authenticate`. The
    /// `dir` handle is opened against the canonical directory at `Hello`
    /// time and used to read the challenge file later — anchoring the
    /// read to the directory's inode means a path-component symlink swap
    /// between `Hello` and `Authenticate` can't redirect us elsewhere.
    AwaitingProof {
        directory: PathBuf,
        challenge: Uuid,
        dir: Arc<Dir>,
    },
    /// `Authenticate` succeeded; business methods are allowed against
    /// `directory`.
    Authenticated { directory: PathBuf },
}

/// The server-side service implementation.
///
/// Holds a per-connection auth-state map keyed by zlink's connection id,
/// plus an optional [`ServerConfig`] that enables the [`Install`](EchoService::install)
/// RPC. Without a config the install method returns
/// [`InstallFailure::ServerNotConfigured`] so the wire schema is the
/// same in either mode.
///
/// **Lifecycle / memory note.** zlink doesn't currently surface a
/// "connection closed" hook to `Service` implementations, so entries in
/// `auth` are never removed: they accumulate for the daemon's lifetime
/// and a restart is the only thing that frees them. This is *only* a
/// bookkeeping concern, not an authentication one — zlink's connection
/// ids are issued via a process-global `AtomicUsize::fetch_add(1)`
/// (`zlink-core/src/connection/mod.rs`), so they're strictly monotonic
/// and never reused. A new connection is guaranteed to see an empty slot
/// in `auth` regardless of how many old, since-closed connections came
/// before it.
///
/// For typical pixi-serve usage (single user, occasional connections,
/// the daemon gets restarted on package upgrade) the leak is well below
/// any threshold worth coding around. If a long-running daemon ever
/// needs cleanup, the right fix is upstream — patch zlink to add an
/// `on_disconnect(conn_id)` hook to `Service`.
#[derive(Debug, Clone, Default)]
pub struct EchoService {
    auth: Arc<Mutex<HashMap<usize, AuthState>>>,
    install_config: Option<Arc<ServerConfig>>,
}

impl EchoService {
    /// Build a service that exposes only the echo interface; the
    /// `Install` RPC will reject every call with
    /// [`InstallFailure::ServerNotConfigured`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a service that handles `Install` against `config` in
    /// addition to echo.
    pub fn with_install_config(config: ServerConfig) -> Self {
        Self {
            auth: Arc::default(),
            install_config: Some(Arc::new(config)),
        }
    }
}

/// Ask the kernel for the canonical path the directory FD currently lives at.
///
/// On Linux this reads the `/proc/self/fd/<n>` magic symlink; on every other
/// Unix we go through `fcntl(F_GETPATH)` (which both macOS and FreeBSD
/// expose). The result is the kernel's authoritative view of where the FD
/// points, so comparing it to what the client sent in `Hello` rejects every
/// shape of symlink resolution that could have happened during the open —
/// final-component, intermediate-component, or even between the client's
/// rename and our open. We're already `cfg(unix)` from the surrounding
/// crate, so no compile-time fallback is needed.
fn fd_canonical_path<F: AsFd>(fd: &F) -> std::io::Result<PathBuf> {
    /// Refuse paths longer than this; bounds both Linux's grow-loop and the
    /// memory the function will allocate. Real filesystems with paths past
    /// PATH_MAX are exotic; honoring them past 16 × PATH_MAX would be a DoS
    /// surface.
    const MAX_PATH_BYTES: usize = libc::PATH_MAX as usize * 16;

    let raw = fd.as_fd().as_raw_fd();

    #[cfg(target_os = "linux")]
    let buf = {
        let proc_path = format!("/proc/self/fd/{raw}\0");
        // `readlink(2)` writes up to `bufsiz` bytes and returns that count;
        // there's no separate "would have written more" signal, so we
        // grow the buffer until the result is strictly shorter than the
        // buffer (i.e. definitively not truncated). Cap the growth so a
        // pathologically deep mount can't make us allocate forever.
        let mut size = libc::PATH_MAX as usize;
        loop {
            let mut buf = vec![0u8; size];
            // SAFETY: `proc_path` is NUL-terminated and points to a valid C
            // string for the duration of the call; `buf` is writable for
            // `buf.len()` bytes.
            let n = unsafe {
                libc::readlink(
                    proc_path.as_ptr() as *const libc::c_char,
                    buf.as_mut_ptr() as *mut libc::c_char,
                    buf.len(),
                )
            };
            if n == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let n = n as usize;
            if n < buf.len() {
                buf.truncate(n);
                break buf;
            }
            // Filled the buffer — content may have been truncated.
            if size >= MAX_PATH_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("FD canonical path exceeds {MAX_PATH_BYTES} bytes"),
                ));
            }
            size = size.saturating_mul(2).min(MAX_PATH_BYTES);
        }
    };

    #[cfg(not(target_os = "linux"))]
    let buf = {
        // `F_GETPATH` requires a buffer of at least `MAXPATHLEN` (== PATH_MAX
        // on macOS / *BSD) and returns `ENAMETOOLONG` if the path doesn't
        // fit. So a single PATH_MAX-sized call is sufficient: either we get
        // the path or `last_os_error` surfaces the kernel's error.
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: `buf` is writable for at least `PATH_MAX` bytes, which is
        // what `F_GETPATH` writes into.
        let r = unsafe {
            libc::fcntl(
                raw,
                libc::F_GETPATH,
                buf.as_mut_ptr().cast::<libc::c_void>(),
            )
        };
        if r == -1 {
            return Err(std::io::Error::last_os_error());
        }
        // `F_GETPATH` writes a NUL-terminated string; trim at the first NUL.
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(len);
        buf
    };

    Ok(PathBuf::from(OsString::from_vec(buf)))
}

/// Build the single terminal reply emitted by the streaming `Install`
/// method.
///
/// All checks live here (auth, env-name shape, server config) so the
/// streaming method body stays a thin shell. Failures land as
/// [`InstallReply::Failed`] inside the stream rather than as a varlink
/// `ReplyError`, because zlink can't currently carry typed errors out
/// of a `#[zlink(more)]` method.
async fn compute_install_reply(
    service: &EchoService,
    conn_id: usize,
    request: InstallRequest,
) -> InstallReply {
    let (directory, cfg) = match preflight_install(service, conn_id) {
        Ok(x) => x,
        Err(error) => return InstallReply::Failed { error },
    };
    if let Err(error) = validate_env_name(&request.env_name) {
        return InstallReply::Failed { error };
    }
    match install::run_install(&cfg, &directory, &request, None).await {
        Ok((prefix, transaction)) => InstallReply::Success {
            prefix: prefix.display().to_string(),
            transaction,
        },
        Err(error) => InstallReply::Failed { error },
    }
}

/// User-facing hint emitted by [`InstallFailure::ServerNotConfigured`].
/// Spelled out once so streaming and non-streaming install paths
/// agree on the wording.
const SERVE_NOT_CONFIGURED_HINT: &str = "start `pixi serve` with `--data <PATH>` and `--cache <PATH>` (or the matching `[serve]` config keys)";

/// Auth + install-config gate shared by the streaming and
/// non-streaming `install` paths. Returns the authenticated client
/// directory and the configured [`install::ServerConfig`] on
/// success; the caller turns the [`InstallFailure`] into whichever
/// terminal reply shape it needs.
fn preflight_install(
    service: &EchoService,
    conn_id: usize,
) -> Result<(PathBuf, Arc<install::ServerConfig>), install::InstallFailure> {
    let directory = service
        .require_authenticated(conn_id)
        .map_err(|_| install::InstallFailure::NotAuthenticated)?;
    let cfg = service.install_config.clone().ok_or_else(|| {
        install::InstallFailure::ServerNotConfigured {
            hint: SERVE_NOT_CONFIGURED_HINT.to_string(),
        }
    })?;
    Ok((directory, cfg))
}

impl EchoService {
    /// Returns the directory the connection is authenticated against, or
    /// [`EchoError::NotAuthenticated`] if it isn't. Logs the rejection so
    /// servers can spot misbehaving clients.
    fn require_authenticated(&self, conn_id: usize) -> Result<PathBuf, EchoError> {
        match self.auth.lock().get(&conn_id) {
            Some(AuthState::Authenticated { directory }) => Ok(directory.clone()),
            Some(AuthState::AwaitingProof { .. }) => {
                debug!(conn_id, "rejecting request: handshake not yet completed");
                Err(EchoError::NotAuthenticated)
            }
            None => {
                debug!(conn_id, "rejecting request: connection has not sent Hello");
                Err(EchoError::NotAuthenticated)
            }
        }
    }
}

#[service(interface = "dev.prefix.pixi.Echo")]
impl<Sock> EchoService
where
    Sock: zlink::connection::Socket,
{
    /// Step 1 of the handshake. Records the directory the client wants and
    /// issues a UUID challenge. Refuses if the connection has already sent
    /// `Hello`.
    #[instrument(level = "debug", skip(self, conn), fields(conn_id = conn.id(), directory))]
    async fn hello(
        &self,
        directory: String,
        #[zlink(connection)] conn: &mut zlink::Connection<Sock>,
    ) -> Result<HelloReply, EchoError> {
        let conn_id = conn.id();
        // Reject duplicate Hello early so we don't spend a `canonicalize`
        // syscall on it.
        if self.auth.lock().contains_key(&conn_id) {
            return Err(EchoError::AlreadyHandshaken);
        }

        let directory = PathBuf::from(directory);

        // Open the directory and immediately ask the kernel for the FD's
        // canonical path. Comparing that to what the client sent rejects
        // every shape of symlink resolution that could have happened
        // during the open — including symlinks above `directory` — in a
        // single check, with no canonicalize→open race window.
        let dir = Dir::open_ambient_dir(&directory, ambient_authority()).map_err(|e| {
            debug!(directory = %directory.display(), error = %e, "rejecting Hello: open failed");
            EchoError::NonCanonicalDirectory {
                reason: format!("could not open {}: {e}", directory.display()),
            }
        })?;
        let fd_path = fd_canonical_path(&dir).map_err(|e| EchoError::NonCanonicalDirectory {
            reason: format!(
                "could not query kernel path for {}: {e}",
                directory.display()
            ),
        })?;
        // Compare as `OsStr`, not `Path`: `Path::eq` normalises `.`
        // components away (so `/foo/.` would compare equal to `/foo`),
        // which silently accepts non-canonical input. Byte-level OsStr
        // equality matches the stated contract.
        if fd_path.as_os_str() != directory.as_os_str() {
            debug!(
                directory = %directory.display(),
                fd_path = %fd_path.display(),
                "rejecting Hello: kernel reports a different path for the FD"
            );
            return Err(EchoError::NonCanonicalDirectory {
                reason: format!(
                    "directory FD resolves to {}, but client sent {}",
                    fd_path.display(),
                    directory.display()
                ),
            });
        }

        let challenge = Uuid::new_v4();
        self.auth.lock().insert(
            conn_id,
            AuthState::AwaitingProof {
                directory: fd_path.clone(),
                challenge,
                dir: Arc::new(dir),
            },
        );
        // The challenge is the bearer secret of the handshake — keep it
        // out of `info`/`warn` events so it doesn't leak into log files.
        debug!(%challenge, "issued challenge");
        info!(directory = %fd_path.display(), "issued auth challenge");
        Ok(HelloReply {
            challenge: challenge.to_string(),
        })
    }

    /// Step 2 of the handshake. Reads
    /// `<directory>/pixi-serve-challenge-<UUID>` and, if its contents match
    /// the UUID issued by `Hello`, marks the connection authenticated.
    #[instrument(level = "debug", skip(self, conn), fields(conn_id = conn.id()))]
    async fn authenticate(
        &self,
        #[zlink(connection)] conn: &mut zlink::Connection<Sock>,
    ) -> Result<AuthReply, EchoError> {
        let conn_id = conn.id();
        // Snapshot the awaiting-proof state, including the cap-std `Dir`
        // handle that anchors the upcoming open to the directory's inode.
        let (directory, challenge, dir) = match self.auth.lock().get(&conn_id) {
            Some(AuthState::AwaitingProof {
                directory,
                challenge,
                dir,
            }) => (directory.clone(), *challenge, Arc::clone(dir)),
            _ => return Err(EchoError::NotAwaitingProof),
        };

        // Read the challenge file via the captured `Dir`:
        //
        //   * The open is `openat(dir_fd, …)` — directory-component symlink
        //     swaps between `Hello` and now can't redirect the read because
        //     the FD already names the inode.
        //   * `O_NOFOLLOW` (via `custom_flags`) refuses a symlink at the
        //     final component itself.
        //
        // Together these close the symlink-attack window the previous
        // canonicalize-and-compare approximated.
        let filename = format!("pixi-serve-challenge-{challenge}");
        let mut opts = OpenOptions::new();
        opts.read(true).custom_flags(libc::O_NOFOLLOW);
        let file = dir
            .open_with(&filename, &opts)
            .map_err(|e| EchoError::ChallengeFailed {
                reason: format!("could not open {filename} in {}: {e}", directory.display()),
            })?;
        // Bound the read: a legitimate challenge file holds a UUID (~36
        // bytes plus optional whitespace). Anyone with write access to
        // the directory could otherwise plant a multi-GB file at this
        // name and force the server to allocate it on every Authenticate.
        const MAX_CHALLENGE_FILE_BYTES: u64 = 256;
        let mut contents = String::with_capacity(MAX_CHALLENGE_FILE_BYTES as usize);
        file.take(MAX_CHALLENGE_FILE_BYTES)
            .read_to_string(&mut contents)
            .map_err(|e| EchoError::ChallengeFailed {
                reason: format!("could not read {filename} in {}: {e}", directory.display()),
            })?;
        if contents.trim() != challenge.to_string() {
            return Err(EchoError::ChallengeFailed {
                reason: format!(
                    "challenge file {filename} in {} has wrong content",
                    directory.display()
                ),
            });
        }

        self.auth.lock().insert(
            conn_id,
            AuthState::Authenticated {
                directory: directory.clone(),
            },
        );
        info!(directory = %directory.display(), "connection authenticated");
        Ok(AuthReply {})
    }

    #[instrument(level = "debug", skip(self, conn), fields(conn_id = conn.id(), len = message.len()))]
    async fn ping(
        &self,
        message: String,
        #[zlink(connection)] conn: &mut zlink::Connection<Sock>,
    ) -> Result<PingReply, EchoError> {
        self.require_authenticated(conn.id())?;
        if message.is_empty() {
            debug!("rejecting empty Ping message");
            Err(EchoError::EmptyMessage)
        } else {
            Ok(PingReply { message })
        }
    }

    /// Solve the requested specs and lay down the resulting binary
    /// records under `<data>/<HASH>/`. With `more=true` (the daemon
    /// path always sets this), every reporter callback the dispatcher
    /// and rattler subsystems make during the install is forwarded as
    /// an [`InstallReply::Progress`] event ahead of the terminal
    /// [`InstallReply::Success`] / [`InstallReply::Failed`]. With
    /// `more=false` (a non-streaming caller, e.g. `serve-test
    /// install`) the body emits only the terminal reply.
    #[zlink(more)]
    #[instrument(level = "debug", skip(self, conn), fields(conn_id = conn.id(), env = %request.env_name, more))]
    async fn install(
        &self,
        more: bool,
        request: InstallRequest,
        #[zlink(connection)] conn: &mut zlink::Connection<Sock>,
    ) -> impl futures::Stream<Item = zlink::Reply<InstallReply>> + Unpin {
        if !more {
            // Non-streaming caller: keep the original "single terminal
            // reply, no reporter" path. Cheap, used by `serve-test`.
            let reply = compute_install_reply(self, conn.id(), request).await;
            let item = zlink::Reply::new(Some(reply)).set_continues(Some(false));
            return futures::stream::iter(vec![item]).boxed();
        }

        // Streaming path. Auth/config preflight stays synchronous so a
        // bad request fails fast with a single terminal reply, no
        // spawned task.
        let preflight = preflight_install(self, conn.id()).and_then(|(dir, cfg)| {
            install::validate_env_name(&request.env_name).map(|_| (dir, cfg))
        });
        let (directory, cfg) = match preflight {
            Ok(x) => x,
            Err(error) => {
                let reply = InstallReply::Failed { error };
                let item = zlink::Reply::new(Some(reply)).set_continues(Some(false));
                return futures::stream::iter(vec![item]).boxed();
            }
        };

        // Channel: WireReporter pushes events from the dispatcher's
        // many reporter callbacks; the stream below forwards them
        // verbatim. Unbounded because dropping events would silently
        // misrepresent install state, and the dispatcher's reporter
        // calls are non-async and cheap.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ReporterCall>();
        let request_for_task = request.clone();
        let install_task = tokio::spawn(async move {
            let result = install::run_install(&cfg, &directory, &request_for_task, Some(tx)).await;
            match result {
                Ok((prefix, transaction)) => InstallReply::Success {
                    prefix: prefix.display().to_string(),
                    transaction,
                },
                Err(error) => InstallReply::Failed { error },
            }
        });

        // Pump reporter calls while the install task runs; once `tx`
        // drops the call stream ends, then chain a single terminal
        // item resolved from the join handle.
        let call_stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(|call| {
            zlink::Reply::new(Some(InstallReply::ReporterCall { call })).set_continues(Some(true))
        });
        let terminal = futures::stream::once(async move {
            let reply = match install_task.await {
                Ok(r) => r,
                Err(e) => InstallReply::Failed {
                    error: install::InstallFailure::InstallFailed {
                        reason: format!("install task panicked or was cancelled: {e}"),
                    },
                },
            };
            zlink::Reply::new(Some(reply)).set_continues(Some(false))
        });
        call_stream.chain(terminal).boxed()
    }

    #[zlink(more)]
    #[instrument(level = "debug", skip(self, conn), fields(conn_id = conn.id(), len = message.len(), more))]
    async fn long_ping(
        &self,
        more: bool,
        message: String,
        #[zlink(connection)] conn: &mut zlink::Connection<Sock>,
    ) -> impl futures::Stream<Item = zlink::Reply<LongPingReply>> + Unpin {
        // Streaming methods can't carry per-method errors through zlink
        // today. If the caller isn't authenticated, log loudly and emit an
        // empty stream so they can't extract any work.
        if self.require_authenticated(conn.id()).is_err() {
            warn!("rejecting unauthenticated long_ping");
            return futures::stream::iter(Vec::new());
        }

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
        trace!(replies = n, "long_ping stream prepared");
        let items: Vec<zlink::Reply<LongPingReply>> = replies
            .into_iter()
            .enumerate()
            .map(move |(i, v)| zlink::Reply::new(Some(v)).set_continues(Some(i + 1 < n)))
            .collect();
        futures::stream::iter(items)
    }
}

/// Serve the echo interface, preferring a systemd-passed socket when
/// available and otherwise binding `socket_path` ourselves.
///
/// When binding, any pre-existing socket file at `socket_path` is removed
/// first so that a stale file from a previous run does not block the bind.
#[instrument(level = "info", skip_all, fields(path = %socket_path.display()))]
pub async fn serve(socket_path: PathBuf) -> Result<(), Error> {
    let listener = take_or_bind_listener(&socket_path).await?;
    serve_on(listener).await
}

/// Serve the echo interface plus the `Install` RPC against `config`.
///
/// Takes an exclusive [`flock`](async_fd_lock) on
/// `<data>/.pixi-serve.lock` and `<cache>/.pixi-serve.lock` before
/// starting the listener. If either lock is already held — by another
/// `pixi serve` instance or any external locker — the call returns
/// [`Error::ServerLockHeld`] naming the contested lock's full absolute
/// path. The locks are released automatically when the returned future
/// is dropped.
#[instrument(level = "info", skip_all, fields(path = %socket_path.display()))]
pub async fn serve_with_install(socket_path: PathBuf, config: ServerConfig) -> Result<(), Error> {
    // Acquire the locks first so a misconfigured second daemon fails
    // before it ever touches the listener.
    let _data_lock = take_serve_lock(&config.data).await?;
    let _cache_lock = take_serve_lock(&config.cache).await?;
    let listener = take_or_bind_listener(&socket_path).await?;
    serve_on_with_service(listener, EchoService::with_install_config(config)).await
}

/// Serve the echo interface on a pre-built listener.
#[instrument(level = "info", skip_all, fields(interface = INTERFACE))]
pub async fn serve_on(listener: unix::Listener) -> Result<(), Error> {
    serve_on_with_service(listener, EchoService::default()).await
}

async fn serve_on_with_service(
    listener: unix::Listener,
    service: EchoService,
) -> Result<(), Error> {
    info!("starting varlink server");
    let server = Server::new(listener, service);
    let result = server.run().await;
    match &result {
        Ok(()) => info!("varlink server stopped"),
        Err(error) => debug!(?error, "varlink server stopped with error"),
    }
    Ok(result?)
}

async fn take_or_bind_listener(socket_path: &Path) -> Result<unix::Listener, Error> {
    match take_socket_activation_listener()? {
        Some(listener) => Ok(listener),
        None => {
            let _ = tokio::fs::remove_file(socket_path).await;
            info!(path = %socket_path.display(), "binding unix socket");
            Ok(unix::bind(socket_path)?)
        }
    }
}

/// Lock file name placed at the root of `data` and `cache` to block a
/// second `pixi serve` against the same directories.
const SERVE_LOCK_FILE: &str = ".pixi-serve.lock";

/// Take an exclusive `flock` on `<dir>/.pixi-serve.lock`. The returned
/// guard owns the file handle; dropping it releases the lock, so
/// callers must keep it alive for the duration they want the lock.
///
/// `dir` is canonicalised first so the error message names the
/// absolute path the kernel would actually lock — diagnostics surface
/// the truth, not whatever relative path the operator typed.
async fn take_serve_lock(
    dir: &Path,
) -> Result<async_fd_lock::RwLockWriteGuard<tokio::fs::File>, Error> {
    let dir = tokio::fs::canonicalize(dir).await.map_err(|e| {
        Error::ServerLockHeld(format!(
            "could not canonicalise lock root {}: {e}",
            dir.display()
        ))
    })?;
    let lock_path = dir.join(SERVE_LOCK_FILE);
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .await
        .map_err(|e| {
            Error::ServerLockHeld(format!("could not open {}: {e}", lock_path.display()))
        })?;
    match file.try_lock_write().await {
        Ok(guard) => Ok(guard),
        Err(err) => {
            warn!(path = %lock_path.display(), error = ?err.error, "serve lock contended");
            Err(Error::ServerLockHeld(format!(
                "another pixi serve is holding {}; refusing to start",
                lock_path.display()
            )))
        }
    }
}

/// Initial handshake state. The only operation reachable on this type is
/// [`hello`](Self::hello), which advances to [`AwaitingProof`].
pub(crate) struct UnauthorizedConnection {
    inner: unix::Connection,
}

impl UnauthorizedConnection {
    /// Send `Hello { directory }`. The server records the directory and
    /// returns a UUID challenge that the client must subsequently echo
    /// through the filesystem (handled by [`AwaitingProof::authenticate`]).
    #[instrument(level = "info", skip(self), fields(directory = %directory.display()))]
    pub(crate) async fn hello(mut self, directory: &Path) -> Result<AwaitingProof, Error> {
        let dir_str = directory
            .to_str()
            .ok_or_else(|| Error::Handshake("directory path is not valid UTF-8".into()))?;

        debug!("sending Hello");
        let hello = self
            .inner
            .hello(dir_str)
            .await?
            .map_err(|e| Error::Handshake(format!("Hello rejected: {e:?}")))?;
        // Same secret-handling rule as the server side: keep the
        // challenge out of `info` so it doesn't end up in log files.
        debug!(challenge = %hello.challenge, "received challenge");
        info!("received challenge");
        Ok(AwaitingProof {
            inner: self.inner,
            directory: directory.to_path_buf(),
            challenge: hello.challenge,
        })
    }
}

/// Post-`Hello`, pre-`Authenticate` state. Holds the directory the client
/// claimed and the challenge UUID the server issued. The only operation
/// reachable on this type is [`authenticate`](Self::authenticate), which
/// writes the challenge file, asks the server to verify it, removes the
/// file, and advances to [`Connection`].
pub(crate) struct AwaitingProof {
    inner: unix::Connection,
    directory: PathBuf,
    challenge: String,
}

impl AwaitingProof {
    /// The directory this connection is awaiting proof against.
    // Used by in-crate tests; production code reaches the directory via
    // [`Connection::directory`] after `authenticate` completes.
    #[allow(dead_code)]
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    /// The path the server expects the challenge file at.
    // Used by in-crate tests; production code never has to compute this — the
    // file is written and removed inside `authenticate`.
    #[allow(dead_code)]
    pub(crate) fn challenge_path(&self) -> PathBuf {
        self.directory
            .join(format!("pixi-serve-challenge-{}", self.challenge))
    }

    /// Finish the handshake: write the challenge file, send `Authenticate`,
    /// remove the file (best effort), and yield an authenticated
    /// [`Connection`] on success.
    // Span is at info, but the challenge is intentionally omitted from
    // its fields — that span is rendered into log lines for every event
    // inside, so including the secret would leak it to the log subscriber.
    #[instrument(level = "info", skip(self), fields(directory = %self.directory.display()))]
    pub(crate) async fn authenticate(mut self) -> Result<Connection, Error> {
        let challenge_path = self.challenge_path();

        debug!(path = %challenge_path.display(), "writing challenge file");
        tokio::fs::write(&challenge_path, &self.challenge)
            .await
            .map_err(|e| Error::Handshake(format!("writing {}: {e}", challenge_path.display())))?;

        debug!("sending Authenticate");
        let auth_result = self.inner.authenticate().await;
        // Best-effort cleanup of the challenge file regardless of outcome —
        // the server has already read it, and leaving it on disk would be
        // visible to anyone watching the directory.
        if let Err(e) = tokio::fs::remove_file(&challenge_path).await {
            debug!(path = %challenge_path.display(), error = %e, "challenge file removal failed");
        } else {
            debug!(path = %challenge_path.display(), "challenge file removed");
        }

        match auth_result? {
            Ok(_) => {
                info!("handshake complete");
                Ok(Connection {
                    inner: self.inner,
                    directory: self.directory,
                })
            }
            Err(e) => Err(Error::Handshake(format!("Authenticate rejected: {e:?}"))),
        }
    }
}

/// Authenticated varlink connection bound to a specific directory.
///
/// All exposed methods are post-handshake; the type system guarantees that
/// callers cannot send business RPCs without having proved access to the
/// directory recorded here.
pub struct Connection {
    inner: unix::Connection,
    directory: PathBuf,
}

impl Connection {
    /// The directory this connection is authenticated for.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Echo `message` back from the server.
    #[instrument(level = "debug", skip(self), fields(directory = %self.directory.display(), len = message.len()))]
    pub async fn ping(&mut self, message: &str) -> zlink::Result<Result<PingReply, EchoError>> {
        trace!("calling Ping");
        AuthProxy::ping(&mut self.inner, message).await
    }

    /// Stream a `LongPingReply` per character, finishing with the final reply.
    #[instrument(level = "debug", skip(self), fields(directory = %self.directory.display(), len = message.len()))]
    pub async fn long_ping(
        &mut self,
        message: String,
    ) -> zlink::Result<impl futures::Stream<Item = zlink::Result<Result<LongPingReply, EchoError>>>>
    {
        trace!("calling LongPing");
        AuthProxy::long_ping(&mut self.inner, message).await
    }

    /// Submit an `Install` request and stream replies. The stream
    /// currently contains exactly one terminal
    /// [`InstallReply::Success`] or [`InstallReply::Failed`]; step 7
    /// will interleave [`InstallReply::Progress`] events ahead of the
    /// terminal reply.
    #[instrument(level = "debug", skip(self), fields(directory = %self.directory.display(), env = %request.env_name))]
    pub async fn install(
        &mut self,
        request: InstallRequest,
    ) -> zlink::Result<impl futures::Stream<Item = zlink::Result<Result<InstallReply, EchoError>>>>
    {
        trace!("calling Install");
        AuthProxy::install(&mut self.inner, request).await
    }
}

/// Connect to a `pixi serve` socket and complete the `Hello` / `Authenticate`
/// handshake against `directory`. Returns a [`Connection`] ready to send
/// business RPCs.
///
/// `directory` is canonicalised before being sent — the server enforces a
/// canonical path, so this saves clients from tripping that check on
/// relative paths or symlinked roots like macOS's `/tmp`.
///
/// Walks the typestate explicitly so the protocol's stages remain visible
/// in the implementation.
#[instrument(level = "debug", skip_all, fields(socket = %socket_path.display(), directory = %directory.display()))]
pub async fn connect(socket_path: &Path, directory: &Path) -> Result<Connection, Error> {
    let canonical = tokio::fs::canonicalize(directory).await.map_err(|e| {
        Error::Handshake(format!(
            "could not canonicalize {}: {e}",
            directory.display()
        ))
    })?;
    let unauth = connect_unauthenticated(socket_path).await?;
    let awaiting = unauth.hello(&canonical).await?;
    awaiting.authenticate().await
}

/// Open the underlying socket without performing the handshake. Crate-private
/// so tests in this module can exercise the server's pre-handshake state
/// machine; production callers must go through [`connect`].
#[instrument(level = "debug", skip_all, fields(path = %socket_path.display()))]
pub(crate) async fn connect_unauthenticated(
    socket_path: &Path,
) -> Result<UnauthorizedConnection, Error> {
    debug!("connecting to varlink socket");
    Ok(UnauthorizedConnection {
        inner: unix::connect(socket_path).await?,
    })
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
#[instrument(level = "debug")]
pub fn take_socket_activation_listener() -> Result<Option<unix::Listener>, Error> {
    let pid = match std::env::var("LISTEN_PID") {
        Ok(v) => v,
        Err(std::env::VarError::NotPresent) => {
            debug!("LISTEN_PID not set, no socket activation");
            return Ok(None);
        }
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
        debug!("LISTEN_FDS=0; no socket activation");
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

    info!(
        fd = SD_LISTEN_FDS_START,
        "adopting systemd-activated socket"
    );

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

    /// The Hello / Authenticate handshake could not be completed.
    #[error("varlink handshake: {0}")]
    Handshake(String),

    /// The exclusive `flock` on `<data>/.pixi-serve.lock` or
    /// `<cache>/.pixi-serve.lock` is held by another process. The
    /// message names the contested path.
    #[error("{0}")]
    ServerLockHeld(String),
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

    /// Drive a single test client against a fresh server. Spins up `serve()`
    /// in a `LocalSet` (`Server::run` is `!Send`, see its doc comment and
    /// rust-lang/rust#100013), waits for the socket to appear, then runs
    /// `client_logic(socket_path)` with the server racing it via
    /// `tokio::select!`. `ENV_LOCK` is held for the duration so server-side
    /// `LISTEN_*` reads don't collide with the env-mutating test.
    #[allow(clippy::await_holding_lock)]
    async fn run_against_server<F, Fut, T>(client_logic: F) -> T
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("echo.varlink");
        let client_path = socket.clone();

        let result = LocalSet::new()
            .run_until(async move {
                let client = async move {
                    for _ in 0..50 {
                        if tokio::fs::try_exists(&client_path).await.unwrap_or(false) {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    client_logic(client_path).await
                };
                tokio::select! {
                    res = serve(socket) => panic!("server exited: {res:?}"),
                    r = client => r,
                }
            })
            .await;

        drop(dir);
        result
    }

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

                    // Server-side enforcement check: bypass the typestate by
                    // using the private proxy traits directly. The server
                    // must reject business methods before the handshake even
                    // when the client tries.
                    let mut raw = unix::connect(&client_socket).await.expect("raw connect");
                    let err = AuthProxy::ping(&mut raw, "nope")
                        .await
                        .expect("call")
                        .expect_err("server should refuse ping before handshake");
                    assert_eq!(err, EchoError::NotAuthenticated);
                    drop(raw);

                    // Happy path through the public API. Canonicalise the
                    // workdir up front: on macOS `TempDir` lives under
                    // `/var/folders` reachable through the `/tmp` symlink,
                    // so the path that comes out of `TempDir::path()` may
                    // not equal its own canonical form.
                    let workdir = TempDir::new().unwrap();
                    let workdir_path = workdir.path().canonicalize().unwrap();
                    let mut conn = connect(&client_socket, &workdir_path)
                        .await
                        .expect("connect+handshake");

                    assert_eq!(conn.directory(), workdir_path);

                    let reply = conn.ping("hello").await.expect("call").expect("reply");
                    assert_eq!(reply.message, "hello");

                    let err = conn.ping("").await.expect("call").expect_err("error");
                    assert_eq!(err, EchoError::EmptyMessage);

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

                    // Server-side enforcement check: a second `Hello` on the
                    // *same* connection must be rejected. Drive that via
                    // `connect_unauthenticated` and the private proxy.
                    let mut raw = unix::connect(&client_socket).await.expect("raw connect");
                    UnauthProxy::hello(&mut raw, workdir_path.to_str().unwrap())
                        .await
                        .expect("call")
                        .expect("first hello on raw conn");
                    let err = UnauthProxy::hello(&mut raw, workdir_path.to_str().unwrap())
                        .await
                        .expect("call")
                        .expect_err("second hello should fail");
                    assert_eq!(err, EchoError::AlreadyHandshaken);
                    drop(raw);

                    // Drive the handshake one typestate transition at a time
                    // so the AwaitingProof intermediate state is exercised
                    // independently of the all-in-one `connect` helper.
                    let workdir2 = TempDir::new().unwrap();
                    let workdir2_path = workdir2.path().canonicalize().unwrap();
                    let unauth = connect_unauthenticated(&client_socket)
                        .await
                        .expect("connect_unauthenticated");
                    let awaiting = unauth.hello(&workdir2_path).await.expect("hello");
                    assert_eq!(awaiting.directory(), workdir2_path);
                    let challenge_path = awaiting.challenge_path();
                    assert!(challenge_path.starts_with(&workdir2_path));
                    let mut conn2 = awaiting.authenticate().await.expect("authenticate");
                    assert_eq!(
                        conn2
                            .ping("staged")
                            .await
                            .expect("call")
                            .expect("reply")
                            .message,
                        "staged"
                    );

                    // The handshake helper cleans up the challenge file.
                    let mut entries = tokio::fs::read_dir(&workdir_path).await.unwrap();
                    assert!(
                        entries.next_entry().await.unwrap().is_none(),
                        "challenge file should be removed by handshake"
                    );
                };

                tokio::select! {
                    res = serve(socket) => panic!("server exited: {res:?}"),
                    () = client => {}
                }
            })
            .await;
    }

    /// Server-side `Hello` should reject a path that isn't in canonical form
    /// (here: a path with a `..` component that resolves back to the same
    /// directory). The error must point at the canonical equivalent so the
    /// client can correct itself.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn hello_with_non_canonical_path_is_rejected() {
        run_against_server(|socket_path| async move {
            let workdir = TempDir::new().unwrap();
            let canonical = workdir.path().canonicalize().unwrap();
            // `<canonical>/../<basename>` resolves to `<canonical>` but isn't
            // itself canonical: `Path::components()` preserves the `..`,
            // unlike `.` which is normalised away during equality checks.
            let basename = canonical.file_name().unwrap().to_owned();
            let non_canonical = canonical.join("..").join(&basename);
            assert_ne!(non_canonical, canonical, "test setup: paths must differ");
            assert_eq!(
                non_canonical.canonicalize().unwrap(),
                canonical,
                "test setup: non_canonical must resolve to canonical"
            );

            let mut raw = unix::connect(&socket_path).await.expect("raw connect");
            let err = UnauthProxy::hello(&mut raw, non_canonical.to_str().unwrap())
                .await
                .expect("transport")
                .expect_err("non-canonical path must be rejected");
            match err {
                EchoError::NonCanonicalDirectory { reason } => {
                    assert!(
                        reason.contains("directory FD resolves to"),
                        "unexpected reason: {reason}"
                    );
                    assert!(
                        reason.contains(canonical.to_str().unwrap()),
                        "reason should name the canonical path: {reason}"
                    );
                }
                other => panic!("expected NonCanonicalDirectory, got {other:?}"),
            }
        })
        .await;
    }

    /// Server-side `Hello` should reject a `.`-component path even though
    /// `Path::eq` would consider it equal to the canonical form. We
    /// compare via `OsStr` precisely to keep this strict.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn hello_with_curdir_component_is_rejected() {
        run_against_server(|socket_path| async move {
            let workdir = TempDir::new().unwrap();
            let canonical = workdir.path().canonicalize().unwrap();
            let dotted = canonical.join(".");
            // Sanity: `Path::eq` *does* consider these equal (the bug we're
            // guarding against), but `OsStr::eq` does not.
            assert_eq!(dotted, canonical, "test premise: Path::eq normalises `.`");
            assert_ne!(
                dotted.as_os_str(),
                canonical.as_os_str(),
                "test premise: OsStr::eq does not"
            );

            let mut raw = unix::connect(&socket_path).await.expect("raw connect");
            let err = UnauthProxy::hello(&mut raw, dotted.to_str().unwrap())
                .await
                .expect("transport")
                .expect_err("`.`-component must be rejected");
            assert!(
                matches!(err, EchoError::NonCanonicalDirectory { .. }),
                "expected NonCanonicalDirectory, got {err:?}"
            );
        })
        .await;
    }

    /// Server-side `Hello` should reject a path that doesn't resolve at all,
    /// surfacing the underlying I/O error.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn hello_with_unresolvable_path_is_rejected() {
        run_against_server(|socket_path| async move {
            let workdir = TempDir::new().unwrap();
            let bogus = workdir.path().join("does-not-exist");
            let mut raw = unix::connect(&socket_path).await.expect("raw connect");
            let err = UnauthProxy::hello(&mut raw, bogus.to_str().unwrap())
                .await
                .expect("transport")
                .expect_err("unresolvable path must be rejected");
            match err {
                EchoError::NonCanonicalDirectory { reason } => assert!(
                    reason.contains("could not open"),
                    "unexpected reason: {reason}"
                ),
                other => panic!("expected NonCanonicalDirectory, got {other:?}"),
            }
        })
        .await;
    }

    /// Directory-component swap defence (only catchable because cap-std
    /// anchors the read to the directory FD captured at `Hello` time):
    /// after `Hello` issues a challenge against `<workdir>`, an attacker
    /// renames `<workdir>` away and puts a symlink with the same name
    /// pointing at a parallel directory holding a forged challenge file.
    /// The server must read the *original* directory's file (i.e. fail,
    /// because the legitimate client never wrote anything) rather than the
    /// decoy.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn authenticate_uses_dir_fd_not_repath() {
        run_against_server(|socket_path| async move {
            // Two siblings under one parent: `original` is what the client
            // claims, `decoy` is the attacker's prepared replacement.
            let parent = TempDir::new().unwrap();
            let parent_path = parent.path().canonicalize().unwrap();
            let original = parent_path.join("original");
            let decoy = parent_path.join("decoy");
            tokio::fs::create_dir(&original).await.unwrap();
            tokio::fs::create_dir(&decoy).await.unwrap();

            let mut raw = unix::connect(&socket_path).await.expect("raw connect");
            let hello = UnauthProxy::hello(&mut raw, original.to_str().unwrap())
                .await
                .expect("transport")
                .expect("hello accepted");

            // Plant the forged challenge file in the decoy directory.
            tokio::fs::write(
                decoy.join(format!("pixi-serve-challenge-{}", hello.challenge)),
                &hello.challenge,
            )
            .await
            .unwrap();

            // Swap: rename `original` away, replace it with a symlink to
            // `decoy`. A path-based `open(<original>/<challenge>)` would
            // now read the decoy file and authenticate — cap-std's FD
            // anchor must prevent that.
            let renamed = parent_path.join("original-moved-out-of-the-way");
            tokio::fs::rename(&original, &renamed).await.unwrap();
            tokio::fs::symlink(&decoy, &original).await.unwrap();

            let err = UnauthProxy::authenticate(&mut raw)
                .await
                .expect("transport")
                .expect_err("dir-swap attack must fail");
            assert!(
                matches!(err, EchoError::ChallengeFailed { .. }),
                "expected ChallengeFailed, got {err:?}"
            );
        })
        .await;
    }

    /// Symlink-swap defence: between `Hello` and `Authenticate` the client
    /// (or anyone with write access to the workdir) replaces the challenge
    /// file with a symlink that points at a file with the right content
    /// elsewhere. The server must refuse rather than read through the link.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn authenticate_refuses_symlinked_challenge_file() {
        run_against_server(|socket_path| async move {
            let workdir = TempDir::new().unwrap();
            let workdir_path = workdir.path().canonicalize().unwrap();
            let mut raw = unix::connect(&socket_path).await.expect("raw connect");
            let hello = UnauthProxy::hello(&mut raw, workdir_path.to_str().unwrap())
                .await
                .expect("transport")
                .expect("hello accepted");

            // Decoy file outside the workdir holding the right UUID — what
            // the attacker would point the symlink at.
            let elsewhere = TempDir::new().unwrap();
            let decoy = elsewhere.path().canonicalize().unwrap().join("decoy");
            tokio::fs::write(&decoy, &hello.challenge).await.unwrap();

            // Plant the symlink where the challenge file would be.
            let challenge_path =
                workdir_path.join(format!("pixi-serve-challenge-{}", hello.challenge));
            tokio::fs::symlink(&decoy, &challenge_path).await.unwrap();

            let err = UnauthProxy::authenticate(&mut raw)
                .await
                .expect("transport")
                .expect_err("authenticate via symlinked challenge must fail");
            match err {
                EchoError::ChallengeFailed { reason } => assert!(
                    reason.contains("could not open"),
                    "unexpected reason: {reason}"
                ),
                other => panic!("expected ChallengeFailed, got {other:?}"),
            }
        })
        .await;
    }

    /// Server-side `Authenticate` as the very first message must be rejected
    /// with `NotAwaitingProof`: until `Hello` has been sent the connection is
    /// in the initial state and has no challenge to verify.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn authenticate_before_hello_reports_not_awaiting_proof() {
        run_against_server(|socket_path| async move {
            let mut raw = unix::connect(&socket_path).await.expect("raw connect");
            let err = UnauthProxy::authenticate(&mut raw)
                .await
                .expect("transport")
                .expect_err("authenticate before hello must fail");
            assert_eq!(err, EchoError::NotAwaitingProof);
        })
        .await;
    }

    /// Server-side `Authenticate` should fail with a `ChallengeFailed` whose
    /// reason names the missing path when the client never wrote the file.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn authenticate_without_challenge_file_reports_missing() {
        run_against_server(|socket_path| async move {
            let workdir = TempDir::new().unwrap();
            let workdir_path = workdir.path().canonicalize().unwrap();
            let mut raw = unix::connect(&socket_path).await.expect("raw connect");
            UnauthProxy::hello(&mut raw, workdir_path.to_str().unwrap())
                .await
                .expect("transport")
                .expect("hello accepted");
            let err = UnauthProxy::authenticate(&mut raw)
                .await
                .expect("transport")
                .expect_err("authenticate without file must fail");
            match err {
                EchoError::ChallengeFailed { reason } => {
                    assert!(
                        reason.contains("could not open"),
                        "unexpected reason: {reason}"
                    );
                    assert!(
                        reason.contains("pixi-serve-challenge-"),
                        "reason should name the challenge file: {reason}"
                    );
                }
                other => panic!("expected ChallengeFailed, got {other:?}"),
            }
        })
        .await;
    }

    /// Server-side `Authenticate` should fail with a `ChallengeFailed` whose
    /// reason flags wrong content when the file exists but holds the wrong
    /// UUID.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn authenticate_with_wrong_content_reports_mismatch() {
        run_against_server(|socket_path| async move {
            let workdir = TempDir::new().unwrap();
            let workdir_path = workdir.path().canonicalize().unwrap();
            let mut raw = unix::connect(&socket_path).await.expect("raw connect");
            let hello = UnauthProxy::hello(&mut raw, workdir_path.to_str().unwrap())
                .await
                .expect("transport")
                .expect("hello accepted");
            let challenge_path =
                workdir_path.join(format!("pixi-serve-challenge-{}", hello.challenge));
            // Write garbage instead of the issued UUID. Different length and
            // different bytes — covers both the trim() and the equality check.
            tokio::fs::write(&challenge_path, "not-the-right-uuid")
                .await
                .unwrap();

            let err = UnauthProxy::authenticate(&mut raw)
                .await
                .expect("transport")
                .expect_err("authenticate with bad content must fail");
            match err {
                EchoError::ChallengeFailed { reason } => assert!(
                    reason.contains("wrong content"),
                    "unexpected reason: {reason}"
                ),
                other => panic!("expected ChallengeFailed, got {other:?}"),
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
