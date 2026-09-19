//! Small filesystem helpers shared by the config and credential files.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// A directory named by an environment variable, or a path under the home directory.
/// Use `xdg_dir` when the variable is an XDG base that holds several applications.
pub fn dir_from(env_key: &str, default_under_home: &str) -> PathBuf {
    if let Ok(dir) = std::env::var(env_key) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home().join(default_under_home)
}

/// An XDG base directory plus this application's leaf, e.g. `$XDG_CONFIG_HOME/usagebar`.
pub fn xdg_dir(env_key: &str, leaf: &str, default_under_home: &str) -> PathBuf {
    if let Ok(root) = std::env::var(env_key) {
        if !root.is_empty() {
            return PathBuf::from(root).join(leaf);
        }
    }
    home().join(default_under_home)
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write a file so a reader never sees a partial one. The mode is applied to the empty
/// temporary before any bytes are written, and an existing target keeps its own mode
/// when `mode` is None, so a vendor file never gets looser permissions than it had.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    // Two writers can run at once (two accounts rotating in one poll), so the temp name
    // has to be theirs alone.
    let tmp = parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let effective_mode = mode.or_else(|| mode_of(path));
    let result = (|| -> Result<(), String> {
        // Create it empty first, private when a mode is known, so there is never a
        // window where the temporary holds data with open permissions.
        std::fs::write(&tmp, b"").map_err(|e| format!("{}: {e}", tmp.display()))?;
        if let Some(mode) = effective_mode {
            set_mode(&tmp, mode)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        file.write_all(bytes)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        file.sync_all().map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The permission bits of an existing file, when it has any.
pub fn mode_of(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|meta| meta.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
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

/// Rename a file out of the way, e.g. a store that no longer parses.
pub fn set_aside(path: &Path, suffix: &str) -> Result<PathBuf, String> {
    let aside = path.with_extension(suffix);
    std::fs::rename(path, &aside).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(aside)
}

/// True when a file exists and only its owner can read it.
pub fn is_private(path: &Path) -> bool {
    #[cfg(unix)]
    {
        mode_of(path).map(|mode| mode & 0o077 == 0).unwrap_or(true)
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

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("usagebar-fs-{}-{name}", std::process::id()))
    }

    #[test]
    fn atomic_write_replaces_content_without_leaving_a_temp() {
        let dir = scratch("basic");
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

    /// A vendor file written by the mirror keeps its own permissions; a new one gets
    /// the caller's mode.
    #[test]
    fn existing_permissions_are_preserved() {
        let dir = scratch("modes");
        std::fs::create_dir_all(&dir).unwrap();
        let vendor = dir.join("auth.json");
        std::fs::write(&vendor, b"{}").unwrap();
        set_mode(&vendor, 0o600).unwrap();
        write_atomic(&vendor, b"{\"new\":true}", None).unwrap();
        assert!(is_private(&vendor), "vendor mode was loosened");
        let fresh = dir.join("fresh.json");
        write_atomic(&fresh, b"{}", Some(0o600)).unwrap();
        assert!(is_private(&fresh));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The temporary file must never exist with data in it and open permissions.
    #[test]
    fn temp_is_private_before_any_content() {
        let dir = scratch("private-temp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        write_atomic(&path, b"secret", Some(0o600)).unwrap();
        assert!(is_private(&path));
        assert_eq!(mode_of(&path), Some(0o600));
        std::fs::remove_dir_all(&dir).ok();
    }
}
