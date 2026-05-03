//! Client-side prefix localisation.
//!
//! Three modes are supported:
//!
//! - [`Mode::Reflink`] (default): walk the server tree and
//!   reflink-copy every regular file (falling back to a plain
//!   byte copy where reflink isn't supported). Symlinks are
//!   reproduced with in-prefix absolute targets rewritten to
//!   point at `local_path`. Cheap on CoW filesystems; the local
//!   prefix is independent of the server's `data/`.
//! - [`Mode::Copy`]: same walk as [`Mode::Reflink`] but always
//!   plain-copies. Slowest, but works on any filesystem and yields
//!   a fully independent local prefix.
//! - [`Mode::Symlink`]: a single symlink at the user's
//!   `~/.pixi/envs/<env_name>` pointing at the server-managed
//!   prefix `<data>/<HASH>/`. Cheapest to materialise; the local
//!   prefix breaks if the server's `data/` is GC'd.

use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use fs_err::tokio as tokio_fs;
use thiserror::Error;
use walkdir::WalkDir;

/// Localisation mode picked by the caller; see the module docs.
/// Default is [`Mode::Reflink`]: cheap on CoW filesystems, falls
/// back to a plain copy on others, and yields a local prefix that
/// survives the daemon's `data/` getting GC'd.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    Symlink,
    #[default]
    Reflink,
    Copy,
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "symlink" => Ok(Self::Symlink),
            "reflink" => Ok(Self::Reflink),
            "copy" => Ok(Self::Copy),
            other => Err(format!(
                "unknown localise mode {other:?}; expected one of `symlink`, `reflink`, `copy`"
            )),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Symlink => "symlink",
            Self::Reflink => "reflink",
            Self::Copy => "copy",
        })
    }
}

/// Marker file written at the root of a walk-localised prefix so a
/// re-run can tell "directory we created" from "directory the user
/// brought" and refuse to clobber the latter.
const LOCALISE_MARKER: &str = ".pixi-localise";

/// Reasons [`localise_prefix`] can refuse or fail.
#[derive(Debug, Error, miette::Diagnostic)]
pub enum LocaliseError {
    /// `local_path` already exists as a real directory, not a
    /// symlink. Most commonly this means the user has a non-daemon
    /// install of the same env on disk; refusing is safer than
    /// silently overwriting.
    #[error(
        "{0} already exists as a real directory; remove or move it before localising the daemon-managed prefix"
    )]
    DirectoryConflict(PathBuf),

    /// `local_path` already exists as a regular file. Almost
    /// certainly user error; refusing rather than overwriting.
    #[error("{0} already exists and is not a symlink; remove it before localising")]
    FileConflict(PathBuf),

    /// Generic I/O failure — `stat`, `remove`, `mkdir`, or `symlink`
    /// failed. The wrapped error names the operation; the path
    /// names the file we were operating on.
    #[error("{op} failed for {path}")]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl LocaliseError {
    fn io(op: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            op,
            path: path.to_path_buf(),
            source,
        }
    }
}

/// `mkdir -p` for `local_path.parent()`, skipping the relative-path
/// edge case where `parent()` is an empty string. Shared by every
/// `localise_*` entry point that ends with a write at `local_path`.
async fn ensure_parent_dir(local_path: &Path) -> Result<(), LocaliseError> {
    if let Some(parent) = local_path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio_fs::create_dir_all(parent)
            .await
            .map_err(|e| LocaliseError::io("create parent directory", parent, e))?;
    }
    Ok(())
}

/// Localise `server_prefix` at `local_path` using `mode`.
///
/// In [`Mode::Symlink`], `local_path` ends up as a symlink pointing
/// at `server_prefix`; idempotent against a previously-written
/// symlink at the same target; refuses to clobber a real
/// file/directory at `local_path`.
///
/// In [`Mode::Reflink`] and [`Mode::Copy`], `local_path` ends up as
/// a real directory with the server tree's contents reproduced
/// inside it (reflink-copying regular files in the former mode,
/// byte-copying in the latter). Symlinks inside the tree are
/// preserved; absolute symlink targets that point inside
/// `server_prefix` are rewritten to point inside `local_path`
/// instead. A `.pixi-localise` marker file at the root records the
/// mode and lets a re-run distinguish "directory we created" from
/// "directory the user brought" — the latter is refused with
/// [`LocaliseError::DirectoryConflict`].
#[cfg(unix)]
pub async fn localise_prefix(
    server_prefix: &Path,
    local_path: &Path,
    mode: Mode,
) -> Result<(), LocaliseError> {
    match mode {
        Mode::Symlink => localise_symlink(server_prefix, local_path).await,
        Mode::Reflink | Mode::Copy => localise_walk(server_prefix, local_path, mode).await,
    }
}

#[cfg(unix)]
async fn localise_symlink(server_prefix: &Path, local_path: &Path) -> Result<(), LocaliseError> {
    match tokio_fs::symlink_metadata(local_path).await {
        Ok(meta) if meta.file_type().is_symlink() => {
            // Existing symlink. If it already points at `server_prefix`
            // we're done; otherwise replace it.
            match tokio_fs::read_link(local_path).await {
                Ok(target) if target == server_prefix => return Ok(()),
                Ok(_) | Err(_) => {
                    tokio_fs::remove_file(local_path)
                        .await
                        .map_err(|e| LocaliseError::io("remove existing symlink", local_path, e))?;
                }
            }
        }
        Ok(meta) if meta.is_dir() => {
            return Err(LocaliseError::DirectoryConflict(local_path.to_path_buf()));
        }
        Ok(_) => {
            return Err(LocaliseError::FileConflict(local_path.to_path_buf()));
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // Will create below.
        }
        Err(e) => {
            return Err(LocaliseError::io("stat", local_path, e));
        }
    }

    ensure_parent_dir(local_path).await?;
    tokio_fs::symlink(server_prefix, local_path)
        .await
        .map_err(|e| LocaliseError::io("create symlink", local_path, e))
}

#[cfg(unix)]
async fn localise_walk(
    server_prefix: &Path,
    local_path: &Path,
    mode: Mode,
) -> Result<(), LocaliseError> {
    debug_assert!(matches!(mode, Mode::Reflink | Mode::Copy));

    // Idempotency / conflict check on `local_path`.
    match tokio_fs::symlink_metadata(local_path).await {
        Ok(meta) if meta.file_type().is_symlink() => {
            // Stale symlink from a previous Symlink-mode install. Replace.
            tokio_fs::remove_file(local_path)
                .await
                .map_err(|e| LocaliseError::io("remove existing symlink", local_path, e))?;
        }
        Ok(meta) if meta.is_dir() => {
            // Refuse unless we placed it ourselves — the marker file
            // is how we tell a daemon-localised tree apart from a
            // user-managed directory that happens to share the path.
            let marker = local_path.join(LOCALISE_MARKER);
            match tokio_fs::symlink_metadata(&marker).await {
                Ok(_) => {
                    tokio_fs::remove_dir_all(local_path).await.map_err(|e| {
                        LocaliseError::io("remove existing localised tree", local_path, e)
                    })?;
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    return Err(LocaliseError::DirectoryConflict(local_path.to_path_buf()));
                }
                Err(e) => {
                    return Err(LocaliseError::io("stat", &marker, e));
                }
            }
        }
        Ok(_) => {
            return Err(LocaliseError::FileConflict(local_path.to_path_buf()));
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // Fresh install location.
        }
        Err(e) => {
            return Err(LocaliseError::io("stat", local_path, e));
        }
    }

    ensure_parent_dir(local_path).await?;

    // The walk itself is sync I/O against many small files; keep
    // it on a blocking thread so it doesn't stall the runtime.
    let server_owned = server_prefix.to_path_buf();
    let local_owned = local_path.to_path_buf();
    tokio::task::spawn_blocking(move || walk_sync(&server_owned, &local_owned, mode))
        .await
        .map_err(|e| {
            LocaliseError::io(
                "walk task",
                local_path,
                std::io::Error::other(format!("join error: {e}")),
            )
        })??;

    tokio_fs::write(local_path.join(LOCALISE_MARKER), mode.to_string())
        .await
        .map_err(|e| LocaliseError::io("write marker", local_path, e))?;

    Ok(())
}

#[cfg(unix)]
fn walk_sync(server_prefix: &Path, local_path: &Path, mode: Mode) -> Result<(), LocaliseError> {
    use std::os::unix::fs::symlink;

    fs_err::create_dir_all(local_path)
        .map_err(|e| LocaliseError::io("create local prefix", local_path, e))?;

    for entry in WalkDir::new(server_prefix).follow_links(false) {
        let entry = entry
            .map_err(|e| LocaliseError::io("walk", server_prefix, std::io::Error::other(e)))?;
        let src = entry.path();
        let rel = src.strip_prefix(server_prefix).unwrap_or(src);
        let dst = local_path.join(rel);
        if rel.as_os_str().is_empty() {
            // Root of the walk; already created above.
            continue;
        }

        let ft = entry.file_type();
        if ft.is_dir() {
            fs_err::create_dir_all(&dst)
                .map_err(|e| LocaliseError::io("create directory", &dst, e))?;
        } else if ft.is_symlink() {
            let target =
                fs_err::read_link(src).map_err(|e| LocaliseError::io("read symlink", src, e))?;
            let rewritten = rewrite_symlink_target(&target, server_prefix, local_path);
            symlink(&rewritten, &dst).map_err(|e| LocaliseError::io("create symlink", &dst, e))?;
        } else if ft.is_file() {
            if let Some(parent) = dst.parent() {
                fs_err::create_dir_all(parent)
                    .map_err(|e| LocaliseError::io("create parent directory", parent, e))?;
            }
            match mode {
                Mode::Reflink => {
                    // Falls back to a plain byte copy on filesystems that
                    // don't support reflink.
                    reflink_copy::reflink_or_copy(src, &dst)
                        .map_err(|e| LocaliseError::io("reflink-or-copy", &dst, e))?;
                }
                Mode::Copy => {
                    fs_err::copy(src, &dst).map_err(|e| LocaliseError::io("copy", &dst, e))?;
                }
                Mode::Symlink => unreachable!("walk_sync only runs for Reflink/Copy"),
            }
        }
        // Other file types (sockets, fifos, devices) are not expected
        // inside a conda prefix; skip silently.
    }
    Ok(())
}

/// If `target` is an absolute path inside `server_prefix`, rewrite
/// it to the corresponding location under `local_path`. Otherwise
/// return `target` unchanged — relative symlinks and out-of-prefix
/// absolute symlinks are reproduced verbatim.
fn rewrite_symlink_target(target: &Path, server_prefix: &Path, local_path: &Path) -> PathBuf {
    if target.is_absolute()
        && let Ok(rel) = target.strip_prefix(server_prefix)
    {
        return local_path.join(rel);
    }
    target.to_path_buf()
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a minimal server-prefix-shaped directory with one
    /// nested file so tests can read through the symlink and
    /// observe the contents end up the same.
    async fn server_prefix_with_marker(dir: &Path) {
        tokio_fs::create_dir_all(dir.join("conda-meta"))
            .await
            .unwrap();
        tokio_fs::write(dir.join("conda-meta").join("marker"), b"hello")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn creates_symlink_when_target_missing() {
        let server = TempDir::new().unwrap();
        let local_root = TempDir::new().unwrap();
        let local = local_root.path().join("foo");
        server_prefix_with_marker(server.path()).await;

        localise_prefix(server.path(), &local, Mode::Symlink)
            .await
            .unwrap();

        let meta = tokio::fs::symlink_metadata(&local).await.unwrap();
        assert!(meta.file_type().is_symlink());
        // Reading through the symlink reaches the server file.
        let contents = tokio::fs::read(local.join("conda-meta").join("marker"))
            .await
            .unwrap();
        assert_eq!(contents, b"hello");
    }

    #[tokio::test]
    async fn second_call_with_same_target_is_noop() {
        let server = TempDir::new().unwrap();
        let local_root = TempDir::new().unwrap();
        let local = local_root.path().join("foo");
        server_prefix_with_marker(server.path()).await;

        localise_prefix(server.path(), &local, Mode::Symlink)
            .await
            .unwrap();
        let mtime_first = tokio::fs::symlink_metadata(&local)
            .await
            .unwrap()
            .modified()
            .unwrap();

        // Sleep a smidge so a replacement would have a different
        // mtime, then re-localise.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        localise_prefix(server.path(), &local, Mode::Symlink)
            .await
            .unwrap();
        let mtime_second = tokio::fs::symlink_metadata(&local)
            .await
            .unwrap()
            .modified()
            .unwrap();

        assert_eq!(
            mtime_first, mtime_second,
            "second call with the same target must not replace the symlink"
        );
    }

    #[tokio::test]
    async fn stale_target_is_rewritten() {
        let local_root = TempDir::new().unwrap();
        let local = local_root.path().join("foo");

        let old_server = TempDir::new().unwrap();
        let new_server = TempDir::new().unwrap();
        server_prefix_with_marker(new_server.path()).await;

        localise_prefix(old_server.path(), &local, Mode::Symlink)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_link(&local).await.unwrap(),
            old_server.path()
        );

        localise_prefix(new_server.path(), &local, Mode::Symlink)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_link(&local).await.unwrap(),
            new_server.path()
        );
        // The new prefix's content shows up through the rewritten symlink.
        let contents = tokio::fs::read(local.join("conda-meta").join("marker"))
            .await
            .unwrap();
        assert_eq!(contents, b"hello");
    }

    #[tokio::test]
    async fn refuses_real_directory() {
        let server = TempDir::new().unwrap();
        let local_root = TempDir::new().unwrap();
        let local = local_root.path().join("foo");
        // The user has a non-daemon install here already.
        tokio::fs::create_dir_all(&local).await.unwrap();

        let err = localise_prefix(server.path(), &local, Mode::Symlink)
            .await
            .unwrap_err();
        assert!(
            matches!(err, LocaliseError::DirectoryConflict(_)),
            "expected DirectoryConflict, got {err:?}"
        );
    }

    #[tokio::test]
    async fn refuses_regular_file() {
        let server = TempDir::new().unwrap();
        let local_root = TempDir::new().unwrap();
        let local = local_root.path().join("foo");
        tokio::fs::write(&local, b"in the way").await.unwrap();

        let err = localise_prefix(server.path(), &local, Mode::Symlink)
            .await
            .unwrap_err();
        assert!(
            matches!(err, LocaliseError::FileConflict(_)),
            "expected FileConflict, got {err:?}"
        );
    }

    /// Build a representative server prefix exercising every symlink
    /// shape the walker has to handle: regular files at multiple
    /// depths, a relative in-prefix symlink, an absolute in-prefix
    /// symlink, and an absolute out-of-prefix symlink.
    async fn rich_server_prefix(server: &Path, out_of_prefix_target: &Path) {
        use std::os::unix::fs::symlink;

        tokio_fs::create_dir_all(server.join("bin")).await.unwrap();
        tokio_fs::create_dir_all(server.join("lib")).await.unwrap();
        tokio_fs::write(server.join("bin").join("foo"), b"binary contents")
            .await
            .unwrap();
        tokio_fs::write(server.join("lib").join("libfoo.so.1.0"), b"libfoo")
            .await
            .unwrap();

        // Relative symlink within `lib/`.
        symlink("libfoo.so.1.0", server.join("lib").join("libfoo.so.1")).unwrap();

        // Absolute symlink that points back into `server` — the kind we
        // need to rewrite so the localised tree stays self-referential.
        symlink(
            server.join("lib").join("libfoo.so.1.0"),
            server.join("lib").join("libfoo.so"),
        )
        .unwrap();

        // Absolute symlink that points *outside* `server` — must be
        // preserved verbatim.
        symlink(out_of_prefix_target, server.join("etc-shortcut")).unwrap();
    }

    #[tokio::test]
    async fn reflink_walk_reproduces_files_and_rewrites_in_prefix_symlinks() {
        use std::os::unix::fs::MetadataExt;

        let outside = TempDir::new().unwrap();
        let outside_target = outside.path().join("ld.so.cache");
        tokio_fs::write(&outside_target, b"ld cache").await.unwrap();

        let server = TempDir::new().unwrap();
        rich_server_prefix(server.path(), &outside_target).await;

        let local_root = TempDir::new().unwrap();
        let local = local_root.path().join("env");

        localise_prefix(server.path(), &local, Mode::Reflink)
            .await
            .unwrap();

        // Regular files: present, content matches, distinct inode (i.e.
        // not a symlink, even on filesystems where reflink falls back
        // to copy).
        let foo_local = tokio::fs::read(local.join("bin").join("foo"))
            .await
            .unwrap();
        assert_eq!(foo_local, b"binary contents");
        let foo_meta = tokio::fs::symlink_metadata(local.join("bin").join("foo"))
            .await
            .unwrap();
        assert!(foo_meta.is_file());
        let server_foo_meta = tokio::fs::symlink_metadata(server.path().join("bin").join("foo"))
            .await
            .unwrap();
        // Same mtime and size aren't required, but for the
        // reflink_or_copy path either reflink (same content, same
        // size) or fallback copy. Either way, content-equality is
        // the contract we promise.
        assert_eq!(foo_meta.size(), server_foo_meta.size());

        // Relative symlink: preserved verbatim.
        let rel = tokio::fs::read_link(local.join("lib").join("libfoo.so.1"))
            .await
            .unwrap();
        assert_eq!(rel, Path::new("libfoo.so.1.0"));
        // And it resolves through to the right content.
        let via_rel = tokio::fs::read(local.join("lib").join("libfoo.so.1"))
            .await
            .unwrap();
        assert_eq!(via_rel, b"libfoo");

        // Absolute in-prefix symlink: rewritten to point inside `local`.
        let abs_in = tokio::fs::read_link(local.join("lib").join("libfoo.so"))
            .await
            .unwrap();
        assert_eq!(abs_in, local.join("lib").join("libfoo.so.1.0"));

        // Absolute out-of-prefix symlink: preserved verbatim.
        let abs_out = tokio::fs::read_link(local.join("etc-shortcut"))
            .await
            .unwrap();
        assert_eq!(abs_out, outside_target);

        // Marker file is in place.
        let marker = tokio::fs::read_to_string(local.join(LOCALISE_MARKER))
            .await
            .unwrap();
        assert_eq!(marker, "reflink");
    }

    #[tokio::test]
    async fn copy_walk_reproduces_files() {
        let outside = TempDir::new().unwrap();
        let outside_target = outside.path().join("placeholder");
        tokio_fs::write(&outside_target, b"out").await.unwrap();

        let server = TempDir::new().unwrap();
        rich_server_prefix(server.path(), &outside_target).await;

        let local_root = TempDir::new().unwrap();
        let local = local_root.path().join("env");

        localise_prefix(server.path(), &local, Mode::Copy)
            .await
            .unwrap();

        let foo = tokio::fs::read(local.join("bin").join("foo"))
            .await
            .unwrap();
        assert_eq!(foo, b"binary contents");
        let abs_in = tokio::fs::read_link(local.join("lib").join("libfoo.so"))
            .await
            .unwrap();
        assert_eq!(abs_in, local.join("lib").join("libfoo.so.1.0"));
        let marker = tokio::fs::read_to_string(local.join(LOCALISE_MARKER))
            .await
            .unwrap();
        assert_eq!(marker, "copy");
    }

    /// Re-running a walk-mode localise replaces the previous tree
    /// in place — the `.pixi-localise` marker tells us we placed it
    /// ourselves, so it's safe to clobber.
    #[tokio::test]
    async fn walk_idempotent_via_marker() {
        let outside = TempDir::new().unwrap();
        tokio_fs::write(outside.path().join("x"), b"x")
            .await
            .unwrap();

        let server = TempDir::new().unwrap();
        rich_server_prefix(server.path(), &outside.path().join("x")).await;

        let local_root = TempDir::new().unwrap();
        let local = local_root.path().join("env");

        localise_prefix(server.path(), &local, Mode::Copy)
            .await
            .unwrap();
        // Drop a stray file under the old tree to confirm the second
        // run blew it away.
        tokio_fs::write(local.join("bin").join("stale"), b"stale")
            .await
            .unwrap();

        localise_prefix(server.path(), &local, Mode::Copy)
            .await
            .unwrap();

        assert!(!local.join("bin").join("stale").exists());
        assert!(local.join("bin").join("foo").exists());
    }

    /// A directory at `local_path` without our marker is the user's;
    /// refuse rather than risk deleting their data.
    #[tokio::test]
    async fn walk_refuses_unmarked_directory() {
        let server = TempDir::new().unwrap();
        server_prefix_with_marker(server.path()).await;

        let local_root = TempDir::new().unwrap();
        let local = local_root.path().join("env");
        // User's data, no marker.
        tokio_fs::create_dir_all(&local).await.unwrap();
        tokio_fs::write(local.join("important"), b"do not delete")
            .await
            .unwrap();

        let err = localise_prefix(server.path(), &local, Mode::Copy)
            .await
            .unwrap_err();
        assert!(
            matches!(err, LocaliseError::DirectoryConflict(_)),
            "expected DirectoryConflict, got {err:?}"
        );
        // And the user's data is intact.
        assert!(local.join("important").exists());
    }

    /// A symlink left over from `Mode::Symlink` is replaced by a real
    /// tree without complaint.
    #[tokio::test]
    async fn walk_replaces_symlink_from_previous_mode() {
        let outside = TempDir::new().unwrap();
        tokio_fs::write(outside.path().join("x"), b"x")
            .await
            .unwrap();

        let server_old = TempDir::new().unwrap();
        rich_server_prefix(server_old.path(), &outside.path().join("x")).await;
        let server_new = TempDir::new().unwrap();
        rich_server_prefix(server_new.path(), &outside.path().join("x")).await;

        let local_root = TempDir::new().unwrap();
        let local = local_root.path().join("env");

        localise_prefix(server_old.path(), &local, Mode::Symlink)
            .await
            .unwrap();
        localise_prefix(server_new.path(), &local, Mode::Reflink)
            .await
            .unwrap();

        let meta = tokio::fs::symlink_metadata(&local).await.unwrap();
        assert!(
            meta.is_dir(),
            "after reflink localise, local_path must be a real directory"
        );
        assert!(local.join(LOCALISE_MARKER).exists());
    }

    #[test]
    fn rewrite_target_inside_prefix() {
        let server = Path::new("/srv/data/abcd");
        let local = Path::new("/home/u/.pixi/envs/foo");
        let rewritten =
            rewrite_symlink_target(&server.join("lib").join("libfoo.so.1.0"), server, local);
        assert_eq!(rewritten, local.join("lib").join("libfoo.so.1.0"));
    }

    #[test]
    fn rewrite_target_outside_prefix_unchanged() {
        let server = Path::new("/srv/data/abcd");
        let local = Path::new("/home/u/.pixi/envs/foo");
        let target = Path::new("/etc/ld.so.cache");
        assert_eq!(rewrite_symlink_target(target, server, local), target);
    }

    #[test]
    fn rewrite_target_relative_unchanged() {
        let server = Path::new("/srv/data/abcd");
        let local = Path::new("/home/u/.pixi/envs/foo");
        let target = Path::new("../sibling");
        assert_eq!(rewrite_symlink_target(target, server, local), target);
    }

    #[test]
    fn mode_round_trip() {
        for (s, m) in [
            ("symlink", Mode::Symlink),
            ("reflink", Mode::Reflink),
            ("copy", Mode::Copy),
        ] {
            assert_eq!(s.parse::<Mode>().unwrap(), m);
            assert_eq!(m.to_string(), s);
        }
        assert!("ramshackle".parse::<Mode>().is_err());
    }

    /// Pins the documented default. Flipping it is a deliberate
    /// behavioural change for the daemon path; this test catches
    /// an accidental flip back.
    #[test]
    fn default_mode_is_reflink() {
        assert_eq!(Mode::default(), Mode::Reflink);
    }
}
