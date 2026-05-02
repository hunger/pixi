//! Client-side prefix localisation: a single symlink at the
//! user's `~/.pixi/envs/<env_name>` pointing at a server-managed
//! prefix `<data>/<HASH>/`.
//!
//! This is the minimum viable [`localise_prefix`] mode. Trampolines
//! bake the *local* path (`<env_root>/<env_name>`) as `CONDA_PREFIX`,
//! and the kernel follows the symlink whenever something reads
//! through it. Cost: deleting the server prefix breaks the client
//! prefix until a re-install fixes it; the reflink-copy mode in
//! Step 7 removes that dependency.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use fs_err::tokio as tokio_fs;
use thiserror::Error;

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

/// Localise `server_prefix` at `local_path` by writing (or refreshing)
/// a single symlink. Idempotent: if a symlink already points at
/// `server_prefix`, no-op; if it points elsewhere (a previous
/// install at a different HASH), rewrite. Real directories or
/// regular files at `local_path` are an error — the caller has to
/// decide what to do with them.
///
/// `server_prefix` is *not* required to exist when the symlink is
/// written — the kernel doesn't validate symlink targets — but
/// reads through `local_path` will obviously fail if it doesn't.
/// Callers normally invoke this right after a successful daemon
/// install, when the server prefix is fresh on disk.
#[cfg(unix)]
pub async fn localise_prefix(server_prefix: &Path, local_path: &Path) -> Result<(), LocaliseError> {
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

    if let Some(parent) = local_path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio_fs::create_dir_all(parent)
            .await
            .map_err(|e| LocaliseError::io("create parent directory", parent, e))?;
    }
    tokio_fs::symlink(server_prefix, local_path)
        .await
        .map_err(|e| LocaliseError::io("create symlink", local_path, e))
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

        localise_prefix(server.path(), &local).await.unwrap();

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

        localise_prefix(server.path(), &local).await.unwrap();
        let mtime_first = tokio::fs::symlink_metadata(&local)
            .await
            .unwrap()
            .modified()
            .unwrap();

        // Sleep a smidge so a replacement would have a different
        // mtime, then re-localise.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        localise_prefix(server.path(), &local).await.unwrap();
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

        localise_prefix(old_server.path(), &local).await.unwrap();
        assert_eq!(
            tokio::fs::read_link(&local).await.unwrap(),
            old_server.path()
        );

        localise_prefix(new_server.path(), &local).await.unwrap();
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

        let err = localise_prefix(server.path(), &local).await.unwrap_err();
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

        let err = localise_prefix(server.path(), &local).await.unwrap_err();
        assert!(
            matches!(err, LocaliseError::FileConflict(_)),
            "expected FileConflict, got {err:?}"
        );
    }
}
