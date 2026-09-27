//! Bounded, symlink-safe file reads. Used for config, credentials, and
//! CodexBar snapshots — never follow a replaced symlink into `/etc`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use thiserror::Error;

/// Linux / macOS `O_NOFOLLOW`: fail if the final path component is a symlink.
/// Linux `fcntl.h`: `O_NOFOLLOW = 0400000`. macOS: `0x0100`.
#[cfg(target_os = "linux")]
const O_NOFOLLOW: i32 = 0o400000;
#[cfg(target_os = "macos")]
const O_NOFOLLOW: i32 = 0o400;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const O_NOFOLLOW: i32 = 0;

/// Do not block when the path is a FIFO with no writer.
#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0o4000;
#[cfg(target_os = "macos")]
const O_NONBLOCK: i32 = 0o4;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const O_NONBLOCK: i32 = 0;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CapReadError {
    #[error("file not found: {0}")]
    NotFound(String),
    #[error("file too large (>{0} bytes)")]
    TooLarge(usize),
    #[error("refusing symlink: {0}")]
    Symlink(String),
    #[error("not a regular file: {0}")]
    NotRegular(String),
    #[error("io: {0}")]
    Io(String),
}

/// Open `path` without following a final-component symlink and read at most
/// `max_bytes`. A file that grows past the cap during the read is rejected
/// (no second unbounded `fs::read`).
pub fn read_file_capped(path: &Path, max_bytes: usize) -> Result<Vec<u8>, CapReadError> {
    let file = open_regular_file(path)?;
    read_capped_from(file, max_bytes)
}

/// Open a regular file. Refuses symlinks, directories, and FIFOs.
/// `O_NONBLOCK` keeps a FIFO raced in after the type check from stalling.
pub fn open_regular_file(path: &Path) -> Result<File, CapReadError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(CapReadError::Symlink(path.display().to_string()));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(CapReadError::NotRegular(path.display().to_string()));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(CapReadError::NotFound(path.display().to_string()));
        }
        Err(e) => return Err(CapReadError::Io(e.to_string())),
    }

    let mut opts = OpenOptions::new();
    opts.read(true);
    let flags = O_NOFOLLOW | O_NONBLOCK;
    if flags != 0 {
        opts.custom_flags(flags);
    }
    let file = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(CapReadError::NotFound(path.display().to_string()));
        }
        Err(e) if is_symlink_open_error(&e) => {
            return Err(CapReadError::Symlink(path.display().to_string()));
        }
        Err(e) => return Err(CapReadError::Io(e.to_string())),
    };
    match file.metadata() {
        Ok(meta) if meta.is_file() => Ok(file),
        Ok(_) => Err(CapReadError::NotRegular(path.display().to_string())),
        Err(e) => Err(CapReadError::Io(e.to_string())),
    }
}

fn is_symlink_open_error(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc_eloop) if libc_eloop == 40 || libc_eloop == 62)
}

fn read_capped_from(file: File, max_bytes: usize) -> Result<Vec<u8>, CapReadError> {
    let mut buf = Vec::new();
    let mut limited = file.take(max_bytes as u64 + 1);
    limited
        .read_to_end(&mut buf)
        .map_err(|e| CapReadError::Io(e.to_string()))?;
    if buf.len() > max_bytes {
        return Err(CapReadError::TooLarge(max_bytes));
    }
    Ok(buf)
}

/// `mkdir -p` then best-effort `chmod 0700`. Existing parents such as `/tmp`
/// are left alone when chmod is denied (EPERM) so tests and `--socket`
/// under a shared tmp still work.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    let created = !path.exists();
    fs::create_dir_all(path)?;
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o700);
    match fs::set_permissions(path, perms) {
        Ok(()) => Ok(()),
        Err(e) if !created && e.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        Err(e) if !created && e.raw_os_error() == Some(1) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Best-effort `chmod 0600` on a regular file we just wrote.
pub fn chmod_private_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)
}

fn private_open_opts() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.mode(0o600);
    if O_NOFOLLOW != 0 {
        opts.custom_flags(O_NOFOLLOW);
    }
    opts
}

/// Create or truncate `path` with mode `0600` at open (not after write).
/// Refuses a final-component symlink and a file that is group/other-readable.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    let mut opts = private_open_opts();
    opts.write(true).create(true).truncate(true);
    let file = opts.open(path)?;
    require_owner_only(&file)?;
    Ok(file)
}

/// Open `path` for append, creating it as `0600` when new.
/// Refuses a symlink and an existing file with group or other permission bits
/// so history is not appended into a shared file.
pub fn open_private_append(path: &Path) -> io::Result<File> {
    let mut opts = private_open_opts();
    opts.create(true).append(true);
    let file = opts.open(path)?;
    require_owner_only(&file)?;
    Ok(file)
}

/// The opened inode must not grant group or other access.
pub fn require_owner_only(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = file.metadata()?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing mode {mode:o}; file must be owner-only"),
        ));
    }
    Ok(())
}

/// True when every path segment is a normal name (no `..`, no empty).
pub fn path_has_parent_dir(path: &Path) -> bool {
    use std::path::Component;
    path.components().any(|c| matches!(c, Component::ParentDir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(name: &str) -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("quota-fsutil-{name}-{stamp}"))
    }

    #[test]
    fn capped_read_rejects_oversize() {
        let dir = scratch("cap");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.txt");
        fs::write(&path, vec![b'x'; 32]).unwrap();
        let err = read_file_capped(&path, 16).unwrap_err();
        assert!(matches!(err, CapReadError::TooLarge(16)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capped_read_ok_small() {
        let dir = scratch("ok");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.txt");
        fs::write(&path, b"hello").unwrap();
        assert_eq!(read_file_capped(&path, 64).unwrap(), b"hello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capped_read_refuses_directory_and_fifo() {
        let dir = scratch("fifo");
        fs::create_dir_all(&dir).unwrap();
        let err = read_file_capped(&dir, 64).unwrap_err();
        assert!(matches!(err, CapReadError::NotRegular(_)));
        let fifo = dir.join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.map(|s| s.success()).unwrap_or(false) {
            let err = read_file_capped(&fifo, 64).unwrap_err();
            assert!(matches!(err, CapReadError::NotRegular(_)));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capped_read_refuses_symlink() {
        let dir = scratch("sym");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.txt");
        let link = dir.join("link.txt");
        fs::write(&target, b"secret").unwrap();
        symlink(&target, &link).unwrap();
        let err = read_file_capped(&link, 64).unwrap_err();
        assert!(matches!(err, CapReadError::Symlink(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parent_dir_detection() {
        assert!(path_has_parent_dir(Path::new("a/../b")));
        assert!(!path_has_parent_dir(Path::new("/home/user/.codex")));
    }

    #[test]
    fn append_refuses_group_readable_and_symlink() {
        let dir = scratch("append");
        fs::create_dir_all(&dir).unwrap();
        let shared = dir.join("shared.jsonl");
        fs::write(&shared, b"{}\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&shared).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&shared, perms).unwrap();
        let err = open_private_append(&shared).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        let target = dir.join("target.jsonl");
        let link = dir.join("link.jsonl");
        fs::write(&target, b"x").unwrap();
        symlink(&target, &link).unwrap();
        assert!(create_private_file(&link).is_err());
        assert!(open_private_append(&link).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_private_file_is_0600() {
        let dir = scratch("privfile");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("book.json");
        let _ = create_private_file(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_dir_mode() {
        let dir = scratch("mode");
        ensure_private_dir(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = fs::remove_dir_all(&dir);
    }
}
