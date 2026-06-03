use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::{CcttyError, Result};
use crate::logging;

pub(super) fn resolve_claude_path() -> Result<String> {
    let own_exe = std::env::current_exe().ok();
    let current_dir = std::env::current_dir().ok();
    resolve_claude_path_from(
        own_exe.as_deref(),
        current_dir.as_deref(),
        std::env::var_os("CCTTY_CLAUDE_PATH"),
        std::env::var_os("CONDUCTOR_AGENT_BINARIES_DIR"),
        std::env::var_os("PATH"),
    )
}

fn resolve_claude_path_from(
    own_exe: Option<&Path>,
    current_dir: Option<&Path>,
    explicit_path: Option<OsString>,
    conductor_agent_binaries_dir: Option<OsString>,
    path_env: Option<OsString>,
) -> Result<String> {
    let own_exe = own_exe.and_then(canonical_path);
    if let Some(path) = explicit_path
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return usable_claude_candidate(&path, own_exe.as_deref()).ok_or_else(|| {
            CcttyError::ClaudeNotFound(format!(
                "CCTTY_CLAUDE_PATH points to unusable Claude binary: {}",
                path.display()
            ))
        });
    }

    for candidate in current_dir_claude_candidates(current_dir) {
        if let Some(path) = usable_claude_candidate(&candidate, own_exe.as_deref()) {
            logging::event(format!("claude_resolved source=current_dir path={path}"));
            return Ok(path);
        }
    }

    for candidate in relative_claude_candidates(own_exe.as_deref()) {
        if let Some(path) = usable_claude_candidate(&candidate, own_exe.as_deref()) {
            logging::event(format!("claude_resolved source=relative path={path}"));
            return Ok(path);
        }
    }

    for candidate in conductor_agent_binary_candidates(conductor_agent_binaries_dir) {
        if let Some(path) = usable_claude_candidate(&candidate, own_exe.as_deref()) {
            logging::event(format!("claude_resolved source=conductor path={path}"));
            return Ok(path);
        }
    }

    for candidate in path_claude_candidates(path_env) {
        if let Some(path) = usable_claude_candidate(&candidate, own_exe.as_deref()) {
            logging::event(format!("claude_resolved source=path path={path}"));
            return Ok(path);
        }
    }

    Err(CcttyError::ClaudeNotFound(
        "no usable real Claude binary found via CCTTY_CLAUDE_PATH, current directory, cctty-relative paths, Conductor agent-binaries, or PATH"
            .to_owned(),
    ))
}

fn current_dir_claude_candidates(current_dir: Option<&Path>) -> Vec<PathBuf> {
    let Some(dir) = current_dir else {
        return Vec::new();
    };
    ["claude.real", "claude.orig", "claude-code", "claude"]
        .into_iter()
        .map(|name| dir.join(name))
        .collect()
}

fn relative_claude_candidates(own_exe: Option<&Path>) -> Vec<PathBuf> {
    let Some(dir) = own_exe.and_then(Path::parent) else {
        return Vec::new();
    };
    [
        "claude.real",
        "claude.orig",
        "claude-code",
        "claude",
        "../claude/claude",
        "../claude-code/claude",
        "../vendor/claude-code/claude",
    ]
    .into_iter()
    .map(|name| dir.join(name))
    .collect()
}

fn conductor_agent_binary_candidates(dir: Option<OsString>) -> Vec<PathBuf> {
    let Some(root) = dir.map(PathBuf::from).filter(|path| path.is_dir()) else {
        return Vec::new();
    };
    let claude_root = root.join("claude");
    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&claude_root) {
        for entry in entries.flatten() {
            candidates.push(entry.path().join("claude"));
        }
    }
    candidates.sort_by(|left, right| right.cmp(left));
    candidates
}

fn path_claude_candidates(path_env: Option<OsString>) -> Vec<PathBuf> {
    path_env
        .map(|value| {
            std::env::split_paths(&value)
                .map(|dir| dir.join("claude"))
                .collect()
        })
        .unwrap_or_default()
}

fn usable_claude_candidate(path: &Path, own_exe: Option<&Path>) -> Option<String> {
    let canonical = canonical_path(path)?;
    if own_exe.is_some_and(|own| own == canonical) {
        return None;
    }
    is_executable_file(&canonical).then(|| canonical.to_string_lossy().to_string())
}

fn canonical_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_real_claude_from_current_directory_before_path() {
        let current = tempfile::tempdir().unwrap();
        let path_dir = tempfile::tempdir().unwrap();
        let own = current.path().join("cctty");
        let current_claude = current.path().join("claude");
        let path_claude = path_dir.path().join("claude");
        write_executable(&own);
        write_executable(&current_claude);
        write_executable(&path_claude);

        let resolved = resolve_claude_path_from(
            Some(&own),
            Some(current.path()),
            None,
            None,
            Some(OsString::from(path_dir.path().as_os_str())),
        )
        .unwrap();

        assert_eq!(
            resolved,
            std::fs::canonicalize(current_claude)
                .unwrap()
                .to_string_lossy()
        );
    }

    #[test]
    fn skips_cctty_itself_when_resolving_path_claude() {
        let current = tempfile::tempdir().unwrap();
        let path_dir = tempfile::tempdir().unwrap();
        let own = current.path().join("claude");
        let path_claude = path_dir.path().join("claude");
        write_executable(&own);
        write_executable(&path_claude);
        let path_env = std::env::join_paths([current.path(), path_dir.path()]).unwrap();

        let resolved =
            resolve_claude_path_from(Some(&own), Some(current.path()), None, None, Some(path_env))
                .unwrap();

        assert_eq!(
            resolved,
            std::fs::canonicalize(path_claude)
                .unwrap()
                .to_string_lossy()
        );
    }

    fn write_executable(path: &Path) {
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
    }
}
