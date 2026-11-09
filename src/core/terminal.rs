//! Long-running terminal sessions with streamed output and process-tree cleanup.
//! Windows puts each session in a Job Object that kills every process in it when closed (so the
//! tree also dies with the app); Unix gives it its own process group.

use super::tools;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

/// Bytes of output kept per session (the tail).
const OUTPUT_CAP: usize = 1 << 20;

/// A saved command the user can run again from the terminal panel.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskProfile {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
}

pub struct Session {
    pub id: u64,
    pub title: String,
    pub output: Arc<Mutex<String>>,
    pub running: Arc<AtomicBool>,
    pub exit_code: Arc<Mutex<Option<i32>>>,
    /// Bumped whenever output changes, so a viewer can cache its layout.
    pub generation: Arc<AtomicU64>,
    waker: Waker,
    tree: Arc<Tree>,
}

/// Called after new output arrives (for example to request a UI repaint).
type Waker = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

impl Session {
    /// Stops the program and everything it started.
    pub fn kill_tree(&self) {
        self.tree.kill();
    }

    /// Stops what the session still runs. Unix kills the group even after the leader exited,
    /// since background children stay in it. Windows only kills while running: the Job handle
    /// closing ends leftovers, and a pid-based fallback after exit could hit a reused pid.
    pub fn stop(&self) {
        if cfg!(unix) || self.running.load(Ordering::Acquire) {
            self.tree.kill();
        }
    }

    /// Runs `wake` each time output arrives.
    pub fn on_output(&self, wake: impl Fn() + Send + 'static) {
        *self.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(wake));
    }

    pub fn clear_output(&self) {
        self.output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.generation.fetch_add(1, Ordering::Release);
    }
}

/// Closing a session stops what it still runs.
impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn spawn(root: &Path, program: &str, args: &[String]) -> Result<Session> {
    spawn_with_env(root, program, args, &[])
}

/// Like `spawn`, also passing the named environment variables through (item 61).
pub fn spawn_with_env(
    root: &Path,
    program: &str,
    args: &[String],
    extra_env: &[String],
) -> Result<Session> {
    ensure!(!program.trim().is_empty(), "Program is required");
    ensure!(root.is_dir(), "Workspace folder does not exist");
    let resolved = tools::resolve_program(root, program)
        .with_context(|| format!("Program not found on PATH or in the workspace: {program}"))?;
    let mut command = tools::scrubbed(&resolved, root, extra_env);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Tree::prepare(&mut command);
    let mut child = command.spawn().context("Could not start program")?;
    let tree = Arc::new(Tree::attach(&child));
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let session = Session {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        title: title(program, args),
        output: Arc::default(),
        running: Arc::new(AtomicBool::new(true)),
        exit_code: Arc::default(),
        generation: Arc::default(),
        waker: Arc::default(),
        tree: tree.clone(),
    };
    let readers = [
        stream(child.stdout.take(), &session),
        stream(child.stderr.take(), &session),
    ];
    let (running, exit_code) = (session.running.clone(), session.exit_code.clone());
    std::thread::spawn(move || {
        let code = child.wait().ok().and_then(|s| s.code());
        // Descendants may keep the pipes open; give the readers a moment, not forever.
        for done in readers {
            let _ = done.recv_timeout(Duration::from_millis(250));
        }
        *exit_code.lock().unwrap_or_else(|e| e.into_inner()) = code;
        running.store(false, Ordering::Release);
        drop(tree);
    });
    Ok(session)
}

fn title(program: &str, args: &[String]) -> String {
    let name = Path::new(program)
        .file_name()
        .map_or(program.into(), |n| n.to_string_lossy());
    let full = std::iter::once(&*name)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    match full.char_indices().nth(60) {
        Some((cut, _)) => format!("{}…", &full[..cut]),
        None => full,
    }
}

/// Copies a pipe into the shared output; the receiver fires when the pipe closes.
fn stream(pipe: Option<impl Read + Send + 'static>, session: &Session) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    let (output, generation, waker) = (
        session.output.clone(),
        session.generation.clone(),
        session.waker.clone(),
    );
    std::thread::spawn(move || {
        if let Some(mut pipe) = pipe {
            let mut carry = Vec::new();
            let mut buffer = [0u8; 8192];
            while let Ok(n @ 1..) = pipe.read(&mut buffer) {
                let text = decode(&mut carry, &buffer[..n]);
                append_capped(
                    &mut output.lock().unwrap_or_else(|e| e.into_inner()),
                    &text,
                    OUTPUT_CAP,
                );
                generation.fetch_add(1, Ordering::Release);
                if let Some(wake) = &*waker.lock().unwrap_or_else(|e| e.into_inner()) {
                    wake();
                }
            }
        }
        let _ = tx.send(());
    });
    rx
}

/// Decodes UTF-8 across chunk boundaries: an incomplete trailing sequence waits in `carry`,
/// invalid bytes become U+FFFD.
fn decode(carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    carry.extend_from_slice(chunk);
    let mut text = String::new();
    let mut rest: &[u8] = carry;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                text.push_str(valid);
                rest = &[];
                break;
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                text.push_str(std::str::from_utf8(valid).unwrap_or_default());
                match e.error_len() {
                    Some(bad) => {
                        text.push('\u{FFFD}');
                        rest = &after[bad..];
                    }
                    None => {
                        rest = after;
                        break;
                    }
                }
            }
        }
    }
    let keep = rest.len();
    carry.drain(..carry.len() - keep);
    text
}

/// Appends and, past `cap` bytes, drops the oldest text (an eighth extra so trims stay rare).
fn append_capped(out: &mut String, text: &str, cap: usize) {
    out.push_str(text);
    if out.len() > cap {
        let mut cut = out.len() - cap + cap / 8;
        while !out.is_char_boundary(cut) {
            cut += 1;
        }
        out.drain(..cut);
    }
}

/// A started process and its descendants.
pub(crate) struct Tree {
    pid: u32,
    /// Job Object handle (as an integer so the type is Send + Sync); 0 when assignment failed.
    #[cfg(windows)]
    job: isize,
}

impl Tree {
    /// Call before spawning so the child can be grouped.
    pub(crate) fn prepare(command: &mut Command) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(not(unix))]
        let _ = command;
    }

    #[cfg(windows)]
    pub(crate) fn attach(child: &Child) -> Tree {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };
        // SAFETY: plain Win32 calls on a handle we own and the live child's process handle.
        let job = unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                0
            } else {
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) != 0
                    && AssignProcessToJobObject(job, child.as_raw_handle()) != 0;
                if ok {
                    job as isize
                } else {
                    CloseHandle(job);
                    0
                }
            }
        };
        Tree {
            pid: child.id(),
            job,
        }
    }

    #[cfg(not(windows))]
    pub(crate) fn attach(child: &Child) -> Tree {
        Tree { pid: child.id() }
    }

    pub(crate) fn kill(&self) {
        #[cfg(windows)]
        {
            use windows_sys::Win32::{Foundation::HANDLE, System::JobObjects::TerminateJobObject};
            // SAFETY: the job handle stays open until Drop.
            if self.job != 0 && unsafe { TerminateJobObject(self.job as HANDLE, 1) } != 0 {
                return;
            }
            let pid = self.pid.to_string();
            let _ = quiet(Command::new("taskkill").args(["/T", "/F", "/PID", &pid]));
        }
        #[cfg(unix)]
        {
            // The group id equals the leader's pid (process_group(0)).
            let group = format!("-{}", self.pid);
            let _ = quiet(Command::new("kill").args(["-KILL", "--", &group]));
        }
    }
}

#[cfg(windows)]
impl Drop for Tree {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        if self.job != 0 {
            // SAFETY: closing our own handle once; KILL_ON_JOB_CLOSE ends any leftovers.
            unsafe { CloseHandle(self.job as HANDLE) };
        }
    }
}

fn quiet(command: &mut Command) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn decodes_split_and_invalid_utf8() {
        let mut carry = Vec::new();
        let bytes = "héllo".as_bytes();
        assert_eq!(decode(&mut carry, &bytes[..2]), "h");
        assert_eq!(carry.len(), 1);
        assert_eq!(decode(&mut carry, &bytes[2..]), "éllo");
        assert!(carry.is_empty());
        assert_eq!(decode(&mut carry, b"a\xffb"), "a\u{FFFD}b");
    }

    #[test]
    fn caps_output_keeping_the_tail() {
        let mut out = String::new();
        for _ in 0..100 {
            append_capped(&mut out, "0123456789", 64);
        }
        assert!(out.len() <= 64 && out.ends_with("789"));
        let mut wide = String::new();
        append_capped(&mut wide, &"é".repeat(40), 33);
        assert!(wide.len() <= 33 && wide.chars().all(|c| c == 'é'));
    }

    #[test]
    fn titles_are_short() {
        assert_eq!(title("/usr/bin/cargo", &["build".into()]), "cargo build");
        assert!(title("x", &["y".repeat(100)]).chars().count() <= 61);
    }

    #[test]
    fn streams_and_reports_exit() -> Result<()> {
        let exe = std::env::current_exe()?;
        let session = spawn(
            &std::env::current_dir()?,
            exe.to_str().unwrap(),
            &["--list".into()],
        )?;
        let start = Instant::now();
        let deadline = Duration::from_secs(30);
        while session.running.load(Ordering::Acquire) && start.elapsed() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(*session.exit_code.lock().unwrap(), Some(0));
        assert!(
            session
                .output
                .lock()
                .unwrap()
                .contains("streams_and_reports_exit")
        );
        let generation = session.generation.load(Ordering::Acquire);
        assert!(generation > 0);
        session.clear_output();
        assert!(session.generation.load(Ordering::Acquire) > generation);
        Ok(())
    }
}
