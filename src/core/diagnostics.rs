//! Local crash log and a diagnostics summary for bug reports. Nothing is sent
//! anywhere: the panic hook writes a file in data_dir() (the Windows build has
//! no console) and `report()` only builds text for the clipboard.
use super::config::data_dir;
use std::any::Any;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Lines of each log shown in the report.
const TAIL_LINES: usize = 40;
/// Bytes read from the end of a log, so a huge llama-server.log stays cheap.
const TAIL_BYTES: u64 = 64 * 1024;

pub fn crash_log() -> PathBuf {
    data_dir().join("crash.log")
}

/// Writes data_dir()/crash.log on a panic (the previous one becomes
/// crash.prev.log), then runs the hook that was installed before.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".into());
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        let text = crash_text(
            &panic_message(info.payload()),
            &location,
            &thread,
            &std::backtrace::Backtrace::force_capture().to_string(),
        );
        let _ = write_crash(&text);
        previous(info);
    }));
}

fn write_crash(text: &str) -> std::io::Result<()> {
    let path = crash_log();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        let _ = std::fs::rename(&path, path.with_file_name("crash.prev.log"));
    }
    std::fs::write(path, text)
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into())
}

fn crash_text(message: &str, location: &str, thread: &str, backtrace: &str) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "KYTTU - AI {} crashed ({} {})\nunix time: {secs}\nthread: {thread}\nat: {location}\n\n{message}\n\nbacktrace:\n{backtrace}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

/// Plain-text summary for the "Copy diagnostics" button: versions, paths and
/// the end of the crash and llama-server logs, with the home folder shortened.
pub fn report() -> String {
    let dir = data_dir();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let mut out = format!(
        "KYTTU - AI {}\nOS: {} {} ({}){}\nExecutable: {exe}\nSettings: {}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
        os_version().map(|v| format!(", {v}")).unwrap_or_default(),
        dir.join("settings.json").display(),
    );
    for name in [
        "crash.log",
        "llama-server.log",
        "llama-server.prev.log",
        "llama-embed.log",
    ] {
        match read_tail(&dir.join(name), TAIL_BYTES) {
            Some(text) => {
                out.push_str(&format!("\n--- {name} (last {TAIL_LINES} lines) ---\n"));
                out.push_str(&tail_lines(&text, TAIL_LINES));
                out.push('\n');
            }
            None if name == "crash.log" => out.push_str("\nNo crash log.\n"),
            None => {}
        }
    }
    match dirs::home_dir() {
        Some(home) => shorten_home(&out, &home),
        None => out,
    }
}

/// The file manager command that opens a folder on this platform.
fn file_manager() -> &'static str {
    if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Opens data_dir() (crash.log, llama-server.log and .prev.log) in the file manager.
pub fn open_logs_folder() -> std::io::Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let mut child = std::process::Command::new(file_manager())
        .arg(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // Reaped in the background so no zombie is left on Unix.
    std::thread::spawn(move || child.wait());
    Ok(())
}

fn read_tail(path: &Path, max: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    let mut bytes = vec![];
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Replaces the home folder with "~" (both slash styles on Windows).
fn shorten_home(text: &str, home: &Path) -> String {
    let home = home.display().to_string();
    if home.len() < 2 {
        return text.to_string();
    }
    text.replace(&home, "~")
        .replace(&home.replace('\\', "/"), "~")
}

fn os_version() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/etc/os-release").ok()?;
        text.lines()
            .find_map(|l| l.strip_prefix("PRETTY_NAME="))
            .map(|v| v.trim_matches('"').to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()?;
        Some(format!(
            "macOS {}",
            String::from_utf8_lossy(&out.stdout).trim()
        ))
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("cmd")
            .args(["/c", "ver"])
            .creation_flags(0x08000000)
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|v| !v.is_empty())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail_lines("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail_lines("a\nb", 5), "a\nb");
        assert_eq!(tail_lines("", 3), "");
    }

    #[test]
    fn home_is_shortened_in_both_slash_styles() {
        let home = Path::new(r"C:\Users\kt");
        let text = r"C:\Users\kt\AppData and C:/Users/kt/x";
        assert_eq!(shorten_home(text, home), r"~\AppData and ~/x");
        assert_eq!(shorten_home("/home/kt/a", Path::new("/home/kt")), "~/a");
        assert_eq!(shorten_home("/a", Path::new("/")), "/a");
    }

    #[test]
    fn panic_payloads_become_text() {
        let s: Box<dyn Any + Send> = Box::new("boom");
        let owned: Box<dyn Any + Send> = Box::new(String::from("bang"));
        let other: Box<dyn Any + Send> = Box::new(7_u8);
        assert_eq!(panic_message(s.as_ref()), "boom");
        assert_eq!(panic_message(owned.as_ref()), "bang");
        assert_eq!(panic_message(other.as_ref()), "non-string panic payload");
    }

    #[test]
    fn crash_text_names_version_and_location() {
        let text = crash_text("oops", "src/main.rs:1:2", "main", "frames");
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
        assert!(text.contains("at: src/main.rs:1:2"));
        assert!(text.contains("oops") && text.contains("frames"));
    }

    #[test]
    fn read_tail_reads_only_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "0123456789").unwrap();
        assert_eq!(read_tail(&path, 4).as_deref(), Some("6789"));
        assert_eq!(read_tail(&path, 100).as_deref(), Some("0123456789"));
        assert!(read_tail(&dir.path().join("missing"), 4).is_none());
    }
}
