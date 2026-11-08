//! Built-in llama.cpp runtime: launches `llama-server` with a chosen GGUF file
//! on a loopback port, so the app can run models without Ollama. Defaults are
//! tuned for Intel Macs (CPU inference, no Metal offload).
use super::config::{Config, data_dir};
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// True on x86_64 macOS, where Metal offload is slow or unavailable and
/// llama.cpp should run on the CPU.
pub const INTEL_MAC: bool = cfg!(all(target_os = "macos", target_arch = "x86_64"));

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    }
}

/// Finds `llama-server`: the configured path, next to this app (including a
/// macOS bundle's `Contents/MacOS` or `Contents/Resources`), Homebrew, then PATH.
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
        candidates.push(dir.join(binary_name()));
        candidates.push(dir.join("llama").join(binary_name()));
        candidates.push(dir.join("../Resources").join(binary_name()));
    }
    if cfg!(target_os = "macos") {
        // Intel Homebrew installs to /usr/local, Apple Silicon to /opt/homebrew.
        candidates.push(PathBuf::from("/usr/local/bin/llama-server"));
        candidates.push(PathBuf::from("/opt/homebrew/bin/llama-server"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join(binary_name())));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Arguments for `llama-server`. Kept separate so it can be tested.
pub fn server_args(config: &Config, intel_mac: bool) -> Vec<String> {
    let mut args = vec![
        "-m".into(),
        config.gguf_model.display().to_string(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        config.llama_port.to_string(),
        "-c".into(),
        config.context_tokens.to_string(),
        "--alias".into(),
        model_alias(config),
        // Chat templates with tool-call support.
        "--jinja".into(),
    ];
    if config.llama_threads > 0 {
        args.extend(["-t".into(), config.llama_threads.to_string()]);
    }
    let layers = match config.llama_gpu_layers {
        Some(n) => Some(n),
        None if intel_mac => Some(0),
        None => None,
    };
    if let Some(n) = layers {
        args.extend(["-ngl".into(), n.to_string()]);
    }
    if intel_mac && config.llama_gpu_layers.unwrap_or(0) == 0 {
        // Old Intel GPUs and the Metal backend misbehave; keep everything on the CPU.
        args.extend(["--device".into(), "none".into()]);
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

#[derive(Clone, Default)]
pub struct Runtime {
    child: Arc<Mutex<Option<Child>>>,
}

impl Runtime {
    pub fn running(&self) -> bool {
        let mut guard = self.child.lock().unwrap();
        match guard.as_mut().map(|c| c.try_wait()) {
            Some(Ok(None)) => true,
            Some(_) => {
                *guard = None;
                false
            }
            None => false,
        }
    }

    /// Starts llama-server and blocks until it reports healthy (model loaded).
    pub fn start(&self, config: &Config) -> Result<()> {
        self.stop();
        ensure!(
            config.gguf_model.is_file(),
            "Choose a .gguf model file first"
        );
        let server = find_server(config).context(
            "llama-server was not found. Choose it in Settings, or install llama.cpp (Homebrew: brew install llama.cpp).",
        )?;
        let log_path = data_dir().join("llama-server.log");
        let _ = std::fs::create_dir_all(data_dir());
        let log = std::fs::File::create(&log_path)?;
        let mut cmd = Command::new(&server);
        cmd.args(server_args(config, INTEL_MAC))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        if let Some(dir) = server.parent() {
            // Release zips ship shared libraries beside the binary.
            cmd.current_dir(dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        let child = cmd.spawn().map_err(|e| {
            if cfg!(target_os = "macos") && e.kind() == std::io::ErrorKind::PermissionDenied {
                anyhow::anyhow!(
                    "macOS blocked llama-server ({e}). Make it executable, or allow it in System Settings → Privacy & Security."
                )
            } else {
                anyhow::anyhow!("Could not start {}: {e}", server.display())
            }
        })?;
        *self.child.lock().unwrap() = Some(child);
        self.wait_healthy(config, &log_path)
    }

    fn wait_healthy(&self, config: &Config, log_path: &Path) -> Result<()> {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        let url = format!("http://127.0.0.1:{}/health", config.llama_port);
        // Large models on old CPUs can take minutes to map and load.
        let deadline = Instant::now() + Duration::from_secs(300);
        while Instant::now() < deadline {
            if !self.running() {
                anyhow::bail!("llama-server exited.\n{}", log_tail(log_path));
            }
            if client
                .get(&url)
                .send()
                .is_ok_and(|r| r.status().is_success())
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        self.stop();
        anyhow::bail!(
            "llama-server did not become ready within 5 minutes.\n{}",
            log_tail(log_path)
        )
    }

    pub fn stop(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
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
    #[test]
    fn intel_mac_defaults_to_cpu_only() {
        let args = server_args(&config(), true);
        let joined = args.join(" ");
        assert!(joined.contains("-ngl 0"));
        assert!(joined.contains("--device none"));
        assert!(joined.contains("--host 127.0.0.1"));
        assert!(joined.contains("--alias qwen2.5-coder-3b-q4_k_m"));
    }
    #[test]
    fn other_platforms_keep_server_gpu_default() {
        let args = server_args(&config(), false).join(" ");
        assert!(!args.contains("-ngl"));
        assert!(!args.contains("--device"));
    }
    #[test]
    fn explicit_gpu_layers_override_intel_default() {
        let mut c = config();
        c.llama_gpu_layers = Some(20);
        c.llama_threads = 4;
        let args = server_args(&c, true).join(" ");
        assert!(args.contains("-ngl 20"));
        assert!(args.contains("-t 4"));
        assert!(!args.contains("--device none"));
    }
}
