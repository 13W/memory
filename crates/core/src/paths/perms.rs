//! Filesystem permission and ownership helpers for the store tree.
//!
//! POSIX (spec 02 §2.1): directories are created `0700`, files `0600`, and every
//! managed path is verified to be owned by the current effective uid before it
//! is trusted. On non-unix platforms the store relies on the default per-user
//! ACLs of `%LOCALAPPDATA%`, so creation uses the platform default and owner
//! verification is a no-op.
//!
//! Modes are set **at creation time** (via `DirBuilder`/`OpenOptions`), never
//! create-then-chmod, so no window exists where a path is visible with a wider
//! mode. `0700`/`0600` are umask-robust: `mkdir(2)`/`open(2)` mask the requested
//! mode with `~umask`, but these modes only set owner bits, so the result is
//! exactly `0700`/`0600` under any umask.

use std::fs;
use std::io;
use std::path::Path;

use super::PathError;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

/// Idempotently ensure `dir` exists as a private (`0700`) directory owned by us.
///
/// A first call creates it; a repeated call re-asserts the mode after verifying
/// ownership. A pre-existing symlink or non-directory at the path is rejected
/// (symlink-swap defence), and a directory owned by another uid is refused
/// before any mode is written.
#[cfg(unix)]
pub fn ensure_dir(dir: &Path) -> Result<(), PathError> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // `symlink_metadata` does not follow links, so a symlink reports as
            // a symlink (not a dir) and is rejected here.
            let meta = fs::symlink_metadata(dir).map_err(|e| PathError::io(dir, e))?;
            if !meta.file_type().is_dir() {
                return Err(PathError::UnexpectedType {
                    path: dir.to_path_buf(),
                    expected: "directory",
                });
            }
            verify_owner_meta(dir, &meta)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| PathError::io(dir, e))?;
            Ok(())
        }
        Err(e) => Err(PathError::io(dir, e)),
    }
}

/// Idempotently ensure `path` exists as a private (`0600`) regular file owned by
/// us. Used by later tasks that create `store.lock`/`*.sqlite`; the mode and
/// ownership guarantees mirror [`ensure_dir`].
#[cfg(unix)]
pub fn ensure_file_0600(path: &Path) -> Result<(), PathError> {
    match fs::OpenOptions::new()
        .mode(0o600)
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(path).map_err(|e| PathError::io(path, e))?;
            if !meta.file_type().is_file() {
                return Err(PathError::UnexpectedType {
                    path: path.to_path_buf(),
                    expected: "file",
                });
            }
            verify_owner_meta(path, &meta)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .map_err(|e| PathError::io(path, e))?;
            Ok(())
        }
        Err(e) => Err(PathError::io(path, e)),
    }
}

/// Refuse a path that is not owned by the current effective uid.
///
/// Uses `symlink_metadata` (does not follow links), so it reports on the path
/// itself.
#[cfg(unix)]
pub fn verify_owner(path: &Path) -> Result<(), PathError> {
    let meta = fs::symlink_metadata(path).map_err(|e| PathError::io(path, e))?;
    verify_owner_meta(path, &meta)
}

#[cfg(unix)]
fn verify_owner_meta(path: &Path, meta: &fs::Metadata) -> Result<(), PathError> {
    let expected = effective_uid();
    let found = meta.uid();
    if found != expected {
        return Err(PathError::WrongOwner {
            path: path.to_path_buf(),
            expected_uid: expected,
            found_uid: found,
        });
    }
    Ok(())
}

/// The current process's effective uid.
///
/// `std` exposes no `geteuid`, so this calls libc directly (see the dependency
/// note in `CONTRIBUTING.md`).
#[cfg(unix)]
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: POSIX `geteuid` takes no arguments, reads no memory, and always
    // succeeds, returning the caller's effective uid as a plain integer.
    #[allow(unsafe_code)]
    unsafe {
        libc::geteuid()
    }
}

// ---- non-unix ---------------------------------------------------------------

/// Ensure `dir` exists, relying on the platform's default per-user ACLs.
#[cfg(not(unix))]
pub fn ensure_dir(dir: &Path) -> Result<(), PathError> {
    fs::create_dir_all(dir).map_err(|e| PathError::io(dir, e))
}

/// Ensure `path` exists as a regular file, relying on default per-user ACLs.
#[cfg(not(unix))]
pub fn ensure_file_0600(path: &Path) -> Result<(), PathError> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(PathError::io(path, e)),
    }
}

/// No-op on platforms without POSIX uid ownership (default ACLs apply).
#[cfg(not(unix))]
pub fn verify_owner(_path: &Path) -> Result<(), PathError> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use local_rag_test_support::TempHome;

    #[test]
    fn ensure_dir_sets_0700_and_is_idempotent() {
        let home = TempHome::new().expect("temp home");
        let dir = home.join("private");

        ensure_dir(&dir).expect("first create");
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "fresh dir is 0700");

        // Widen the mode, then a repeated ensure must re-assert 0700.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_dir(&dir).expect("second create is idempotent");
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "re-asserted to 0700");
    }

    #[test]
    fn ensure_file_sets_0600_and_is_idempotent() {
        let home = TempHome::new().expect("temp home");
        let file = home.join("secret");

        ensure_file_0600(&file).expect("first create");
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        ensure_file_0600(&file).expect("second create is idempotent");
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn ensure_dir_rejects_non_directory() {
        let home = TempHome::new().expect("temp home");
        let path = home.join("afile");
        ensure_file_0600(&path).expect("create file");
        assert!(matches!(
            ensure_dir(&path),
            Err(PathError::UnexpectedType {
                expected: "directory",
                ..
            })
        ));
    }

    #[test]
    fn verify_owner_accepts_our_own_dir() {
        let home = TempHome::new().expect("temp home");
        verify_owner(home.path()).expect("we own our temp home");
    }

    #[test]
    fn verify_owner_refuses_foreign_owner() {
        // Platform-gated wrong-owner refusal without needing privileges: `/` is
        // root-owned (uid 0). Skip only if we are actually root.
        if effective_uid() == 0 {
            return;
        }
        match verify_owner(Path::new("/")) {
            Err(PathError::WrongOwner { found_uid: 0, .. }) => {}
            other => panic!("expected WrongOwner(found_uid: 0), got {other:?}"),
        }
    }
}
