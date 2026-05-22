use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::{CcttyError, Result};

const PTY_OUTPUT_BUFFER_LIMIT: usize = 64 * 1024;

pub struct PtySpawnSpec {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub unset_env: Vec<String>,
}

pub struct PtyProcess {
    pid: libc::pid_t,
    writer: File,
    output: Arc<Mutex<Vec<u8>>>,
    _reader: std::thread::JoinHandle<()>,
}

impl PtyProcess {
    pub fn spawn(spec: &PtySpawnSpec) -> Result<Self> {
        spawn_unix_pty(spec)
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    pub fn interrupt(&mut self) -> io::Result<()> {
        self.write_all(b"\x03")
    }

    pub fn recent_output(&self) -> String {
        self.output
            .lock()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }

    pub fn kill(&mut self) {
        if self.pid <= 0 {
            return;
        }
        unsafe {
            libc::kill(-self.pid, libc::SIGTERM);
            libc::kill(self.pid, libc::SIGTERM);
        }
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

fn spawn_unix_pty(spec: &PtySpawnSpec) -> Result<PtyProcess> {
    let command = CString::new(spec.command.as_str())
        .map_err(|_| CcttyError::Tty("claude command contains NUL byte".to_owned()))?;
    let mut c_args = Vec::with_capacity(spec.args.len() + 1);
    c_args.push(command.clone());
    for arg in &spec.args {
        c_args.push(
            CString::new(arg.as_str())
                .map_err(|_| CcttyError::Tty("claude argument contains NUL byte".to_owned()))?,
        );
    }
    let mut argv = c_args.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
    argv.push(std::ptr::null());

    let cwd = CString::new(path_bytes(&spec.cwd))
        .map_err(|_| CcttyError::Tty("cwd contains NUL byte".to_owned()))?;
    let env = spec
        .env
        .iter()
        .map(|(key, value)| {
            Ok((
                CString::new(key.as_str())
                    .map_err(|_| CcttyError::Tty("environment key contains NUL byte".to_owned()))?,
                CString::new(value.as_str()).map_err(|_| {
                    CcttyError::Tty("environment value contains NUL byte".to_owned())
                })?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let unset_env = spec
        .unset_env
        .iter()
        .map(|key| {
            CString::new(key.as_str())
                .map_err(|_| CcttyError::Tty("environment key contains NUL byte".to_owned()))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut master_fd: libc::c_int = -1;
    let mut winsize = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let pid = unsafe {
        libc::forkpty(
            &mut master_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut winsize,
        )
    };
    if pid < 0 {
        return Err(CcttyError::Tty(format!(
            "forkpty failed: {}",
            io::Error::last_os_error()
        )));
    }
    if pid == 0 {
        unsafe {
            libc::chdir(cwd.as_ptr());
            libc::unsetenv(c"CLAUDECODE".as_ptr());
            for key in &unset_env {
                libc::unsetenv(key.as_ptr());
            }
            for (key, value) in &env {
                libc::setenv(key.as_ptr(), value.as_ptr(), 1);
            }
            libc::execvp(command.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
    }

    let master = unsafe { File::from_raw_fd(master_fd) };
    let mut reader = master
        .try_clone()
        .map_err(|error| CcttyError::Tty(format!("failed to clone pty master: {error}")))?;
    let output = Arc::new(Mutex::new(Vec::new()));
    let reader_output = output.clone();
    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(len) => {
                    if let Ok(mut output) = reader_output.lock() {
                        output.extend_from_slice(&buf[..len]);
                        if output.len() > PTY_OUTPUT_BUFFER_LIMIT {
                            let drain_len = output.len() - PTY_OUTPUT_BUFFER_LIMIT;
                            output.drain(..drain_len);
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    Ok(PtyProcess {
        pid,
        writer: master,
        output,
        _reader: reader_thread,
    })
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}
