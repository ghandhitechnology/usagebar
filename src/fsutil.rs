//! Small filesystem helpers shared by the config and credential files.

use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// Resolve a directory from an env override, then an XDG-style base, then a home default.
pub fn dir_from(env_key: &str, xdg_base: Option<&str>, default_under_home: &str) -> PathBuf {
    if let Ok(dir) = std::env::var(env_key) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(base) = xdg_base {
        if let Ok(root) = std::env::var(base) {
            if !root.is_empty() {
                return PathBuf::from(root).join(default_under_home);
            }
        }
    }
    home().join(default_under_home)
}

/// Write a file so a reader never sees a partial one, optionally with a fixed mode.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    let tmp = parent.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Some(mode) = mode {
        set_mode(&tmp, mode)?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

/// True when a file exists and only its owner can read it.
pub fn is_private(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(meta) => meta.permissions().mode() & 0o077 == 0,
            Err(_) => true,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_content_without_leaving_a_temp() {
        let dir = std::env::temp_dir().join(format!("usagebar-fs-{}", std::process::id()));
        let path = dir.join("nested/file.json");
        write_atomic(&path, b"{\"a\":1}", Some(0o600)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");
        write_atomic(&path, b"{\"a\":2}", Some(0o600)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}");
        assert!(is_private(&path));
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
