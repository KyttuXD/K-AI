//! Sets KYTTU_COMMIT, the commit this build was made from, for the nightly update check
//! (src/core/update.rs reads it with option_env!). It comes from GITHUB_SHA in CI, else
//! from `git rev-parse HEAD`, and is empty when neither is available (a source archive).
use std::{fs, path::Path, process::Command};

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A full 40-digit commit id, or None.
fn commit_id(text: &str) -> Option<String> {
    let t = text.trim();
    (t.len() == 40 && t.bytes().all(|b| b.is_ascii_hexdigit())).then(|| t.to_ascii_lowercase())
}

/// Re-run when HEAD moves: HEAD itself (branch switches), the branch file it points at
/// (new commits) and packed-refs (after a gc).
fn watch_head() {
    let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) else {
        return;
    };
    if Path::new(&head).exists() {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Ok(text) = fs::read_to_string(&head)
        && let Some(reference) = text.trim().strip_prefix("ref:")
        && let Some(path) = git(&["rev-parse", "--git-path", reference.trim()])
        && Path::new(&path).exists()
    {
        println!("cargo:rerun-if-changed={path}");
    }
    if let Some(packed) = git(&["rev-parse", "--git-path", "packed-refs"])
        && Path::new(&packed).exists()
    {
        println!("cargo:rerun-if-changed={packed}");
    }
}

fn main() {
    let commit = std::env::var("GITHUB_SHA")
        .ok()
        .and_then(|s| commit_id(&s))
        .or_else(|| git(&["rev-parse", "HEAD"]).and_then(|s| commit_id(&s)))
        .unwrap_or_default();
    println!("cargo:rustc-env=KYTTU_COMMIT={commit}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    watch_head();
}
