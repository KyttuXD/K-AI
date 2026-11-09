use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

/// Which optional tools the model is offered for this request.
pub struct ToolSet {
    pub docs: bool,
    pub web: bool,
}

/// Directories never searched or listed.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "target-pass",
    "node_modules",
    "dist",
    ".venv",
];
/// Characters of file text returned by one read_file call.
const READ_BUDGET: usize = 16_000;
/// Bytes kept from the start and from the end of each command stream.
const STREAM_HEAD: usize = 6_000;
const STREAM_TAIL: usize = 6_000;
/// Environment variables passed through to approved commands: what shells, package managers,
/// compilers and git helpers need, without the rest (which may hold secrets).
const PASS_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "TEMP",
    "TMP",
    "SystemRoot",
    "windir",
    "LANG",
    "LC_ALL",
    "CARGO_HOME",
    "RUSTUP_HOME",
    // Windows
    "PATHEXT",
    "ComSpec",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "CommonProgramFiles",
    "SystemDrive",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    "USERNAME",
    // Unix
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
];

pub fn definitions(set: &ToolSet) -> Value {
    let mut tools = vec![
        json!({"type":"function","function":{"name":"list_files","description":"List workspace files. Paths are relative to the workspace.","parameters":{"type":"object","properties":{},"required":[]}}}),
        json!({"type":"function","function":{"name":"read_file","description":"Read a UTF-8 workspace file. Output lines are prefixed with their line number and a tab (not part of the file). Use start_line/end_line to read a range; a note says when output was truncated.","parameters":{"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer","description":"First line to read (1-based)"},"end_line":{"type":"integer","description":"Last line to read (inclusive)"}},"required":["path"]}}}),
        json!({"type":"function","function":{"name":"search_code","description":"Search workspace files for literal text (not a regex). Returns path:line: text matches. Use it to find where a symbol is defined or used before reading files.","parameters":{"type":"object","properties":{"query":{"type":"string"},"case_sensitive":{"type":"boolean","description":"Default false"}},"required":["query"]}}}),
    ];
    if set.docs {
        tools.push(json!({"type":"function","function":{"name":"search_docs","description":"Search local documentation. Include specific API names and the technology in the query.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}}));
    }
    if set.web {
        tools.push(json!({"type":"function","function":{"name":"web_search","description":"Search the web for references. Send only a focused search query.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}}));
    }
    tools.extend([
        json!({"type":"function","function":{"name":"edit_file","description":"Propose replacing old_text with new_text in an existing workspace file. old_text must match the file exactly once (copy it without line numbers, include enough surrounding lines to be unique). The user must review and approve.","parameters":{"type":"object","properties":{"path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"}},"required":["path","old_text","new_text"]}}}),
        json!({"type":"function","function":{"name":"write_file","description":"Propose creating or replacing a whole UTF-8 workspace file. Prefer edit_file for small changes. Read existing files first. The user must review and approve. Parent directory must exist.","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}}),
        json!({"type":"function","function":{"name":"run_command","description":"Propose running a program in the workspace. Requires user approval. No shell expansion; pass program and argument array. Maximum runtime 60 seconds.","parameters":{"type":"object","properties":{"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}}},"required":["program","args"]}}}),
    ]);
    Value::Array(tools)
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
        .take(READ_BUDGET)
        .collect())
}

/// Reads a line range with numbered lines; ends with a note when only part of the file is shown.
pub fn read_range(
    root: &Path,
    name: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<String> {
    let path = resolve(root, name)?;
    ensure!(path.is_file(), "Not a file: {name}");
    ensure!(
        std::fs::metadata(&path)?.len() <= 256_000,
        "File exceeds 256 KB read limit; use search_code to find the relevant lines"
    );
    let text = std::fs::read_to_string(&path).context("File is not UTF-8 text")?;
    number_lines(&text, start_line, end_line, READ_BUDGET)
}

fn number_lines(
    text: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    budget: usize,
) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return Ok("(empty file)".into());
    }
    let start = start_line.unwrap_or(1).max(1);
    ensure!(
        start <= total,
        "start_line {start} is past the end of the file ({total} lines)"
    );
    let end = end_line.unwrap_or(total).min(total);
    ensure!(end >= start, "end_line {end} is before start_line {start}");
    let mut out = String::new();
    let mut used = 0;
    let mut last = start - 1;
    let mut cut_line = false;
    for (i, line) in lines[start - 1..end].iter().enumerate() {
        let number = start + i;
        let entry = format!("{number}\t{line}\n");
        let size = entry.chars().count();
        if used + size > budget {
            if last < start {
                // A single huge line: show what fits of it.
                out.extend(entry.chars().take(budget));
                out.push('\n');
                last = number;
                cut_line = true;
            }
            break;
        }
        out.push_str(&entry);
        used += size;
        last = number;
    }
    if cut_line {
        out.push_str(&format!(
            "[truncated: line {last} is longer than {budget} characters; lines {start}-{last} of {total}]"
        ));
    } else if last < end {
        out.push_str(&format!(
            "[truncated: lines {start}-{last} of {total}; read again with start_line={} for more]",
            last + 1
        ));
    } else if start > 1 || end < total {
        out.push_str(&format!("[lines {start}-{end} of {total}]"));
    }
    Ok(out)
}

/// Literal text search over workspace text files.
pub fn search_code(root: &Path, query: &str, case_sensitive: bool, max: usize) -> Result<String> {
    ensure!(!query.trim().is_empty(), "A search query is required");
    ensure!(query.len() <= 500, "Search query is too long");
    ensure!(root.is_dir(), "Workspace folder does not exist");
    let max = max.clamp(1, 500);
    let needle = if case_sensitive {
        query.to_owned()
    } else {
        query.to_lowercase()
    };
    let mut found = Vec::new();
    let mut more = 0usize;
    let files = walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(16)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !e
                    .file_name()
                    .to_str()
                    .is_some_and(|n| SKIP_DIRS.contains(&n))
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.metadata().is_ok_and(|m| m.len() <= 1_000_000))
        .take(20_000);
    for entry in files {
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if bytes[..bytes.len().min(8_000)].contains(&0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let shown = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .display()
            .to_string();
        for (i, line) in text.lines().enumerate() {
            let hit = if case_sensitive {
                line.contains(&needle)
            } else {
                line.to_lowercase().contains(&needle)
            };
            if !hit {
                continue;
            }
            if found.len() < max {
                let trimmed = line.trim();
                let mut snippet: String = trimmed.chars().take(200).collect();
                if snippet.len() < trimmed.len() {
                    snippet.push_str("...");
                }
                found.push(format!("{shown}:{}: {snippet}", i + 1));
            } else {
                more += 1;
            }
        }
    }
    if found.is_empty() {
        return Ok(format!("No matches for \"{query}\"."));
    }
    let mut out = found.join("\n");
    if more > 0 {
        out.push_str(&format!(
            "\n[{more} more matches not shown; use a more specific query]"
        ));
    }
    Ok(out)
}

/// Prepares a find-and-replace edit; returns (current, updated) for review and `write`.
pub fn plan_edit(
    root: &Path,
    name: &str,
    old_text: &str,
    new_text: &str,
) -> Result<(String, String)> {
    ensure!(!old_text.is_empty(), "old_text must not be empty");
    let path = resolve(root, name)?;
    ensure!(
        path.is_file(),
        "{name} does not exist; use write_file to create it"
    );
    ensure!(
        std::fs::metadata(&path)?.len() <= 128_000,
        "Existing file exceeds review limit"
    );
    let current = std::fs::read_to_string(&path).context("File is not UTF-8 text")?;
    let updated = replace_once(&current, old_text, new_text)?;
    ensure!(updated.len() <= 128_000, "Edited file exceeds 128 KB");
    Ok((current, updated))
}

fn replace_once(current: &str, old_text: &str, new_text: &str) -> Result<String> {
    // Accept LF-only old_text against a CRLF file, keeping the file's line endings.
    let crlf = current.contains("\r\n") && !old_text.contains("\r\n");
    let (old, new) = if crlf {
        (
            old_text.replace('\n', "\r\n"),
            new_text.replace('\n', "\r\n"),
        )
    } else {
        (old_text.to_owned(), new_text.to_owned())
    };
    match current.matches(old.as_str()).count() {
        1 => Ok(current.replacen(old.as_str(), &new, 1)),
        0 => bail!(
            "old_text was not found. Read the file again and copy the text exactly, without line numbers."
        ),
        n => bail!(
            "old_text matches {n} places. Include more surrounding lines so it matches exactly once."
        ),
    }
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

/// Finds the executable a command would run: a path (relative to the workspace) or a PATH lookup.
pub fn resolve_program(root: &Path, program: &str) -> Option<PathBuf> {
    let given = Path::new(program);
    if program.is_empty() || program.contains('\0') {
        return None;
    }
    let dirs: Vec<PathBuf> = if given.is_absolute() {
        vec![PathBuf::new()]
    } else if given.components().count() > 1 {
        vec![root.to_path_buf()]
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?).collect()
    };
    let names = executable_names(program);
    dirs.iter()
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| is_executable(path))
}

fn executable_names(program: &str) -> Vec<String> {
    #[cfg(windows)]
    if Path::new(program).extension().is_none() {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        return exts
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| format!("{program}{}", e.to_ascii_lowercase()))
            .collect();
    }
    vec![program.to_owned()]
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// True for shells and script interpreters, which can run arbitrary code from their arguments.
pub fn is_interpreter(program: &str) -> bool {
    let name = Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    let name = [".exe", ".com", ".bat", ".cmd"]
        .iter()
        .find_map(|ext| name.strip_suffix(ext))
        .unwrap_or(name.as_str());
    const EXACT: &[&str] = &[
        "cmd",
        "command",
        "powershell",
        "powershell_ise",
        "pwsh",
        "bash",
        "sh",
        "zsh",
        "fish",
        "dash",
        "ksh",
        "csh",
        "tcsh",
        "nu",
        "wsl",
        "py",
        "pyw",
        "node",
        "deno",
        "bun",
        "perl",
        "ruby",
        "irb",
        "php",
        "lua",
        "luajit",
        "tclsh",
        "wish",
        "rscript",
        "osascript",
        "wscript",
        "cscript",
        "mshta",
        "rundll32",
        "regsvr32",
        "env",
        "sudo",
        "doas",
        "xargs",
        "busybox",
        "awk",
        "gawk",
        "java",
        "jshell",
        "dotnet",
    ];
    EXACT.contains(&name)
        || ["python", "pythonw", "perl", "ruby", "php", "lua"]
            .iter()
            .any(|p| {
                name.strip_prefix(p)
                    .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit() || c == '.'))
            })
}

/// Bytes captured from one output stream: the start, the end, and the total length.
#[derive(Default)]
struct Captured {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
}

impl Captured {
    fn push(&mut self, mut chunk: &[u8]) {
        self.total += chunk.len();
        let room = STREAM_HEAD.saturating_sub(self.head.len()).min(chunk.len());
        self.head.extend_from_slice(&chunk[..room]);
        chunk = &chunk[room..];
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > STREAM_TAIL * 2 {
            self.tail.drain(..self.tail.len() - STREAM_TAIL);
        }
    }

    fn render(&self, label: &str) -> String {
        if self.total == 0 {
            return format!("{label}: (empty)");
        }
        let tail = &self.tail[self.tail.len().saturating_sub(STREAM_TAIL)..];
        let omitted = self.total - self.head.len() - tail.len();
        let mut out = format!("{label}:\n{}", String::from_utf8_lossy(&self.head));
        if omitted > 0 {
            out.push_str(&format!("\n[... {omitted} bytes omitted ...]\n"));
        }
        out.push_str(&String::from_utf8_lossy(tail));
        out.truncate(out.trim_end().len());
        out
    }
}

fn summary(status: &ExitStatus) -> String {
    let verdict = if status.success() { "passed" } else { "failed" };
    match status.code() {
        Some(code) => format!("Exit code {code}: {verdict}"),
        None => format!("Terminated without an exit code ({status}): {verdict}"),
    }
}

/// True for a plain environment variable name a workspace may allow through (item 61).
pub fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '(' | ')'))
}

/// A command for `program` in `root` with stdin closed, no console window on Windows, and an
/// environment of only PASS_ENV plus the allowed `extra_env` names that are set.
pub(crate) fn scrubbed(program: &Path, root: &Path, extra_env: &[String]) -> Command {
    let mut command = Command::new(program);
    command.current_dir(root).env_clear().stdin(Stdio::null());
    let mut pass = |key: &str| {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    };
    PASS_ENV.iter().for_each(|k| pass(k));
    extra_env
        .iter()
        .map(|k| k.trim())
        .filter(|k| valid_env_name(k))
        .for_each(pass);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

/// Lists workspace files with `/` separators, sorted. In a git repo this honours .gitignore
/// (tracked plus untracked, not ignored, files); otherwise it walks the folder skipping build dirs.
pub fn list_ignored_aware(root: &Path) -> Vec<String> {
    const MAX: usize = 20_000;
    let mut files: Vec<String> = git_files(root).unwrap_or_else(|| {
        walkdir::WalkDir::new(root)
            .follow_links(false)
            .max_depth(16)
            .into_iter()
            .filter_entry(|e| {
                e.depth() == 0
                    || !e
                        .file_name()
                        .to_str()
                        .is_some_and(|n| SKIP_DIRS.contains(&n))
            })
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .take(MAX)
            .filter_map(|e| {
                let rel = e.path().strip_prefix(root).ok()?;
                Some(rel.to_string_lossy().replace('\\', "/"))
            })
            .collect()
    });
    files.truncate(MAX);
    files.sort_unstable();
    files
}

fn git_files(root: &Path) -> Option<Vec<String>> {
    if !super::git::is_repo(root) {
        return None;
    }
    let git = resolve_program(root, "git")?;
    let mut command = scrubbed(&git, root, &[]);
    let out = command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["-c", "core.fsmonitor=false", "ls-files", "-z"])
        .args(["--cached", "--others", "--exclude-standard", "--", "."])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut files: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split('\0')
        // Tracked files deleted from the worktree are still listed by --cached.
        .filter(|p| !p.is_empty() && root.join(p).is_file())
        .map(str::to_owned)
        .collect();
    files.dedup();
    Some(files)
}

/// Checkpoints a git workspace before the first approved write of a turn (item 1).
/// Returns the checkpoint id, or None when it is not a repo.
pub fn checkpoint_before_write(root: &Path) -> Result<Option<String>> {
    if !super::git::is_repo(root) {
        return Ok(None);
    }
    super::git::checkpoint(root, "Before agent changes").map(Some)
}

#[cfg(test)]
pub fn command(root: &Path, program: &str, args: &[String]) -> Result<String> {
    command_with_env(root, program, args, &[])
}

/// Runs an approved command; `extra_env` names variables passed through on top of PASS_ENV.
/// The program and everything it starts are stopped on timeout.
pub fn command_with_env(
    root: &Path,
    program: &str,
    args: &[String],
    extra_env: &[String],
) -> Result<String> {
    ensure!(!program.is_empty(), "Program is required");
    ensure!(root.is_dir(), "Workspace folder does not exist");
    let resolved = resolve_program(root, program)
        .with_context(|| format!("Program not found on PATH or in the workspace: {program}"))?;
    let mut command = scrubbed(&resolved, root, extra_env);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    super::terminal::Tree::prepare(&mut command);
    let mut child = command.spawn().context("Could not start program")?;
    // On Windows, dropping the tree at the end of this call also ends leftover descendants.
    let tree = super::terminal::Tree::attach(&child);
    fn drain(mut pipe: impl Read + Send + 'static) -> std::sync::mpsc::Receiver<Captured> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut saved = Captured::default();
            let mut buffer = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                saved.push(&buffer[..n]);
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
            tree.kill();
            let _ = child.kill();
            let _ = child.wait();
            bail!("Command exceeded 60 seconds and was stopped with its child processes.");
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
        "{}\n{}\n{}",
        summary(&status),
        stdout.render("stdout"),
        stderr.render("stderr")
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
        assert!(output.starts_with("Exit code 0: passed"));
        assert!(output.contains("stdout:"));
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
    #[test]
    fn definitions_follow_tool_set() {
        let names = |set: ToolSet| -> Vec<String> {
            definitions(&set)
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
                .collect()
        };
        let plain = names(ToolSet {
            docs: false,
            web: false,
        });
        assert!(plain.contains(&"search_code".into()) && plain.contains(&"edit_file".into()));
        assert!(!plain.contains(&"search_docs".into()) && !plain.contains(&"web_search".into()));
        let all = names(ToolSet {
            docs: true,
            web: true,
        });
        assert!(all.contains(&"search_docs".into()) && all.contains(&"web_search".into()));
    }
    #[test]
    fn numbers_lines_and_reports_truncation() -> Result<()> {
        let text = "a\nb\nc\nd\n";
        assert_eq!(
            number_lines(text, None, None, 1000)?,
            "1\ta\n2\tb\n3\tc\n4\td\n"
        );
        assert_eq!(
            number_lines(text, Some(2), Some(3), 1000)?,
            "2\tb\n3\tc\n[lines 2-3 of 4]"
        );
        let cut = number_lines(text, None, None, 8)?;
        assert!(cut.starts_with("1\ta\n2\tb\n"));
        assert!(
            cut.ends_with("[truncated: lines 1-2 of 4; read again with start_line=3 for more]")
        );
        assert!(number_lines(text, Some(9), None, 1000).is_err());
        assert!(number_lines(text, Some(3), Some(2), 1000).is_err());
        Ok(())
    }
    #[test]
    fn edit_requires_one_exact_match() -> Result<()> {
        assert_eq!(replace_once("fn a() {}\n", "a()", "b()")?, "fn b() {}\n");
        assert!(replace_once("x x", "x", "y").is_err());
        assert!(replace_once("x", "z", "y").is_err());
        assert_eq!(replace_once("a\r\nb\r\n", "a\nb", "c\nd")?, "c\r\nd\r\n");
        let temp = tempfile::tempdir()?;
        std::fs::write(temp.path().join("f.txt"), "one two")?;
        let (current, updated) = plan_edit(temp.path(), "f.txt", "two", "three")?;
        assert_eq!(
            (current.as_str(), updated.as_str()),
            ("one two", "one three")
        );
        assert!(plan_edit(temp.path(), "missing.txt", "a", "b").is_err());
        assert!(plan_edit(temp.path(), "../f.txt", "a", "b").is_err());
        Ok(())
    }
    #[test]
    fn searches_text_and_skips_build_dirs() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::create_dir_all(root.join("target"))?;
        std::fs::write(
            root.join("src").join("main.rs"),
            "fn main() {}\nfn Helper() {}\n",
        )?;
        std::fs::write(root.join("target").join("out.rs"), "fn helper() {}\n")?;
        std::fs::write(root.join("blob.bin"), b"helper\0\x01")?;
        let found = search_code(root, "helper", false, 10)?;
        assert!(found.contains("main.rs:2: fn Helper() {}"));
        assert!(!found.contains("out.rs") && !found.contains("blob.bin"));
        assert!(search_code(root, "helper", true, 10)?.starts_with("No matches"));
        let capped = search_code(root, "fn", true, 1)?;
        assert!(capped.contains("[1 more matches"));
        Ok(())
    }
    #[test]
    fn keeps_head_and_tail_of_streams() {
        let mut captured = Captured::default();
        let data: Vec<u8> = (0..STREAM_HEAD + STREAM_TAIL + 500)
            .map(|i| {
                if i < STREAM_HEAD {
                    b'h'
                } else if i < STREAM_HEAD + 500 {
                    b'm'
                } else {
                    b't'
                }
            })
            .collect();
        for chunk in data.chunks(1000) {
            captured.push(chunk);
        }
        let shown = captured.render("stdout");
        assert!(shown.starts_with("stdout:\nhhh"));
        assert!(shown.contains("[... 500 bytes omitted ...]"));
        assert!(shown.ends_with("ttt") && !shown.contains("mm"));
        assert_eq!(Captured::default().render("stderr"), "stderr: (empty)");
    }
    #[test]
    fn flags_interpreters() {
        for p in [
            "cmd",
            "PowerShell.exe",
            "/usr/bin/bash",
            "python3.12",
            "node",
            "pwsh",
        ] {
            assert!(is_interpreter(p), "{p}");
        }
        for p in ["cargo", "git", "rustc.exe", "npm"] {
            assert!(!is_interpreter(p), "{p}");
        }
    }
    #[test]
    fn checks_env_names() {
        for ok in ["MY_TOKEN", "ProgramFiles(x86)", "a1"] {
            assert!(valid_env_name(ok), "{ok}");
        }
        for bad in ["", "A=B", "1X", "A B", "X\0"] {
            assert!(!valid_env_name(bad), "{bad}");
        }
    }
    #[test]
    fn scrubbed_env_passes_only_allowed_names() {
        let extra = std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .find(|k| valid_env_name(k) && !PASS_ENV.iter().any(|p| p.eq_ignore_ascii_case(k)));
        let Some(extra) = extra else { return };
        let has = |allowed: &[String]| {
            scrubbed(Path::new("x"), Path::new("."), allowed)
                .get_envs()
                .any(|(k, v)| k == extra.as_str() && v.is_some())
        };
        assert!(!has(&[]));
        assert!(has(std::slice::from_ref(&extra)));
    }
    #[test]
    fn lists_files_honouring_gitignore() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::create_dir_all(root.join("target"))?;
        std::fs::write(root.join("src").join("main.rs"), "")?;
        std::fs::write(root.join("target").join("out.o"), "")?;
        std::fs::write(root.join("notes.log"), "")?;
        std::fs::write(root.join(".gitignore"), "*.log\n")?;
        let walked = list_ignored_aware(root);
        assert!(walked.contains(&"src/main.rs".into()) && !walked.contains(&"target/out.o".into()));
        assert!(walked.contains(&"notes.log".into()));
        let git = resolve_program(root, "git");
        let init = git.map(|g| scrubbed(&g, root, &[]).args(["init", "-q"]).status());
        if !init.is_some_and(|s| s.is_ok_and(|s| s.success())) {
            return Ok(());
        }
        let listed = list_ignored_aware(root);
        assert!(listed.contains(&"src/main.rs".into()) && listed.contains(&".gitignore".into()));
        assert!(!listed.contains(&"notes.log".into()));
        Ok(())
    }
    #[test]
    fn resolves_programs() -> Result<()> {
        let exe = std::env::current_exe()?;
        assert_eq!(
            resolve_program(Path::new("."), exe.to_str().unwrap()),
            Some(exe.clone())
        );
        assert!(resolve_program(Path::new("."), "definitely-not-a-real-program-xyz").is_none());
        assert!(resolve_program(Path::new("."), "").is_none());
        Ok(())
    }
}
