//! Soft delete: a file or folder is moved into `data_dir()/trash/<id>/` instead of being removed, so the UI can
//! offer Undo. `restore` moves it back, `commit` really deletes it, and `purge_old` (at startup) clears whatever a
//! crash or a forced exit left behind. Only paths inside the data folder are accepted, never anything outside.
use crate::config::data_dir;
use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Trash entries older than this are deleted at startup.
const MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// An item moved to the trash and not yet deleted for good.
#[derive(Debug)]
pub struct Staged {
    id: String,
    original: PathBuf,
    held: PathBuf,
}

fn trash_root() -> PathBuf {
    data_dir().join("trash")
}

/// Moves a file or folder into the trash. Only paths inside the data folder (library, models, presets, ...) are
/// accepted; symlinks, the data folder itself and the trash are refused.
pub fn stage(path: &Path) -> Result<Staged> {
    stage_in(path, &data_dir(), &trash_root())
}

/// Deletes trash entries older than an hour. Errors are ignored: the next start tries again.
pub fn purge_old() {
    purge_in(&trash_root(), MAX_AGE);
}

impl Staged {
    /// Moves the item back to where it was. Never overwrites something that has appeared there since.
    pub fn restore(self) -> Result<()> {
        self.restore_in(&trash_root())
    }

    /// Deletes the item for good.
    pub fn commit(self) -> Result<()> {
        self.commit_in(&trash_root())
    }

    fn entry_dir(&self, trash: &Path) -> Result<PathBuf> {
        let dir = trash.join(&self.id);
        if self.held.parent() != Some(dir.as_path()) {
            bail!("the held item is not inside the trash");
        }
        Ok(dir)
    }

    fn restore_in(self, trash: &Path) -> Result<()> {
        let dir = self.entry_dir(trash)?;
        if fs::symlink_metadata(&self.original).is_ok() {
            bail!("{} already exists", self.original.display());
        }
        if let Some(parent) = self.original.parent() {
            fs::create_dir_all(parent)?;
        }
        move_path(&self.held, &self.original)?;
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    fn commit_in(self, trash: &Path) -> Result<()> {
        let dir = self.entry_dir(trash)?;
        if dir.exists() {
            fs::remove_dir_all(&dir).with_context(|| format!("deleting {}", dir.display()))?;
        }
        Ok(())
    }
}

/// Whether `path` may be staged: it must exist, not be a symlink, lie strictly inside `base` and not inside `trash`.
/// Returns the canonical path.
fn check_allowed(path: &Path, base: &Path, trash: &Path) -> Result<PathBuf> {
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("{} not found", path.display()))?;
    if meta.file_type().is_symlink() {
        bail!("{} is a symbolic link", path.display());
    }
    let canon = path.canonicalize()?;
    let base = base.canonicalize()?;
    if canon == base || !canon.starts_with(&base) {
        bail!("{} is outside the data folder", path.display());
    }
    // The trash lies under the base; compare through the canonical base so `\\?\` prefixes cannot differ.
    let trash_rel = trash.file_name().unwrap_or_default();
    if canon.starts_with(base.join(trash_rel)) {
        bail!("{} is already in the trash", path.display());
    }
    Ok(canon)
}

fn stage_in(path: &Path, base: &Path, trash: &Path) -> Result<Staged> {
    let canon = check_allowed(path, base, trash)?;
    let name = canon.file_name().context("path has no name")?.to_owned();
    let id = new_id();
    let dir = trash.join(&id);
    fs::create_dir_all(&dir)?;
    let held = dir.join(name);
    if let Err(e) = move_path(&canon, &held) {
        let _ = fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok(Staged {
        id,
        original: canon,
        held,
    })
}

fn new_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!(
        "{ms}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Rename on the same volume, else copy then delete. A failed copy leaves the source untouched.
fn move_path(from: &Path, to: &Path) -> Result<()> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if let Err(e) = copy_all(from, to) {
        let _ = remove_path(to);
        return Err(e);
    }
    if let Err(e) = remove_path(from) {
        // Keep exactly one copy: the source stays and the duplicate goes.
        let _ = remove_path(to);
        return Err(e);
    }
    Ok(())
}

fn copy_all(from: &Path, to: &Path) -> Result<()> {
    if from.is_file() {
        fs::copy(from, to)?;
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(from).follow_links(false) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(from)?;
        let target = to.join(rel);
        let kind = entry.file_type();
        if kind.is_dir() {
            fs::create_dir_all(&target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)?;
        } else {
            bail!("{} is not a regular file", entry.path().display());
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Whether an entry of this age should be purged.
fn is_old(age: Duration, max_age: Duration) -> bool {
    age > max_age
}

fn purge_in(trash: &Path, max_age: Duration) {
    let Ok(entries) = fs::read_dir(trash) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        let age = now.duration_since(modified).unwrap_or_default();
        if is_old(age, max_age) {
            // Never follow a link out of the trash: only real folders and files are removed.
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        tmp: tempfile::TempDir,
        base: PathBuf,
        trash: PathBuf,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("data");
        let trash = base.join("trash");
        fs::create_dir_all(&base).unwrap();
        Env { tmp, base, trash }
    }

    #[test]
    fn stage_restore_file() {
        let e = env();
        let f = e.base.join("a.txt");
        fs::write(&f, "hi").unwrap();
        let s = stage_in(&f, &e.base, &e.trash).unwrap();
        assert!(!f.exists());
        assert!(s.held.is_file());
        s.restore_in(&e.trash).unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "hi");
        assert_eq!(fs::read_dir(&e.trash).unwrap().count(), 0);
    }

    #[test]
    fn stage_commit_folder() {
        let e = env();
        let d = e.base.join("lib").join("pack");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub").join("x"), "1").unwrap();
        let s = stage_in(&d, &e.base, &e.trash).unwrap();
        assert!(!d.exists());
        assert!(s.held.join("sub").join("x").is_file());
        s.commit_in(&e.trash).unwrap();
        assert_eq!(fs::read_dir(&e.trash).unwrap().count(), 0);
        assert!(!d.exists());
    }

    #[test]
    fn restore_never_overwrites() {
        let e = env();
        let f = e.base.join("a.txt");
        fs::write(&f, "old").unwrap();
        let s = stage_in(&f, &e.base, &e.trash).unwrap();
        fs::write(&f, "new").unwrap();
        assert!(s.restore_in(&e.trash).is_err());
        assert_eq!(fs::read_to_string(&f).unwrap(), "new");
    }

    #[test]
    fn refuses_outside_and_special_paths() {
        let e = env();
        let outside = e.tmp.path().join("outside.txt");
        fs::write(&outside, "x").unwrap();
        assert!(stage_in(&outside, &e.base, &e.trash).is_err());
        assert!(outside.exists());
        assert!(stage_in(&e.base, &e.base, &e.trash).is_err());
        assert!(stage_in(&e.base.join("missing"), &e.base, &e.trash).is_err());
        let f = e.base.join("a.txt");
        fs::write(&f, "x").unwrap();
        let s = stage_in(&f, &e.base, &e.trash).unwrap();
        assert!(stage_in(&s.held, &e.base, &e.trash).is_err());
        assert!(stage_in(&e.trash, &e.base, &e.trash).is_err());
        // `..` segments cannot escape the base.
        let sneaky = e.base.join("..").join("outside.txt");
        assert!(stage_in(&sneaky, &e.base, &e.trash).is_err());
        assert!(outside.exists());
    }

    #[test]
    fn commit_refuses_foreign_held_path() {
        let e = env();
        let f = e.base.join("keep.txt");
        fs::write(&f, "x").unwrap();
        let bad = Staged {
            id: "1".into(),
            original: f.clone(),
            held: f.clone(),
        };
        assert!(bad.commit_in(&e.trash).is_err());
        assert!(f.exists());
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(new_id(), new_id());
    }

    #[test]
    fn age_rule() {
        let h = Duration::from_secs(3600);
        assert!(!is_old(Duration::from_secs(10), h));
        assert!(!is_old(h, h));
        assert!(is_old(h + Duration::from_secs(1), h));
    }

    #[test]
    fn purge_removes_only_old_entries() {
        let e = env();
        let f = e.base.join("a.txt");
        fs::write(&f, "x").unwrap();
        let s = stage_in(&f, &e.base, &e.trash).unwrap();
        purge_in(&e.trash, Duration::from_secs(3600));
        assert!(s.held.exists());
        std::thread::sleep(Duration::from_millis(20));
        purge_in(&e.trash, Duration::ZERO);
        assert!(!s.held.exists());
    }
}
