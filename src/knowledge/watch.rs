//! Watches enabled documentation sources and rebuilds a changed source (incrementally)
//! once its files have been quiet for two seconds. Dropping the watcher stops it.
use super::docs::{self, DocSource, Library};
use anyhow::Result;
use notify::{EventKind, RecursiveMode, Watcher};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

const QUIET: Duration = Duration::from_secs(2);

pub struct DocWatcher {
    _watcher: notify::RecommendedWatcher,
}

/// True for events that can change the index: a supported file, or a folder that may hold some.
fn relevant(path: &Path) -> bool {
    !path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some(".git" | "node_modules" | "target" | ".venv")
        )
    }) && (docs::supported(path) || path.extension().is_none())
}

impl DocWatcher {
    pub fn start(sources: &[DocSource], library: Library) -> Result<Self> {
        let roots: Vec<(PathBuf, DocSource)> = sources
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| Some((s.path.canonicalize().ok()?, s.clone())))
            .collect();
        let (send, recv) = mpsc::channel::<PathBuf>();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else { return };
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                for path in event.paths {
                    let _ = send.send(path);
                }
            })?;
        for (root, _) in &roots {
            let mode = if root.is_dir() {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            watcher.watch(root, mode)?;
        }
        std::thread::spawn(move || {
            let (mut changed, mut last) = (HashSet::<usize>::new(), Instant::now());
            loop {
                match recv.recv_timeout(QUIET) {
                    Ok(path) => {
                        if relevant(&path)
                            && let Some(i) =
                                roots.iter().position(|(root, _)| path.starts_with(root))
                        {
                            changed.insert(i);
                            last = Instant::now();
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                // Busy unrelated files must not hold back the rebuild of a changed source.
                if !changed.is_empty() && last.elapsed() >= QUIET {
                    library.rebuild_sources(changed.drain().map(|i| roots[i].1.clone()).collect());
                }
            }
        });
        Ok(Self { _watcher: watcher })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ignores_unsupported_and_build_folders() {
        assert!(relevant(Path::new("docs/guide.md")));
        assert!(relevant(Path::new("docs/new-folder")));
        assert!(!relevant(Path::new("docs/image.png")));
        assert!(!relevant(Path::new("docs/.git/index.md")));
    }
}
