use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::PathBuf;

/// Directory holding the server socket and log, private to the user.
pub fn runtime_dir() -> io::Result<PathBuf> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map_or_else(std::env::temp_dir, PathBuf::from);
    let dir = base.join(format!("tmux-replacement-{uid}"));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&dir)?;
    // Refuse a directory someone else created for us in a shared /tmp.
    let meta = std::fs::metadata(&dir)?;
    if meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} must be owned by you with mode 700", dir.display()),
        ));
    }
    Ok(dir)
}

pub fn log_path() -> io::Result<PathBuf> {
    Ok(runtime_dir()?.join("server.log"))
}

pub fn socket_path() -> io::Result<PathBuf> {
    Ok(runtime_dir()?.join("default.sock"))
}
