use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn event(message: impl AsRef<str>) {
    if std::env::var("CCTTY_LOG").ok().as_deref() == Some("0") {
        return;
    }
    let Some(path) = log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    lock(&file);
    let _ = writeln!(
        file,
        "{} pid={} {}",
        timestamp_millis(),
        std::process::id(),
        message.as_ref()
    );
    unlock(&file);
}

pub(crate) fn log_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CCTTY_LOG_FILE") {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }

    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    if cfg!(target_os = "macos") {
        return Some(
            home.join("Library")
                .join("Logs")
                .join("cctty")
                .join("cctty.log"),
        );
    }

    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| Some(home.join(".local").join("state")))
        .map(|base| base.join("cctty").join("cctty.log"))
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(unix)]
fn lock(file: &fs::File) {
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_EX);
    }
}

#[cfg(unix)]
fn unlock(file: &fs::File) {
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

#[cfg(not(unix))]
fn lock(_file: &fs::File) {}

#[cfg(not(unix))]
fn unlock(_file: &fs::File) {}
