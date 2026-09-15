//! Atomic filesystem artifact persistence (Task 5.2, codex M4).
//!
//! Every write is: temp file in the SAME directory → `fsync` → atomic
//! `rename` → `fsync` the directory. A crash leaves either no visible file
//! or the complete bytes — never a partial artifact. All paths live under
//! the canonicalized root; file names derive only from job UUIDs or content
//! addresses, and symlinked targets are refused on read.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use application::error::StoreError;

fn io_error(context: &'static str) -> impl Fn(std::io::Error) -> StoreError {
    move |error| StoreError::Backend(format!("{context}: {error}"))
}

/// Canonicalizes the artifact root, creating it first if needed.
pub(super) fn canonical_root(root: &Path) -> Result<PathBuf, StoreError> {
    fs::create_dir_all(root).map_err(io_error("create render dir"))?;
    fs::canonicalize(root).map_err(io_error("canonicalize render dir"))
}

/// Atomically writes `bytes` as `name` under `root` (canonicalized).
/// `name` must be a bare file name — path separators are refused.
pub(super) fn write_atomic(root: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, StoreError> {
    if name.contains(['/', '\\']) || name == "." || name == ".." {
        return Err(StoreError::Invariant(
            "artifact name must be a bare file name",
        ));
    }
    let root = canonical_root(root)?;
    let final_path = root.join(name);
    let temp_path = root.join(format!("{name}.tmp"));
    {
        let mut temp = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_path)
            .map_err(io_error("open temp artifact"))?;
        temp.write_all(bytes).map_err(io_error("write artifact"))?;
        temp.sync_all().map_err(io_error("fsync artifact"))?;
    }
    fs::rename(&temp_path, &final_path).map_err(io_error("rename artifact"))?;
    if let Ok(dir) = File::open(&root) {
        // Directory fsync is advisory on some filesystems; failure to sync
        // the entry is not a torn artifact, so it is not an error.
        let _ = dir.sync_all();
    }
    Ok(final_path)
}

/// Reads a finalized artifact strictly under the canonicalized root,
/// refusing symlinks and any name that escapes the root.
pub(super) fn read_under_root(root: &Path, name: &str) -> Result<Option<Vec<u8>>, StoreError> {
    if name.contains(['/', '\\']) || name == "." || name == ".." {
        return Ok(None);
    }
    let root = canonical_root(root)?;
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => return Ok(None),
        Ok(_) => {}
        Err(_) => return Ok(None),
    }
    // `name` is a bare component and the final component is not a symlink,
    // so reading `root/name` cannot escape the canonical root.
    fs::read(path).map(Some).map_err(io_error("read artifact"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("opinions-w2-artifacts")
            .join(name)
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_is_atomic_and_readable_back() {
        let root = scratch("atomic");
        let path = write_atomic(&root, "a.svg", b"<svg/>").unwrap();
        assert!(path.ends_with("a.svg"));
        assert_eq!(read_under_root(&root, "a.svg").unwrap().unwrap(), b"<svg/>");
        // No temp residue after a successful write.
        assert!(!root.join("a.svg.tmp").exists());
        // Overwrite lands complete new bytes.
        write_atomic(&root, "a.svg", b"<svg>2</svg>").unwrap();
        assert_eq!(
            read_under_root(&root, "a.svg").unwrap().unwrap(),
            b"<svg>2</svg>"
        );
    }

    #[test]
    fn partial_writes_are_never_visible() {
        let root = scratch("partial");
        // Simulate a crash between temp write and rename: a stray temp file.
        fs::write(root.join("b.svg.tmp"), b"partial").unwrap();
        assert_eq!(read_under_root(&root, "b.svg").unwrap(), None);
        // The temp name itself is not a servable artifact either — serving
        // reads only exact UUID-derived names, and the temp file is ignored.
        let final_written = write_atomic(&root, "b.svg", b"complete").unwrap();
        assert_eq!(fs::read(final_written).unwrap(), b"complete");
    }

    #[test]
    fn traversal_names_are_refused() {
        let root = scratch("traversal");
        assert!(matches!(
            write_atomic(&root, "../escape.svg", b"x"),
            Err(StoreError::Invariant(_))
        ));
        assert!(matches!(
            write_atomic(&root, "a/b.svg", b"x"),
            Err(StoreError::Invariant(_))
        ));
        assert_eq!(read_under_root(&root, "../etc/passwd").unwrap(), None);
        assert_eq!(read_under_root(&root, "..").unwrap(), None);
        assert_eq!(read_under_root(&root, "missing.svg").unwrap(), None);
    }

    #[test]
    fn symlinks_are_refused_on_read() {
        let root = scratch("symlink");
        let outside = scratch("symlink-target");
        fs::write(outside.join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.svg")).unwrap();
        assert_eq!(read_under_root(&root, "link.svg").unwrap(), None);
    }

    #[test]
    fn filesystem_failures_remain_typed_backend_errors() {
        let root = scratch("backend-error");
        let file = root.join("not-a-directory");
        fs::write(&file, b"occupied").unwrap();
        let error = canonical_root(&file).unwrap_err();
        assert!(
            matches!(error, StoreError::Backend(message) if message.contains("create render dir"))
        );
    }
}
