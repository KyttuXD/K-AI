use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub fn definitions() -> Value {
    json!([
        {"type":"function","function":{"name":"web_search","description":"Search the web for references. Requires the user to enable Web search. Send only a focused search query.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}},
        {"type":"function","function":{"name":"list_files","description":"List workspace files. Paths are relative to the workspace.","parameters":{"type":"object","properties":{},"required":[]}}},
        {"type":"function","function":{"name":"read_file","description":"Read a UTF-8 workspace file (up to 16000 characters).","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}},
        {"type":"function","function":{"name":"search_docs","description":"Search local documentation. Include specific API names and the technology in the query.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}},
        {"type":"function","function":{"name":"write_file","description":"Propose creating or replacing a UTF-8 workspace file. Read existing files first. The user must review and approve. Parent directory must exist.","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}},
        {"type":"function","function":{"name":"run_command","description":"Propose running a program in the workspace. Requires user approval. No shell expansion; pass program and argument array. Maximum runtime 60 seconds.","parameters":{"type":"object","properties":{"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}}},"required":["program","args"]}}}
    ])
}

pub fn resolve(root: &Path, name: &str) -> Result<PathBuf> {
    ensure!(!name.is_empty(), "A relative file path is required");
    let input = Path::new(name);
    ensure!(
        !input.is_absolute()
            && input
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
        "Only relative workspace paths are allowed"
    );
    // Reject Windows alternate data streams, device aliases, and shell-like path syntax.
    ensure!(
        !name.contains(':') && !name.contains('\0'),
        "Invalid file path"
    );
    #[cfg(windows)]
    for component in input.components() {
        let segment = component.as_os_str().to_string_lossy();
        let base = segment.split('.').next().unwrap_or("").to_ascii_uppercase();
        let numbered_device = base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.as_bytes()[3].is_ascii_digit();
        ensure!(
            !matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !numbered_device
                && (segment == "." || !segment.ends_with([' ', '.'])),
            "Reserved Windows filename"
        );
    }
    let root = root.canonicalize()?;
    let candidate = root.join(input);
    let resolved = if candidate.exists() {
        candidate.canonicalize()?
    } else {
        let parent = candidate
            .parent()
            .context("Missing parent")?
            .canonicalize()
            .context("Parent folder must exist")?;
        parent.join(candidate.file_name().context("Missing filename")?)
    };
    ensure!(
        resolved.starts_with(&root),
        "Path escapes workspace (possibly through a symlink)"
    );
    ensure!(
        !resolved
            .strip_prefix(&root)?
            .components()
            .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git")),
        "The .git directory is excluded"
    );
    Ok(resolved)
}

pub fn read(root: &Path, name: &str) -> Result<String> {
    let path = resolve(root, name)?;
    ensure!(
        std::fs::metadata(&path)?.len() <= 256_000,
        "File exceeds 256 KB read limit"
    );
    Ok(std::fs::read_to_string(path)?
        .chars()
        .take(16_000)
        .collect())
}

pub fn list(root: &Path) -> String {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(5)
        .into_iter()
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some(".git" | "target" | "node_modules" | ".venv")
            )
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .take(400)
        .map(|e| {
            e.path()
                .strip_prefix(root)
                .unwrap_or(e.path())
                .display()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn write(root: &Path, name: &str, expected: Option<&str>, content: &str) -> Result<String> {
    ensure!(content.len() <= 128_000, "Proposed file exceeds 128 KB");
    let path = resolve(root, name)?;
    let current = if path.exists() {
        Some(std::fs::read_to_string(&path)?)
    } else {
        None
    };
    ensure!(
        current.as_deref() == expected,
        "File changed since review. Please request a fresh proposal."
    );
    if let Some(old) = &current {
        let backup_dir = crate::config::data_dir().join("backups");
        std::fs::create_dir_all(&backup_dir)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        std::fs::write(backup_dir.join(format!("{stamp}.bak")), old)?;
    }
    atomic_replace(&path, expected, content)?;
    Ok(format!("Saved {}", path.display()))
}

fn atomic_replace(path: &Path, expected: Option<&str>, content: &str) -> Result<()> {
    use std::io::Write;
    let mut staged = tempfile::NamedTempFile::new_in(path.parent().context("Missing parent")?)?;
    staged.write_all(content.as_bytes())?;
    if let Ok(metadata) = std::fs::metadata(path) {
        staged.as_file().set_permissions(metadata.permissions())?;
    }
    staged.as_file().sync_all()?;
    let current = if path.exists() {
        Some(std::fs::read_to_string(path)?)
    } else {
        None
    };
    ensure!(
        current.as_deref() == expected,
        "File changed while preparing the write; request a fresh proposal"
    );
    staged
        .persist(path)
        .map_err(|e| anyhow::anyhow!("Could not commit file change: {}", e.error))?;
    Ok(())
}

pub fn command(root: &Path, program: &str, args: &[String]) -> Result<String> {
    ensure!(!program.is_empty(), "Program is required");
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().context("Could not start program")?;
    fn drain(mut pipe: impl Read + Send + 'static) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut saved = Vec::new();
            let mut buffer = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                let keep = n.min(16_000usize.saturating_sub(saved.len()));
                saved.extend_from_slice(&buffer[..keep]);
            }
            let _ = tx.send(saved);
        });
        rx
    }
    let out = drain(child.stdout.take().unwrap());
    let err = drain(child.stderr.take().unwrap());
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "Command exceeded 60 seconds and was stopped. Child processes may require manual cleanup."
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    // Do not wait on inherited descendant pipe handles after the main process exits.
    let stdout = out
        .recv_timeout(Duration::from_millis(250))
        .unwrap_or_default();
    let stderr = err
        .recv_timeout(Duration::from_millis(250))
        .unwrap_or_default();
    Ok(format!(
        "Exit: {status}\n{}\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atomic_write_does_not_modify_hardlinked_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let external = temp.path().join("outside.txt");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        std::fs::write(&external, "original")?;
        let linked = workspace.join("linked.txt");
        std::fs::hard_link(&external, &linked)?;
        atomic_replace(&linked, Some("original"), "changed")?;
        assert_eq!(std::fs::read_to_string(&external)?, "original");
        assert_eq!(std::fs::read_to_string(&linked)?, "changed");
        Ok(())
    }
    #[test]
    fn captures_fast_command_output() -> Result<()> {
        let output = command(
            &std::env::current_dir()?,
            std::env::current_exe()?.to_str().unwrap(),
            &["--list".into()],
        )?;
        assert!(output.contains("captures_fast_command_output"));
        Ok(())
    }
    #[test]
    fn blocks_escape_and_stale_writes() -> Result<()> {
        let root = std::env::temp_dir().join(format!("localai-tools-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        assert!(resolve(&root, "../escape.txt").is_err());
        assert!(resolve(&root, ".git/config").is_err());
        assert!(resolve(&root, "C:\\escape.txt").is_err());
        std::fs::write(root.join("sample.txt"), "original")?;
        assert!(write(&root, "sample.txt", Some("stale"), "new").is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("sample.txt"))?,
            "original"
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
