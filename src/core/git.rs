//! Git helpers for change review and checkpoints, all through the git CLI with a scrubbed environment.
//! Nothing here commits to a branch, pushes or resets: checkpoints are dangling commits kept alive by
//! refs under `refs/kyttu/checkpoints/`, and restoring one only rewrites worktree files.

use super::tools;
use anyhow::{Context, Result, bail, ensure};
use std::{
    path::{Component, Path},
    process::{Output, Stdio},
};

const CHECKPOINT_REFS: &str = "refs/kyttu/checkpoints/";
/// Checkpoints kept per repository; older ones are dropped.
const KEEP_CHECKPOINTS: usize = 50;

#[derive(Clone, Debug, PartialEq)]
pub struct GitFile {
    pub path: String,
    /// Porcelain status letters: ' ' unchanged, M, A, D, R, C, U, '?' untracked.
    pub index: char,
    pub worktree: char,
}

fn run(root: &Path, args: &[&str]) -> Result<Output> {
    ensure!(root.is_dir(), "Workspace folder does not exist");
    let git = tools::resolve_program(root, "git").context("git is not installed or not on PATH")?;
    let mut command = tools::scrubbed(&git, root, &[]);
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        // Paths are plain file names, never pathspec magic or globs.
        .env("GIT_LITERAL_PATHSPECS", "1")
        .args(["-c", "core.fsmonitor=false", "-c", "core.quotepath=off"])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.output().context("Could not run git")
}

/// Runs git and returns stdout, failing with git's own message.
fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = run(root, args)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("git {}: {}", args.first().unwrap_or(&""), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Workspace-relative path checks shared by every command that takes paths.
fn check_path(path: &str) -> Result<()> {
    let p = Path::new(path);
    ensure!(
        !path.is_empty()
            && !path.contains('\0')
            && p.components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
            && !p
                .components()
                .any(|c| c.as_os_str().eq_ignore_ascii_case(".git")),
        "Invalid workspace path: {path}"
    );
    Ok(())
}

pub fn is_repo(root: &Path) -> bool {
    git(root, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|s| s.trim() == "true")
}

/// Changed and untracked files under `root`, with paths relative to `root`.
pub fn status(root: &Path) -> Result<Vec<GitFile>> {
    let prefix = git(root, &["rev-parse", "--show-prefix"])?;
    let raw = git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ],
    )?;
    Ok(parse_status(&raw)
        .into_iter()
        .filter_map(|mut f| {
            f.path = f.path.strip_prefix(prefix.trim_end())?.to_owned();
            Some(f)
        })
        .collect())
}

/// Parses `git status --porcelain=v1 -z`; a rename or copy is followed by its original path.
fn parse_status(raw: &str) -> Vec<GitFile> {
    let mut files = Vec::new();
    let mut entries = raw.split('\0').filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        let mut chars = entry.chars();
        let (Some(index), Some(worktree), Some(' ')) = (chars.next(), chars.next(), chars.next())
        else {
            continue;
        };
        if matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C') {
            entries.next();
        }
        files.push(GitFile {
            path: chars.as_str().to_owned(),
            index,
            worktree,
        });
    }
    files
}

/// Unified diff of the worktree against the index (or of the index against HEAD when `staged`).
/// An untracked file shows as fully added.
pub fn diff(root: &Path, path: Option<&str>, staged: bool) -> Result<String> {
    let mut args = vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv"];
    if staged {
        args.push("--cached");
    }
    let Some(path) = path else {
        return git(root, &args);
    };
    check_path(path)?;
    let tracked = run(root, &["ls-files", "--error-unmatch", "--", path])?
        .status
        .success();
    if staged || tracked {
        args.extend(["--", path]);
        return git(root, &args);
    }
    // `--no-index` exits with 1 when the files differ, which is the expected case here.
    args.extend(["--no-index", "--", "/dev/null", path]);
    let out = run(root, &args)?;
    ensure!(
        out.status.code().is_some_and(|c| c <= 1),
        "git diff: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn stage(root: &Path, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    paths.iter().try_for_each(|p| check_path(p))?;
    let mut args = vec!["add", "--"];
    args.extend(paths.iter().map(String::as_str));
    git(root, &args).map(drop)
}

pub fn unstage(root: &Path, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    paths.iter().try_for_each(|p| check_path(p))?;
    let has_head = run(root, &["rev-parse", "--verify", "--quiet", "HEAD"])?
        .status
        .success();
    let mut args = if has_head {
        vec!["restore", "--staged", "--"]
    } else {
        // Nothing committed yet: unstaging means dropping the files from the index.
        vec!["rm", "--cached", "-r", "--quiet", "--"]
    };
    args.extend(paths.iter().map(String::as_str));
    git(root, &args).map(drop)
}

/// Throws away worktree changes to one file (an untracked file is deleted), after backing it up.
pub fn discard(root: &Path, path: &str) -> Result<()> {
    discard_into(root, path, &crate::config::data_dir().join("backups"))
}

fn discard_into(root: &Path, path: &str, dir: &Path) -> Result<()> {
    check_path(path)?;
    let file = root.join(path);
    if file.is_file() {
        std::fs::create_dir_all(dir)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let name = file
            .file_name()
            .map_or("file".into(), |n| n.to_string_lossy());
        std::fs::copy(&file, dir.join(format!("{stamp}-{name}.bak")))
            .context("Could not back up the file; nothing was discarded")?;
    }
    let tracked = run(root, &["ls-files", "--error-unmatch", "--", path])?
        .status
        .success();
    if tracked {
        git(root, &["restore", "--worktree", "--", path]).map(drop)
    } else {
        ensure!(file.is_file(), "{path} is not a file");
        // Resolve again so a symlink cannot point the delete outside the workspace.
        std::fs::remove_file(tools::resolve(root, path)?).context("Could not delete the file")
    }
}

/// Snapshots tracked files (staged and unstaged changes) without touching the worktree or index,
/// and returns the checkpoint id. Untracked files are not included.
pub fn checkpoint(root: &Path, label: &str) -> Result<String> {
    let label = label.trim();
    let label = if label.is_empty() {
        "Checkpoint"
    } else {
        label
    };
    // `stash create` prints nothing when the worktree is clean; HEAD is then the snapshot.
    let stash = git(root, &["stash", "create", label])?;
    let source = match stash.trim() {
        "" => "HEAD".to_owned(),
        sha => sha.to_owned(),
    };
    let tree = format!("{source}^{{tree}}");
    // A fixed identity so a missing user.name/user.email never blocks a checkpoint.
    let commit = git(
        root,
        &[
            "-c",
            "user.name=KYTTU",
            "-c",
            "user.email=checkpoint@kyttu.invalid",
            "commit-tree",
            "--no-gpg-sign",
            &tree,
            "-p",
            "HEAD",
            "-m",
            label,
        ],
    )?;
    let existing = refs(root)?;
    let id = (existing.iter().map(|(n, _)| *n).max().unwrap_or(0) + 1).to_string();
    git(
        root,
        &[
            "update-ref",
            &format!("{CHECKPOINT_REFS}{id}"),
            commit.trim(),
        ],
    )?;
    let mut numbers: Vec<u64> = existing.iter().map(|(n, _)| *n).collect();
    numbers.sort_unstable();
    let drop_count = (numbers.len() + 1).saturating_sub(KEEP_CHECKPOINTS);
    for old in &numbers[..drop_count] {
        let _ = git(
            root,
            &["update-ref", "-d", &format!("{CHECKPOINT_REFS}{old}")],
        );
    }
    Ok(id)
}

/// Checkpoint numbers with their labels, in ref order.
fn refs(root: &Path) -> Result<Vec<(u64, String)>> {
    let raw = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(subject)",
            CHECKPOINT_REFS,
        ],
    )?;
    Ok(parse_refs(&raw))
}

fn parse_refs(raw: &str) -> Vec<(u64, String)> {
    raw.lines()
        .filter_map(|line| {
            let (name, subject) = line.split_once('\0')?;
            let n = name.strip_prefix(CHECKPOINT_REFS)?.parse().ok()?;
            Some((n, subject.to_owned()))
        })
        .collect()
}

/// (id, label) pairs, newest first.
pub fn checkpoints(root: &Path) -> Result<Vec<(String, String)>> {
    let mut list = refs(root)?;
    list.sort_unstable_by_key(|a| std::cmp::Reverse(a.0));
    Ok(list
        .into_iter()
        .map(|(n, label)| (n.to_string(), label))
        .collect())
}

/// Puts tracked worktree files back to a checkpoint (files added since are removed, untracked
/// files are left alone). The current state is saved as a new checkpoint first.
pub fn restore_checkpoint(root: &Path, id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()),
        "Invalid checkpoint id"
    );
    let target = git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{CHECKPOINT_REFS}{id}^{{commit}}"),
        ],
    )
    .with_context(|| format!("Checkpoint {id} not found"))?;
    checkpoint(root, &format!("Before restoring checkpoint {id}"))?;
    git(
        root,
        &[
            "restore",
            &format!("--source={}", target.trim()),
            "--worktree",
            "--",
            ".",
        ],
    )
    .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp repo with one commit, or None when git is unavailable.
    fn repo() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().ok()?;
        let root = dir.path();
        git(root, &["init", "-q"]).ok()?;
        std::fs::write(root.join("a.txt"), "one\n").ok()?;
        git(root, &["add", "a.txt"]).ok()?;
        git(
            root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        )
        .ok()?;
        Some(dir)
    }

    #[test]
    fn parses_porcelain_status() {
        let raw = " M src/a.rs\0A  new.rs\0R  to.rs\0from.rs\0?? x y.txt\0";
        let files = parse_status(raw);
        let short: Vec<_> = files
            .iter()
            .map(|f| (f.path.as_str(), f.index, f.worktree))
            .collect();
        assert_eq!(
            short,
            [
                ("src/a.rs", ' ', 'M'),
                ("new.rs", 'A', ' '),
                ("to.rs", 'R', ' '),
                ("x y.txt", '?', '?')
            ]
        );
    }

    #[test]
    fn parses_checkpoint_refs() {
        let raw = format!("{CHECKPOINT_REFS}2\0Before edit\n{CHECKPOINT_REFS}x\0bad\n");
        assert_eq!(parse_refs(&raw), [(2, "Before edit".to_owned())]);
    }

    #[test]
    fn rejects_unsafe_paths() {
        assert!(check_path("src/a.rs").is_ok());
        for bad in ["", "../x", ".git/config", "/etc/passwd", "a\0b"] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn status_stage_and_diff() -> Result<()> {
        let Some(dir) = repo() else { return Ok(()) };
        let root = dir.path();
        assert!(is_repo(root));
        std::fs::write(root.join("a.txt"), "two\n")?;
        std::fs::write(root.join("b.txt"), "new\n")?;
        let files = status(root)?;
        assert!(files.contains(&GitFile {
            path: "a.txt".into(),
            index: ' ',
            worktree: 'M'
        }));
        assert!(files.iter().any(|f| f.path == "b.txt" && f.index == '?'));
        assert!(diff(root, Some("a.txt"), false)?.contains("+two"));
        assert!(diff(root, Some("b.txt"), false)?.contains("+new"));
        stage(root, &["a.txt".into()])?;
        assert!(diff(root, None, true)?.contains("+two"));
        unstage(root, &["a.txt".into()])?;
        assert!(diff(root, None, true)?.is_empty());
        Ok(())
    }

    #[test]
    fn checkpoints_and_restore_leave_history_alone() -> Result<()> {
        let Some(dir) = repo() else { return Ok(()) };
        let root = dir.path();
        let head = git(root, &["rev-parse", "HEAD"])?;
        std::fs::write(root.join("a.txt"), "edited\n")?;
        let id = checkpoint(root, "Before agent changes")?;
        assert_eq!(std::fs::read_to_string(root.join("a.txt"))?, "edited\n");
        std::fs::write(root.join("a.txt"), "broken\n")?;
        restore_checkpoint(root, &id)?;
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt"))?.replace('\r', ""),
            "edited\n"
        );
        let list = checkpoints(root)?;
        assert_eq!(list.len(), 2);
        assert_eq!(list[1], (id, "Before agent changes".to_owned()));
        assert_eq!(git(root, &["rev-parse", "HEAD"])?, head);
        assert!(restore_checkpoint(root, "999").is_err());
        assert!(restore_checkpoint(root, "1; rm").is_err());
        Ok(())
    }

    #[test]
    fn discard_restores_tracked_and_deletes_untracked() -> Result<()> {
        let Some(dir) = repo() else { return Ok(()) };
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "changed\n")?;
        std::fs::write(root.join("junk.txt"), "x")?;
        let backups = tempfile::tempdir()?;
        discard_into(root, "a.txt", backups.path())?;
        discard_into(root, "junk.txt", backups.path())?;
        assert_eq!(std::fs::read_dir(backups.path())?.count(), 2);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt"))?.replace('\r', ""),
            "one\n"
        );
        assert!(!root.join("junk.txt").exists());
        Ok(())
    }
}
