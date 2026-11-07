use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config;

const MAX_BACKUPS: usize = 100;

/// Save a backup of an ODT under ~/.cache/scribe/backups/.
///
/// Each file gets its own directory, keeping the last 100 distinct versions.
pub fn save_backup(identity: &str, display: &str, bytes: &[u8]) -> io::Result<()> {
    let cache =
        config::cache_dir().map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    save_backup_in(&cache, identity, display, bytes)
}

fn save_backup_in(cache: &Path, identity: &str, display: &str, bytes: &[u8]) -> io::Result<()> {
    let dir = backup_dir_in(cache, identity, display);
    fs::create_dir_all(&dir)?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));

    let mut files = list_backups(&dir);
    if let Some(newest) = files.last() {
        if let Ok(existing) = fs::read(newest) {
            if existing == bytes {
                return Ok(());
            }
        }
    }

    let name = next_name(&dir);
    atomic_write(&dir.join(name), bytes)?;

    files = list_backups(&dir);
    if files.len() > MAX_BACKUPS {
        for old in &files[..files.len() - MAX_BACKUPS] {
            let _ = fs::remove_file(old);
        }
    }
    Ok(())
}

fn backup_dir_in(cache: &Path, identity: &str, display: &str) -> PathBuf {
    let stem = Path::new(display)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(display);
    let safe = safe_name(stem);
    let hash = fnv1a(identity);
    cache.join("backups").join(format!("{safe}-{hash:08x}"))
}

fn safe_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "book".to_string()
    } else {
        out
    }
}

fn fnv1a(value: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in value.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

fn list_backups(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension()
                            .and_then(|s| s.to_str())
                            .map(|s| s.eq_ignore_ascii_case("odt"))
                            .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    files
}

fn next_name(dir: &Path) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    for counter in 0..1000 {
        let name = format!("{millis:013}-{counter:03}.odt");
        if !dir.join(&name).exists() {
            return name;
        }
    }
    format!("{millis:013}-999.odt")
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    let _ = fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o600));
    tmp.persist(path)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_last_100_and_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..110 {
            let data = format!("version {i}");
            save_backup_in(tmp.path(), "book-id", "My Book.odt", data.as_bytes()).unwrap();
        }
        let dir = backup_dir_in(tmp.path(), "book-id", "My Book.odt");
        let files = list_backups(&dir);
        assert_eq!(files.len(), MAX_BACKUPS);
        let newest = fs::read(files.last().unwrap()).unwrap();
        save_backup_in(tmp.path(), "book-id", "My Book.odt", &newest).unwrap();
        assert_eq!(list_backups(&dir).len(), MAX_BACKUPS);
    }
}
