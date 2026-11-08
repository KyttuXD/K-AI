//! Built-in llama.cpp runtime: launches `llama-server` with a chosen GGUF file
//! on a loopback port, so the app can run models without Ollama. Defaults are
//! tuned for Intel Macs (CPU inference, no Metal offload).
use super::config::{Config, data_dir};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

/// True on x86_64 macOS, where Metal offload is slow or unavailable and
/// llama.cpp should run on the CPU.
pub const INTEL_MAC: bool = cfg!(all(target_os = "macos", target_arch = "x86_64"));

/// Oldest llama.cpp builds that accept these flags; older builds exit on an
/// unknown flag, so the flag is left out for them.
const MIN_JINJA: u32 = 4600;
const MIN_DEVICE: u32 = 4400;
const MIN_CACHE_REUSE: u32 = 4000;
/// From here flash attention defaults to auto, which a quantized V cache needs.
const MIN_FLASH_AUTO: u32 = 6325;

pub(crate) fn binary_name() -> &'static str {
    if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    }
}

/// Hides the console window of a helper process on Windows.
pub(crate) fn quiet(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd
}

/// Finds `llama-server`: the configured path, next to this app (including a
/// macOS bundle's `Contents/MacOS` or `Contents/Resources`), Homebrew, PATH,
/// then a copy downloaded by first-run setup.
pub fn find_server(config: &Config) -> Option<PathBuf> {
    if !config.llama_server.as_os_str().is_empty() {
        return config
            .llama_server
            .is_file()
            .then(|| config.llama_server.clone());
    }
    let mut candidates = vec![];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for sub in ["", "llama", "../Resources", "../Resources/llama"] {
            candidates.push(dir.join(sub).join(binary_name()));
        }
    }
    if cfg!(target_os = "macos") {
        // Intel Homebrew installs to /usr/local, Apple Silicon to /opt/homebrew.
        candidates.push(PathBuf::from("/usr/local/bin/llama-server"));
        candidates.push(PathBuf::from("/opt/homebrew/bin/llama-server"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join(binary_name())));
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .or_else(super::setup::installed_server)
}

/// Probed version per server path, with the modification time it was read at.
type VersionCache = HashMap<PathBuf, (Option<SystemTime>, Option<String>)>;
static VERSIONS: LazyLock<Mutex<VersionCache>> = LazyLock::new(Default::default);

/// The build reported by `llama-server --version` (for example "5186 (5fa07c2f)"),
/// or None when the binary does not run. Cached per path and modification time.
pub fn server_version(path: &Path) -> Option<String> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok();
    let cached = VERSIONS.lock().unwrap().get(path).cloned();
    if let Some((time, version)) = cached
        && time == mtime
    {
        return version;
    }
    let version = probe_version(path);
    VERSIONS
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), (mtime, version.clone()));
    version
}

fn probe_version(path: &Path) -> Option<String> {
    // A temp file instead of pipes: device listings can be long and nothing
    // reads a pipe while we wait.
    let mut out = tempfile::tempfile().ok()?;
    let mut cmd = Command::new(path);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(out.try_clone().ok()?)
        .stderr(out.try_clone().ok()?);
    if let Some(dir) = path.parent() {
        cmd.current_dir(dir);
    }
    let mut child = quiet(&mut cmd).spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut bytes = vec![];
    out.seek(SeekFrom::Start(0)).ok()?;
    out.read_to_end(&mut bytes).ok()?;
    parse_version(&String::from_utf8_lossy(&bytes))
}

/// Reads the "version: 5186 (5fa07c2f)" line of `--version` output.
fn parse_version(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("version:"))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The build number of a version string ("5186 (5fa07c2f)" or "b5186").
pub fn build_number(version: &str) -> Option<u32> {
    let digits: String = version
        .trim()
        .trim_start_matches('b')
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// How one launch differs from the saved settings.
#[derive(Clone, Copy, Debug, Default)]
pub struct Launch {
    pub port: u16,
    pub intel_mac: bool,
    /// llama.cpp build number; None when unknown (assumed recent).
    pub build: Option<u32>,
    /// Forced CPU run after a GPU failure.
    pub cpu_only: bool,
}

/// Arguments for `llama-server`. Kept separate so it can be tested.
pub fn server_args(config: &Config, launch: &Launch) -> Vec<String> {
    let supports = |min: u32| launch.build.is_none_or(|b| b >= min);
    let mut args: Vec<String> = vec![
        "-m".into(),
        config.gguf_model.display().to_string(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        launch.port.to_string(),
        "-c".into(),
        config.context_tokens.to_string(),
        "--alias".into(),
        model_alias(config),
        // One slot, so the whole context and its KV cache serve this app.
        "--parallel".into(),
        "1".into(),
    ];
    if supports(MIN_JINJA) {
        // Chat templates with tool-call support.
        args.push("--jinja".into());
    }
    if supports(MIN_CACHE_REUSE) {
        args.extend(["--cache-reuse".into(), "256".into()]);
    }
    if config.llama_threads > 0 {
        args.extend(["-t".into(), config.llama_threads.to_string()]);
    }
    let layers = match config.llama_gpu_layers {
        _ if launch.cpu_only => Some(0),
        Some(n) => Some(n),
        None if launch.intel_mac => Some(0),
        None => None,
    };
    if let Some(n) = layers {
        args.extend(["-ngl".into(), n.to_string()]);
    }
    let cpu = launch.cpu_only || (launch.intel_mac && config.llama_gpu_layers.unwrap_or(0) == 0);
    if cpu && supports(MIN_DEVICE) {
        // Old Intel GPUs and the Metal backend misbehave; keep everything on the CPU.
        args.extend(["--device".into(), "none".into()]);
    }
    if launch.intel_mac && config.context_tokens > 4096 {
        // Low-end profile: an 8-bit KV cache halves its RAM on long contexts.
        args.extend(["--cache-type-k".into(), "q8_0".into()]);
        if supports(MIN_FLASH_AUTO) {
            args.extend(["--cache-type-v".into(), "q8_0".into()]);
        }
    }
    args
}

pub fn model_alias(config: &Config) -> String {
    config
        .gguf_model
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local-model".into())
}

/// The first port from `preferred` that nothing listens on and we can bind.
fn free_port(preferred: u16) -> Option<u16> {
    (preferred..=preferred.saturating_add(50)).find(|&port| {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_err()
            && TcpListener::bind(addr).is_ok()
    })
}

/// True when /v1/models (OpenAI or llama.cpp shape) lists `alias`.
fn lists_model(models: &Value, alias: &str) -> bool {
    let ids = |key: &str, field: &str| {
        models[key]
            .as_array()
            .is_some_and(|list| list.iter().any(|m| m[field].as_str() == Some(alias)))
    };
    ids("data", "id") || ids("models", "name") || ids("models", "model")
}

/// True when the log shows the GPU backend failing to start or running out of memory.
pub fn gpu_failed(log: &str) -> bool {
    let log = log.to_lowercase();
    [
        "out of memory",
        "outofdevicememory",
        "erroroutof",
        "failed to allocate",
        "unable to allocate",
        "cudamalloc failed",
        "ggml_cuda_init: failed",
        "failed to initialize cuda",
        "no kernel image",
        "devicelost",
        "ggml_vulkan: error",
        "failed to initialize vulkan",
    ]
    .iter()
    .any(|needle| log.contains(needle))
}

/// Summarises the backend and device llama-server actually used, from its log.
pub fn parse_backend(log: &str) -> String {
    let mut devices: Vec<String> = vec![];
    let mut offloaded = None;
    for line in log.lines() {
        let device = line
            .split_once("using device ")
            .or_else(|| line.split_once("found device: "))
            .map(|(_, rest)| rest.split(" - ").next().unwrap_or(rest).trim().to_string());
        if let Some(d) = device.filter(|d| !d.is_empty())
            && !devices.contains(&d)
        {
            devices.push(d);
        }
        if let Some((_, rest)) = line.split_once("offloaded ")
            && rest.contains("layers to GPU")
            && let Some(fraction) = rest.split_whitespace().next()
        {
            offloaded = Some(fraction.to_string());
        }
    }
    let on_gpu = offloaded
        .as_deref()
        .and_then(|f| f.split('/').next()?.parse::<u32>().ok())
        .unwrap_or(0);
    match offloaded {
        Some(fraction) if on_gpu > 0 => {
            let names = if devices.is_empty() {
                "GPU".into()
            } else {
                devices.join(", ")
            };
            format!("{names}, {fraction} layers on GPU")
        }
        _ => "CPU".into(),
    }
}

fn pid_path() -> PathBuf {
    data_dir().join("llama-server.pid")
}

/// The image name from `tasklist /FO CSV /NH` or `ps -o comm=` output.
fn parse_process_name(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let name = match line.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?,
        None => line,
    };
    name.rsplit(['/', '\\']).next().map(str::to_string)
}

fn is_llama_server(name: &str) -> bool {
    let name = name.trim().to_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name) == "llama-server"
}

fn process_name(pid: u32) -> Option<String> {
    let (filter, pid) = (format!("PID eq {pid}"), pid.to_string());
    let out = if cfg!(windows) {
        quiet(Command::new("tasklist").args(["/FI", filter.as_str(), "/FO", "CSV", "/NH"]))
            .output()
            .ok()?
    } else {
        Command::new("ps")
            .args(["-p", pid.as_str(), "-o", "comm="])
            .output()
            .ok()?
    };
    parse_process_name(&String::from_utf8_lossy(&out.stdout))
}

/// The PID file holds `<server pid> <app pid>`; older files hold only the server PID.
fn parse_pid_file(text: &str) -> Option<(u32, Option<u32>)> {
    let mut parts = text.split_whitespace().map(|s| s.parse::<u32>().ok());
    let server = parts.next()??;
    Some((server, parts.next().flatten()))
}

/// True when `pid` is this process or another running copy of this app.
fn app_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    let exe = std::env::current_exe().ok();
    let exe = exe
        .as_deref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    exe.zip(process_name(pid))
        .is_some_and(|(exe, name)| exe.eq_ignore_ascii_case(name.trim()))
}

/// Kills a llama-server left behind by a crash, using the PID file. Any other
/// process that has since reused the PID is left alone, and so is a server that
/// another running KYTTU instance still owns.
fn kill_stale() {
    let Some((pid, app)) = std::fs::read_to_string(pid_path())
        .ok()
        .and_then(|s| parse_pid_file(&s))
    else {
        let _ = std::fs::remove_file(pid_path());
        return;
    };
    if app.is_some_and(app_alive) {
        return;
    }
    if process_name(pid).is_some_and(|n| is_llama_server(&n)) {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("taskkill");
            c.arg("/PID").arg(pid.to_string()).arg("/F");
            c
        } else {
            let mut c = Command::new("kill");
            c.arg(pid.to_string());
            c
        };
        let _ = quiet(&mut cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // Give the OS a moment to release the port and memory.
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = std::fs::remove_file(pid_path());
}

#[derive(Clone, Default)]
pub struct Runtime {
    child: Arc<Mutex<Option<Child>>>,
    /// Started successfully and not stopped by us.
    live: Arc<AtomicBool>,
    crashed: Arc<AtomicBool>,
    backend: Arc<Mutex<Option<String>>>,
}

impl Runtime {
    pub fn running(&self) -> bool {
        let mut guard = self.child.lock().unwrap();
        match guard.as_mut().map(|c| c.try_wait()) {
            Some(Ok(None)) => true,
            Some(_) => {
                *guard = None;
                let _ = std::fs::remove_file(pid_path());
                if self.live.swap(false, Ordering::SeqCst) {
                    self.crashed.store(true, Ordering::SeqCst);
                }
                false
            }
            None => false,
        }
    }

    /// True when a server that started fine has since exited on its own.
    /// Cleared by the next start() or stop().
    pub fn exited_unexpectedly(&self) -> bool {
        !self.running() && self.crashed.load(Ordering::SeqCst)
    }

    /// Backend and device of the running server, e.g. "CPU" or
    /// "Vulkan0 (AMD Radeon RX 6600), 29/29 layers on GPU".
    pub fn backend_info(&self) -> Option<String> {
        self.backend.lock().unwrap().clone()
    }

    /// Starts llama-server, blocks until it serves the model, and returns the
    /// port it listens on (the next free one when llama_port is taken).
    pub fn start(&self, config: &Config) -> Result<u16> {
        self.stop();
        ensure!(
            config.gguf_model.is_file(),
            "Choose a .gguf model file first"
        );
        let server = find_server(config).context(
            "llama-server was not found. Run setup, choose it in Settings, or install llama.cpp (Homebrew: brew install llama.cpp).",
        )?;
        kill_stale();
        let _ = std::fs::create_dir_all(data_dir());
        let log_path = data_dir().join("llama-server.log");
        let mut launch = Launch {
            port: 0,
            intel_mac: INTEL_MAC,
            build: server_version(&server).as_deref().and_then(build_number),
            cpu_only: false,
        };
        let already_cpu =
            config.llama_gpu_layers == Some(0) || (INTEL_MAC && config.llama_gpu_layers.is_none());
        let first = self.launch(config, &server, &mut launch, &log_path);
        if let Err(e) = first {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            if already_cpu || !gpu_failed(&log) {
                return Err(e);
            }
            self.stop();
            launch.cpu_only = true;
            self.launch(config, &server, &mut launch, &log_path)
                .with_context(|| format!("GPU offload failed ({e}); the CPU retry failed too"))?;
        }
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let mut backend = parse_backend(&log);
        if launch.cpu_only {
            backend.push_str(" (GPU failed, using CPU)");
        }
        if let Some(build) = launch.build {
            backend.push_str(&format!(", build b{build}"));
        }
        *self.backend.lock().unwrap() = Some(backend);
        self.crashed.store(false, Ordering::SeqCst);
        self.live.store(true, Ordering::SeqCst);
        Ok(launch.port)
    }

    fn launch(
        &self,
        config: &Config,
        server: &Path,
        launch: &mut Launch,
        log_path: &Path,
    ) -> Result<()> {
        launch.port = free_port(config.llama_port)
            .with_context(|| format!("No free port found from {} upwards", config.llama_port))?;
        let log = std::fs::File::create(log_path)?;
        let mut cmd = Command::new(server);
        cmd.args(server_args(config, launch))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        if let Some(dir) = server.parent() {
            // Release zips ship shared libraries beside the binary.
            cmd.current_dir(dir);
        }
        let child = quiet(&mut cmd).spawn().map_err(|e| {
            if cfg!(target_os = "macos") && e.kind() == std::io::ErrorKind::PermissionDenied {
                anyhow::anyhow!(
                    "macOS blocked llama-server ({e}). Make it executable, or allow it in System Settings → Privacy & Security."
                )
            } else {
                anyhow::anyhow!("Could not start {}: {e}", server.display())
            }
        })?;
        let _ = std::fs::write(pid_path(), format!("{} {}", child.id(), std::process::id()));
        *self.child.lock().unwrap() = Some(child);
        self.wait_ready(config, launch.port, log_path)
    }

    fn wait_ready(&self, config: &Config, port: u16, log_path: &Path) -> Result<()> {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        let base = format!("http://127.0.0.1:{port}");
        let alias = model_alias(config);
        // Large models on old CPUs can take minutes to map and load.
        let deadline = Instant::now() + Duration::from_secs(300);
        while Instant::now() < deadline {
            if !self.running() {
                bail!("llama-server exited.\n{}", log_tail(log_path));
            }
            let healthy = client
                .get(format!("{base}/health"))
                .send()
                .is_ok_and(|r| r.status().is_success());
            if healthy
                && let Some(models) = client
                    .get(format!("{base}/v1/models"))
                    .send()
                    .ok()
                    .and_then(|r| r.json::<Value>().ok())
            {
                if lists_model(&models, &alias) {
                    return Ok(());
                }
                self.stop();
                bail!("Another server answered on port {port} and does not serve {alias}.");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        self.stop();
        bail!(
            "llama-server did not become ready within 5 minutes.\n{}",
            log_tail(log_path)
        )
    }

    pub fn stop(&self) {
        self.live.store(false, Ordering::SeqCst);
        self.crashed.store(false, Ordering::SeqCst);
        *self.backend.lock().unwrap() = None;
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(pid_path());
        }
    }
}

fn log_tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<_> = text.lines().rev().take(8).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        Config {
            gguf_model: PathBuf::from("/models/qwen2.5-coder-3b-q4_k_m.gguf"),
            ..Default::default()
        }
    }
    fn launch(intel_mac: bool) -> Launch {
        Launch {
            port: 8081,
            intel_mac,
            ..Default::default()
        }
    }
    #[test]
    fn intel_mac_defaults_to_cpu_only() {
        let joined = server_args(&config(), &launch(true)).join(" ");
        assert!(joined.contains("-ngl 0"));
        assert!(joined.contains("--device none"));
        assert!(joined.contains("--host 127.0.0.1"));
        assert!(joined.contains("--port 8081"));
        assert!(joined.contains("--alias qwen2.5-coder-3b-q4_k_m"));
        assert!(joined.contains("--parallel 1"));
        assert!(joined.contains("--cache-reuse 256"));
    }
    #[test]
    fn other_platforms_keep_server_gpu_default() {
        let args = server_args(&config(), &launch(false)).join(" ");
        assert!(!args.contains("-ngl"));
        assert!(!args.contains("--device"));
        assert!(!args.contains("--cache-type"));
        assert!(args.contains("--jinja"));
    }
    #[test]
    fn explicit_gpu_layers_override_intel_default() {
        let mut c = config();
        c.llama_gpu_layers = Some(20);
        c.llama_threads = 4;
        let args = server_args(&c, &launch(true)).join(" ");
        assert!(args.contains("-ngl 20"));
        assert!(args.contains("-t 4"));
        assert!(!args.contains("--device none"));
    }
    #[test]
    fn cpu_fallback_overrides_gpu_layers() {
        let mut c = config();
        c.llama_gpu_layers = Some(99);
        let l = Launch {
            cpu_only: true,
            ..launch(false)
        };
        let args = server_args(&c, &l).join(" ");
        assert!(args.contains("-ngl 0"));
        assert!(args.contains("--device none"));
        assert!(!args.contains("-ngl 99"));
    }
    #[test]
    fn old_builds_get_only_supported_flags() {
        let l = Launch {
            build: Some(3500),
            ..launch(true)
        };
        let args = server_args(&config(), &l).join(" ");
        assert!(!args.contains("--jinja"));
        assert!(!args.contains("--device"));
        assert!(!args.contains("--cache-reuse"));
        assert!(args.contains("--cache-type-k q8_0"));
        assert!(!args.contains("--cache-type-v"));
        let new = Launch {
            build: Some(7000),
            ..launch(true)
        };
        let args = server_args(&config(), &new).join(" ");
        assert!(args.contains("--jinja") && args.contains("--cache-type-v q8_0"));
    }
    #[test]
    fn low_end_profile_only_for_long_contexts() {
        let mut c = config();
        c.context_tokens = 4096;
        assert!(
            !server_args(&c, &launch(true))
                .join(" ")
                .contains("--cache-type")
        );
    }
    #[test]
    fn parses_versions() {
        let out = "ggml_vulkan: Found 1 Vulkan devices:\nversion: 5186 (5fa07c2f)\nbuilt with MSVC for x64\n";
        assert_eq!(parse_version(out).as_deref(), Some("5186 (5fa07c2f)"));
        assert_eq!(parse_version("usage: llama-server"), None);
        assert_eq!(build_number("5186 (5fa07c2f)"), Some(5186));
        assert_eq!(build_number("b12110"), Some(12110));
        assert_eq!(build_number("unknown"), None);
    }
    #[test]
    fn reads_backend_from_log() {
        let gpu = "llama_model_load_from_file_impl: using device Vulkan0 (AMD Radeon RX 6600) - 7920 MiB free\nload_tensors: offloaded 29/29 layers to GPU\n";
        assert_eq!(
            parse_backend(gpu),
            "Vulkan0 (AMD Radeon RX 6600), 29/29 layers on GPU"
        );
        assert_eq!(
            parse_backend("load_tensors: offloaded 0/29 layers to GPU"),
            "CPU"
        );
        assert_eq!(parse_backend(""), "CPU");
    }
    #[test]
    fn recognises_gpu_failures() {
        assert!(gpu_failed(
            "ggml_vulkan: Device memory allocation of size 1 failed.\nvk::Device::allocateMemory: ErrorOutOfDeviceMemory"
        ));
        assert!(gpu_failed("CUDA error: out of memory"));
        assert!(!gpu_failed(
            "main: server is listening on http://127.0.0.1:8080"
        ));
    }
    #[test]
    fn checks_served_model() {
        let v: Value = serde_json::json!({"object":"list","data":[{"id":"qwen"}]});
        assert!(lists_model(&v, "qwen"));
        assert!(!lists_model(&v, "other"));
        let v: Value = serde_json::json!({"models":[{"name":"qwen","model":"qwen"}]});
        assert!(lists_model(&v, "qwen"));
    }
    #[test]
    fn only_llama_server_counts_as_stale() {
        let csv = "\"llama-server.exe\",\"1234\",\"Console\",\"1\",\"2,000,000 K\"\r\n";
        let name = parse_process_name(csv).unwrap();
        assert!(is_llama_server(&name));
        assert!(is_llama_server(
            &parse_process_name("/opt/homebrew/bin/llama-server\n").unwrap()
        ));
        assert!(!is_llama_server(
            &parse_process_name("\"chrome.exe\",\"1234\"").unwrap()
        ));
        assert!(!is_llama_server(
            &parse_process_name("INFO: No tasks are running which match the specified criteria.")
                .unwrap()
        ));
        assert_eq!(parse_process_name(""), None);
    }
    #[test]
    fn pid_file_records_the_owning_app() {
        assert_eq!(parse_pid_file("123 456\n"), Some((123, Some(456))));
        assert_eq!(parse_pid_file("123"), Some((123, None)));
        assert_eq!(parse_pid_file("x 1"), None);
        assert_eq!(parse_pid_file(""), None);
        assert!(app_alive(std::process::id()));
    }
}
