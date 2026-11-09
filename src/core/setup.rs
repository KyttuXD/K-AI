//! First-run setup: finds or downloads llama.cpp's `llama-server` and a GGUF
//! model sized for this machine's RAM, with resumable, verified downloads.
use super::config::{Config, data_dir};
use super::llama;
use anyhow::{Context, Result, bail, ensure};
use reqwest::{StatusCode, blocking::Client};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// The llama.cpp release downloaded when no llama-server is installed.
pub const LLAMA_TAG: &str = "b12110";

pub struct ModelChoice {
    pub name: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    pub min_ram_gb: u64,
}

/// MODELS[0] is the primary model: what automatic setup downloads on every computer
/// unless another one is chosen in Settings.
pub const MODELS: &[ModelChoice] = &[
    ModelChoice {
        name: "Llama 3.2 1B (Q4_K_M)",
        file: "Llama-3.2-1B-Instruct-Q4_K_M.gguf",
        url: "https://huggingface.co/bartowski/Llama-3.2-1B-Instruct-GGUF/resolve/main/Llama-3.2-1B-Instruct-Q4_K_M.gguf",
        size: 807694464,
        sha256: "6f85a640a97cf2bf5b8e764087b1e83da0fdb51d7c9fab7d0fece9385611df83",
        min_ram_gb: 0,
    },
    ModelChoice {
        name: "Qwen2.5 Coder 1.5B (Q4_K_M)",
        file: "Qwen2.5-Coder-1.5B-Instruct-Q4_K_M.gguf",
        url: "https://huggingface.co/bartowski/Qwen2.5-Coder-1.5B-Instruct-GGUF/resolve/main/Qwen2.5-Coder-1.5B-Instruct-Q4_K_M.gguf",
        size: 986048800,
        sha256: "f530705d447660a4336c329981af164b471b60b974b1d808d57e8ec9fe23b239",
        min_ram_gb: 0,
    },
    ModelChoice {
        name: "Qwen2.5 Coder 3B (Q4_K_M)",
        file: "Qwen2.5-Coder-3B-Instruct-Q4_K_M.gguf",
        url: "https://huggingface.co/bartowski/Qwen2.5-Coder-3B-Instruct-GGUF/resolve/main/Qwen2.5-Coder-3B-Instruct-Q4_K_M.gguf",
        size: 1929903360,
        sha256: "3da3afe6cf5c674ac195803ea0dd6fee7e1c228c2105c1ce8c66890d1d4ab460",
        min_ram_gb: 8,
    },
    ModelChoice {
        name: "Qwen2.5 Coder 7B (Q4_K_M)",
        file: "Qwen2.5-Coder-7B-Instruct-Q4_K_M.gguf",
        url: "https://huggingface.co/bartowski/Qwen2.5-Coder-7B-Instruct-GGUF/resolve/main/Qwen2.5-Coder-7B-Instruct-Q4_K_M.gguf",
        size: 4683074336,
        sha256: "1664fccab734674a50763490a8c6931b70e3f2f8ec10031b54806d30e5f956b6",
        min_ram_gb: 16,
    },
];

/// Embedding model for document rerank with llama.cpp (`llama-server --embedding`).
pub const EMBED_MODEL: ModelChoice = ModelChoice {
    name: "nomic-embed-text v1.5 (Q8_0)",
    file: "nomic-embed-text-v1.5.Q8_0.gguf",
    url: "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5-GGUF/resolve/main/nomic-embed-text-v1.5.Q8_0.gguf",
    size: 146146432,
    sha256: "3e24342164b3d94991ba9692fdc0dd08e3fd7362e0aacc396a9a5c54a544c3b7",
    min_ram_gb: 0,
};

/// Installed RAM in whole GiB (rounded, so a "16 GB" machine reporting 15.8 counts as 16).
pub fn total_ram_gb() -> Option<u64> {
    static RAM: OnceLock<Option<u64>> = OnceLock::new();
    *RAM.get_or_init(|| total_ram_bytes().map(|b| (b + (1 << 29)) >> 30))
}

/// False on an x86 CPU without AVX2 (older Intel), which needs the CPU-only llama.cpp build.
pub fn has_avx2() -> bool {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    {
        is_x86_feature_detected!("avx2")
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
    {
        true
    }
}

/// Physical CPU cores (llama.cpp runs best with one thread per core), or the
/// logical count when it cannot be read.
pub fn physical_cores() -> usize {
    static CORES: OnceLock<usize> = OnceLock::new();
    *CORES.get_or_init(|| {
        physical_core_count()
            .filter(|&n| n > 0)
            .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
            .unwrap_or(1)
    })
}

#[cfg(windows)]
fn physical_core_count() -> Option<usize> {
    // SYSTEM_LOGICAL_PROCESSOR_INFORMATION: a mask, the relationship, then a 16-byte union.
    #[repr(C)]
    #[allow(dead_code)]
    struct Info {
        mask: usize,
        relationship: i32,
        rest: [u64; 2],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalProcessorInformation(buffer: *mut Info, length: *mut u32) -> i32;
    }
    let mut len = 0u32;
    // SAFETY: a null buffer asks for the required length only.
    unsafe { GetLogicalProcessorInformation(std::ptr::null_mut(), &mut len) };
    let count = len as usize / size_of::<Info>();
    if count == 0 {
        return None;
    }
    let mut buf: Vec<Info> = (0..count)
        .map(|_| Info {
            mask: 0,
            relationship: 0,
            rest: [0; 2],
        })
        .collect();
    // SAFETY: `buf` holds `len` bytes of correctly laid out entries.
    let ok = unsafe { GetLogicalProcessorInformation(buf.as_mut_ptr(), &mut len) } != 0;
    let filled = (len as usize / size_of::<Info>()).min(count);
    // RelationProcessorCore = 0: one entry per physical core.
    ok.then(|| buf[..filled].iter().filter(|i| i.relationship == 0).count())
}

#[cfg(target_os = "macos")]
fn physical_core_count() -> Option<usize> {
    // Performance cores only on Apple Silicon (as llama.cpp's own default):
    // threads on the efficiency cores hold back every barrier. Intel Macs have
    // no perflevel0 key and fall back to all physical cores.
    ["hw.perflevel0.physicalcpu", "hw.physicalcpu"]
        .iter()
        .find_map(|key| {
            let out = Command::new("sysctl").args(["-n", key]).output().ok()?;
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .ok()
                .filter(|&n: &usize| n > 0)
        })
}

#[cfg(not(any(windows, target_os = "macos")))]
fn physical_core_count() -> Option<usize> {
    parse_cpuinfo_cores(&fs::read_to_string("/proc/cpuinfo").ok()?)
}

/// Distinct (physical id, core id) pairs in /proc/cpuinfo.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn parse_cpuinfo_cores(text: &str) -> Option<usize> {
    let mut cores = std::collections::BTreeSet::new();
    let (mut package, mut core) = (None, None);
    for line in text.lines().chain([""]) {
        let value = |l: &str| l.split_once(':').map(|(_, v)| v.trim().to_string());
        if line.starts_with("physical id") {
            package = value(line);
        } else if line.starts_with("core id") {
            core = value(line);
        } else if line.trim().is_empty() {
            if let Some(c) = core.take() {
                cores.insert((package.take(), c));
            }
            package = None;
        }
    }
    (!cores.is_empty()).then_some(cores.len())
}

#[cfg(windows)]
fn total_ram_bytes() -> Option<u64> {
    // MEMORYSTATUSEX: two u32s, then seven u64s starting with ullTotalPhys.
    #[repr(C)]
    #[allow(dead_code)]
    struct MemoryStatusEx {
        length: u32,
        load: u32,
        total_phys: u64,
        rest: [u64; 6],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx {
        length: size_of::<MemoryStatusEx>() as u32,
        load: 0,
        total_phys: 0,
        rest: [0; 6],
    };
    // SAFETY: `status` is a correctly sized MEMORYSTATUSEX with dwLength set.
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status.total_phys)
}

#[cfg(target_os = "macos")]
fn total_ram_bytes() -> Option<u64> {
    let out = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn total_ram_bytes() -> Option<u64> {
    parse_meminfo(&fs::read_to_string("/proc/meminfo").ok()?)
}

/// Bytes of MemTotal in /proc/meminfo.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn parse_meminfo(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// The model automatic setup downloads by default on every computer: Llama 3.2 1B.
/// Small and fast enough for old Intel Macs; larger ones can be picked in Settings.
pub fn recommended() -> &'static ModelChoice {
    &MODELS[0]
}

/// The largest model that fits this machine's RAM (shown as an option, not downloaded by default).
pub fn best_fit() -> &'static ModelChoice {
    pick_model(total_ram_gb(), llama::INTEL_MAC)
}

/// Unknown RAM picks a small model; Intel Macs never go above 3B (CPU only).
/// Among equal RAM tiers the later (coding) model wins.
fn pick_model(ram_gb: Option<u64>, intel_mac: bool) -> &'static ModelChoice {
    let ram = ram_gb.unwrap_or(0);
    let cap = if intel_mac { 8 } else { u64::MAX };
    MODELS
        .iter()
        .filter(|m| m.min_ram_gb <= ram.min(cap))
        .max_by_key(|m| m.min_ram_gb)
        .unwrap_or(&MODELS[0])
}

/// Settings suggested for this machine (Models page, first run).
#[derive(Clone, Copy)]
pub struct RecommendedSettings {
    pub threads: usize,
    pub context_tokens: usize,
    /// None = automatic (the server offloads what fits).
    pub gpu_layers: Option<i32>,
    pub model: &'static ModelChoice,
}

pub fn recommend_settings() -> RecommendedSettings {
    recommend(total_ram_gb(), physical_cores(), llama::INTEL_MAC)
}

/// One thread per physical core; context grows with RAM (unknown RAM is
/// treated as a small machine); Intel Macs run on the CPU and stop at 8k.
fn recommend(ram_gb: Option<u64>, cores: usize, intel_mac: bool) -> RecommendedSettings {
    let context = match ram_gb {
        None => 4096,
        Some(r) if r < 6 => 2048,
        Some(r) if r < 12 => 4096,
        Some(r) if r < 24 => 8192,
        Some(_) => 16384,
    };
    RecommendedSettings {
        threads: cores.max(1),
        context_tokens: if intel_mac {
            context.min(8192)
        } else {
            context
        },
        gpu_layers: intel_mac.then_some(0),
        model: pick_model(ram_gb, intel_mac),
    }
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

pub(crate) fn install_dir() -> PathBuf {
    data_dir().join("llama").join(LLAMA_TAG)
}

/// llama-server from a previous setup download, if any. The release layouts
/// are checked first; a folder walk is the fallback.
pub fn installed_server() -> Option<PathBuf> {
    let dir = install_dir();
    if !dir.is_dir() {
        return None;
    }
    [
        dir.join(llama::binary_name()),
        dir.join("build/bin").join(llama::binary_name()),
    ]
    .into_iter()
    .find(|p| p.is_file())
    .or_else(|| find_binary(&dir))
}

fn find_binary(dir: &Path) -> Option<PathBuf> {
    let name = std::ffi::OsStr::new(llama::binary_name());
    walkdir::WalkDir::new(dir)
        .max_depth(5)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| e.file_type().is_file() && e.file_name() == name)
        .map(|e| e.into_path())
}

/// The official release asset for an OS/arch pair (std::env::consts names).
/// Windows x64 without AVX2 gets the CPU build.
fn asset_for(os: &str, arch: &str, avx2: bool) -> Option<String> {
    let suffix = match (os, arch) {
        ("windows", "x86_64") if !avx2 => "bin-win-cpu-x64.zip",
        ("windows", "x86_64") => "bin-win-vulkan-x64.zip",
        ("windows", "aarch64") => "bin-win-cpu-arm64.zip",
        ("linux", "x86_64") => "bin-ubuntu-x64.tar.gz",
        ("linux", "aarch64") => "bin-ubuntu-arm64.tar.gz",
        ("macos", "aarch64") => "bin-macos-arm64.tar.gz",
        ("macos", "x86_64") => "bin-macos-x64.tar.gz",
        _ => return None,
    };
    Some(format!("llama-{LLAMA_TAG}-{suffix}"))
}

/// SHA-256 and size of every official asset setup can download at LLAMA_TAG
/// (the release's "digest" fields; scripts/fetch-llama.* pin the same hashes).
const PINNED: &[(&str, &str, u64)] = &[
    (
        "bin-win-vulkan-x64.zip",
        "9732a5e48887ca54d0bfe4af4324dc85fd56cd8cda19b52df3542fa6ebe6874f",
        37661146,
    ),
    (
        "bin-win-cpu-x64.zip",
        "98901e327c252875accaf2d1bf364e10e868843b0b63468baad1aa945cdc5da2",
        24708445,
    ),
    (
        "bin-win-cpu-arm64.zip",
        "f8d20fb07d0b499988ced8963afcd5ded2d2cdd9983b757fbc7e51c4abd722e8",
        13197637,
    ),
    (
        "bin-ubuntu-x64.tar.gz",
        "368227edcc229be5f9bed16a2223d79b5e41d0ac2e19bcf5825e82291c060e55",
        23051958,
    ),
    (
        "bin-ubuntu-arm64.tar.gz",
        "4758b51335e37b4346251f2687d64841ff1c07ecb27fc46323945bb2ec9c44d2",
        18723578,
    ),
    (
        "bin-macos-arm64.tar.gz",
        "4f5b8de12e0b9516658ca6a5764c6ffb3d7c659c3dd538556b1413c7b4050662",
        16650691,
    ),
    (
        "bin-macos-x64.tar.gz",
        "c8094268b6bf62f4998f0e45656fd9f5e2d4afb6b1cd0044d761db64be1dd47d",
        12388601,
    ),
];

/// (SHA-256, size) of an official asset at LLAMA_TAG.
fn pinned(asset: &str) -> Option<(&'static str, u64)> {
    let suffix = asset.strip_prefix(&format!("llama-{LLAMA_TAG}-"))?;
    PINNED
        .iter()
        .find(|(s, _, _)| *s == suffix)
        .map(|&(_, hash, size)| (hash, size))
}

/// Client for large downloads: no total timeout.
fn download_client() -> Result<Client> {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    llama::shared(&CLIENT, || {
        Client::builder()
            .user_agent("kyttu-ai")
            .connect_timeout(Duration::from_secs(30))
            .timeout(None::<Duration>)
            .build()
    })
}

#[derive(Clone, Default)]
pub struct SetupProgress {
    pub stage: String,
    pub done: u64,
    pub total: u64,
    pub finished: bool,
    pub error: Option<String>,
}

pub struct SetupResult {
    pub server: PathBuf,
    pub model: PathBuf,
}

/// Finds or downloads llama-server, then downloads the recommended model unless
/// config.gguf_model already exists. Blocking: run it on a worker thread and
/// poll `progress`; set `cancel` to stop (partial downloads resume next time).
/// `progress.finished` is set at the end, with `error` on failure.
pub fn auto_setup(
    config: &Config,
    progress: Arc<Mutex<SetupProgress>>,
    cancel: Arc<AtomicBool>,
) -> Result<SetupResult> {
    let result = run_setup(config, &progress, &cancel);
    llama::rescan_server();
    let mut p = progress.lock().unwrap();
    p.finished = true;
    if let Err(e) = &result {
        p.error = Some(format!("{e:#}"));
    }
    result
}

fn run_setup(
    config: &Config,
    progress: &Mutex<SetupProgress>,
    cancel: &AtomicBool,
) -> Result<SetupResult> {
    let client = download_client()?;
    set_stage(progress, "Looking for llama-server");
    let explicit = !config.llama_server.as_os_str().is_empty();
    let found = llama::find_server(config)
        .filter(|p| explicit || llama::server_version(p).is_some())
        .or_else(|| installed_server().filter(|p| llama::server_version(p).is_some()));
    let server = match found {
        Some(path) => path,
        None => install_server(&client, progress, cancel)?,
    };
    check_cancel(cancel)?;
    let (model, note) = if config.gguf_model.is_file() {
        (config.gguf_model.clone(), None)
    } else {
        let choice = MODELS
            .iter()
            .find(|m| m.file == config.setup_model)
            .unwrap_or_else(recommended);
        download_model(&client, choice, progress, cancel)?
    };
    set_stage(
        progress,
        &match note {
            Some(note) => format!("Ready ({note})"),
            None => "Ready".into(),
        },
    );
    Ok(SetupResult { server, model })
}

fn set_stage(progress: &Mutex<SetupProgress>, stage: &str) {
    let mut p = progress.lock().unwrap();
    p.stage = stage.into();
    p.done = 0;
    p.total = 0;
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "Setup cancelled");
    Ok(())
}

/// Prefers the Windows system copy of a tool (bsdtar reads .zip; a Git or
/// MSYS tar earlier on PATH does not).
fn system_tool(name: &str) -> PathBuf {
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        let path = PathBuf::from(root)
            .join("System32")
            .join(format!("{name}.exe"));
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from(name)
}

fn install_server(
    client: &Client,
    progress: &Mutex<SetupProgress>,
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    use std::env::consts::{ARCH, OS};
    let asset = asset_for(OS, ARCH, has_avx2()).with_context(|| {
        format!("No prebuilt llama.cpp for {OS} {ARCH}. Install llama.cpp and choose llama-server in Settings.")
    })?;
    let (sha256, size) =
        pinned(&asset).with_context(|| format!("No checksum is pinned for {asset}"))?;
    let url =
        format!("https://github.com/ggml-org/llama.cpp/releases/download/{LLAMA_TAG}/{asset}");
    let root = data_dir().join("llama");
    fs::create_dir_all(&root)?;
    let archive = root.join(&asset);
    // An archive left by an older version may not have been verified yet.
    if archive.is_file() && !sha256_file(&archive).is_some_and(|h| h.eq_ignore_ascii_case(sha256)) {
        let _ = fs::remove_file(&archive);
    }
    fetch_verified(
        client,
        &url,
        size,
        Some(sha256),
        true,
        &archive,
        "Downloading llama.cpp",
        progress,
        cancel,
    )?;
    check_cancel(cancel)?;
    set_stage(progress, "Extracting llama.cpp");
    let staging = root.join(format!("{LLAMA_TAG}.partial"));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let out = llama::quiet(
        Command::new(system_tool("tar"))
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .stdin(Stdio::null()),
    )
    .output()
    .context("Could not run tar to extract llama.cpp")?;
    if !out.status.success() {
        let _ = fs::remove_file(&archive);
        let _ = fs::remove_dir_all(&staging);
        bail!(
            "Extracting {asset} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let target = install_dir();
    let _ = fs::remove_dir_all(&target);
    fs::rename(&staging, &target)?;
    let _ = fs::remove_file(&archive);
    let server = find_binary(&target)
        .with_context(|| format!("{asset} did not contain {}", llama::binary_name()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&server, fs::Permissions::from_mode(0o755))?;
    }
    set_stage(progress, "Checking llama-server");
    if llama::probe_version(&server).is_none() {
        bail!(not_runnable(&server));
    }
    // Later launches read the tag instead of running `--version` again.
    let _ = fs::write(server.with_file_name(llama::TAG_FILE), LLAMA_TAG);
    Ok(server)
}

fn not_runnable(server: &Path) -> String {
    let fix = if cfg!(target_os = "macos") {
        "install one with `brew install llama.cpp`, or build llama.cpp with -DCMAKE_OSX_DEPLOYMENT_TARGET=10.15 -DGGML_METAL=OFF"
    } else {
        "install llama.cpp from your package manager or build it from source"
    };
    let why = if llama::INTEL_MAC {
        "The official llama.cpp build for Intel Macs needs a newer macOS than this Mac runs."
    } else {
        "The downloaded llama-server does not run on this system."
    };
    format!(
        "{why} ({})\nThe KYTTU app bundle ships a compatible llama-server: reinstall the app from its release, or {fix}, then choose llama-server in Settings.",
        server.display()
    )
}

fn download_model(
    client: &Client,
    choice: &ModelChoice,
    progress: &Mutex<SetupProgress>,
    cancel: &AtomicBool,
) -> Result<(PathBuf, Option<&'static str>)> {
    let dir = models_dir();
    fs::create_dir_all(&dir)?;
    let dest = dir.join(choice.file);
    let note = fetch_verified(
        client,
        choice.url,
        choice.size,
        Some(choice.sha256),
        false,
        &dest,
        &format!("Downloading {}", choice.name),
        progress,
        cancel,
    )?;
    Ok((dest, note))
}

/// Downloads `url` to `dest` with resume, checks the size (when non-zero) and
/// SHA-256 (when given), and only then gives it the final name. Without a
/// system hash tool a known size is accepted. Blocking: run it on a worker
/// thread and poll `progress`; `cancel` stops it (the .part file resumes later).
pub fn download_verified(
    url: &str,
    size: u64,
    sha256: Option<&str>,
    dest: &Path,
    progress: &Mutex<SetupProgress>,
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    fetch_verified(
        &download_client()?,
        url,
        size,
        sha256,
        false,
        dest,
        &format!("Downloading {name}"),
        progress,
        cancel,
    )?;
    Ok(dest.to_path_buf())
}

/// The download behind `download_verified`. `require_hash` refuses a file
/// that cannot be hashed; otherwise that case returns a note.
#[allow(clippy::too_many_arguments)]
fn fetch_verified(
    client: &Client,
    url: &str,
    size: u64,
    sha256: Option<&str>,
    require_hash: bool,
    dest: &Path,
    stage: &str,
    progress: &Mutex<SetupProgress>,
    cancel: &AtomicBool,
) -> Result<Option<&'static str>> {
    let expected = (size > 0).then_some(size);
    // Only verified downloads are renamed to the final name.
    if let Some(n) = expected
        && fs::metadata(dest).is_ok_and(|m| m.len() == n)
    {
        return Ok(None);
    }
    let part = download(client, url, dest, expected, stage, progress, cancel)?;
    let file = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut note = None;
    if let Some(want) = sha256 {
        set_stage(progress, "Verifying checksum");
        match sha256_file(&part) {
            Some(hash) if hash.eq_ignore_ascii_case(want) => {}
            Some(hash) => {
                let _ = fs::remove_file(&part);
                bail!(
                    "Checksum mismatch for {file} (got {hash}, expected {want}). The partial file was removed; try again."
                );
            }
            None if require_hash || expected.is_none() => bail!(
                "Could not compute the SHA-256 of {file} (no certutil, sha256sum or shasum found)."
            ),
            None => note = Some("no checksum tool found; verified by file size"),
        }
    }
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::rename(&part, dest)?;
    Ok(note)
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

/// Downloads `url` to `<dest>.part`, resuming with an HTTP Range request, and
/// returns the completed .part path.
fn download(
    client: &Client,
    url: &str,
    dest: &Path,
    expected: Option<u64>,
    stage: &str,
    progress: &Mutex<SetupProgress>,
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    let part = part_path(dest);
    let mut have = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if expected.is_some_and(|n| have > n) {
        fs::remove_file(&part)?;
        have = 0;
    }
    if have > 0 && expected == Some(have) {
        return Ok(part);
    }
    set_stage(progress, stage);
    let mut request = client.get(url);
    if have > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let mut response = request
        .send()
        .with_context(|| format!("Could not download {url}"))?;
    let status = response.status();
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        let _ = fs::remove_file(&part);
        bail!("The server refused to resume {url}; run setup again to restart the download.");
    }
    ensure!(status.is_success(), "Download failed ({status}): {url}");
    let resumed = status == StatusCode::PARTIAL_CONTENT;
    if !resumed {
        have = 0;
    }
    let total = response
        .content_length()
        .map(|n| n + have)
        .or(expected)
        .unwrap_or(0);
    if let Some(n) = expected {
        ensure!(
            total == n,
            "{url} is {total} bytes, expected {n}; the file may have changed upstream."
        );
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(&part)?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = have;
    {
        let mut p = progress.lock().unwrap();
        p.done = done;
        p.total = total;
    }
    loop {
        check_cancel(cancel)?;
        let n = response
            .read(&mut buf)
            .context("Download interrupted; run setup again to resume")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        let mut p = progress.lock().unwrap();
        p.done = done;
        p.total = total;
    }
    file.flush()?;
    if total > 0 {
        ensure!(
            done == total,
            "Downloaded {done} of {total} bytes; run setup again to resume"
        );
    }
    Ok(part)
}

/// SHA-256 of a file via the OS tool, or None when no tool is available.
fn sha256_file(path: &Path) -> Option<String> {
    let mut commands = vec![];
    if cfg!(windows) {
        let mut c = Command::new(system_tool("certutil"));
        c.arg("-hashfile").arg(path).arg("SHA256");
        commands.push(c);
    } else {
        if !cfg!(target_os = "macos") {
            let mut c = Command::new("sha256sum");
            c.arg(path);
            commands.push(c);
        }
        let mut c = Command::new("shasum");
        c.args(["-a", "256"]).arg(path);
        commands.push(c);
    }
    commands.into_iter().find_map(|mut c| {
        let out = llama::quiet(&mut c).stdin(Stdio::null()).output().ok()?;
        out.status
            .success()
            .then(|| parse_hash_output(&String::from_utf8_lossy(&out.stdout)))
            .flatten()
    })
}

/// Finds the hex digest in certutil (optionally space-separated bytes),
/// shasum or sha256sum output.
fn parse_hash_output(text: &str) -> Option<String> {
    let is_hash = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    text.lines().find_map(|line| {
        // sha256sum prefixes a backslash when the file name has escapes.
        let first = line.split_whitespace().next()?.trim_start_matches('\\');
        if is_hash(first) {
            return Some(first.to_ascii_lowercase());
        }
        let joined: String = line.split_whitespace().collect();
        is_hash(&joined).then(|| joined.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn picks_an_asset_per_target() {
        let asset = |os, arch| asset_for(os, arch, true);
        assert_eq!(
            asset("windows", "x86_64").as_deref(),
            Some("llama-b12110-bin-win-vulkan-x64.zip")
        );
        assert_eq!(
            asset_for("windows", "x86_64", false).as_deref(),
            Some("llama-b12110-bin-win-cpu-x64.zip")
        );
        assert_eq!(
            asset("windows", "aarch64").as_deref(),
            Some("llama-b12110-bin-win-cpu-arm64.zip")
        );
        assert_eq!(
            asset("linux", "x86_64").as_deref(),
            Some("llama-b12110-bin-ubuntu-x64.tar.gz")
        );
        assert_eq!(
            asset("linux", "aarch64").as_deref(),
            Some("llama-b12110-bin-ubuntu-arm64.tar.gz")
        );
        assert_eq!(
            asset("macos", "aarch64").as_deref(),
            Some("llama-b12110-bin-macos-arm64.tar.gz")
        );
        assert_eq!(
            asset("macos", "x86_64").as_deref(),
            Some("llama-b12110-bin-macos-x64.tar.gz")
        );
        assert_eq!(asset("freebsd", "x86_64"), None);
    }
    #[test]
    fn every_downloadable_asset_is_pinned() {
        for (os, arch) in [
            ("windows", "x86_64"),
            ("windows", "aarch64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "aarch64"),
            ("macos", "x86_64"),
        ] {
            for avx2 in [true, false] {
                let (hash, size) = asset_for(os, arch, avx2)
                    .and_then(|a| pinned(&a))
                    .unwrap_or_default();
                assert!(
                    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && size > 0,
                    "{os} {arch} avx2={avx2}"
                );
            }
        }
        assert_eq!(pinned("llama-b1-bin-win-vulkan-x64.zip"), None);
    }
    #[test]
    fn recommends_settings_per_machine() {
        let small = recommend(Some(4), 2, false);
        assert_eq!((small.threads, small.context_tokens), (2, 2048));
        assert_eq!(small.gpu_layers, None);
        assert_eq!(small.model.min_ram_gb, 0);
        let big = recommend(Some(32), 8, false);
        assert_eq!(big.context_tokens, 16384);
        assert_eq!(big.model.min_ram_gb, 16);
        let mac = recommend(Some(32), 4, true);
        assert_eq!((mac.context_tokens, mac.gpu_layers), (8192, Some(0)));
        assert_eq!(recommend(None, 0, false).threads, 1);
        assert!(physical_cores() >= 1);
    }
    #[test]
    fn counts_physical_cores_in_cpuinfo() {
        let two_threads_per_core = "processor\t: 0\nphysical id\t: 0\ncore id\t: 0\n\nprocessor\t: 1\nphysical id\t: 0\ncore id\t: 1\n\nprocessor\t: 2\nphysical id\t: 0\ncore id\t: 0\n\nprocessor\t: 3\nphysical id\t: 0\ncore id\t: 1\n";
        assert_eq!(parse_cpuinfo_cores(two_threads_per_core), Some(2));
        assert_eq!(parse_cpuinfo_cores("processor : 0\n"), None);
    }
    #[test]
    fn recommends_by_ram() {
        assert_eq!(recommended().file, "Llama-3.2-1B-Instruct-Q4_K_M.gguf");
        assert_eq!(pick_model(None, false).min_ram_gb, 0);
        assert_eq!(pick_model(Some(4), false).min_ram_gb, 0);
        assert_eq!(pick_model(Some(8), false).min_ram_gb, 8);
        assert_eq!(pick_model(Some(15), false).min_ram_gb, 8);
        assert_eq!(pick_model(Some(16), false).min_ram_gb, 16);
        assert_eq!(pick_model(Some(64), false).min_ram_gb, 16);
        // Intel Macs stay at 3B however much RAM they have.
        assert_eq!(pick_model(Some(64), true).min_ram_gb, 8);
        assert_eq!(pick_model(Some(4), true).min_ram_gb, 0);
    }
    #[test]
    fn parses_hash_tool_output() {
        let hash = "f530705d447660a4336c329981af164b471b60b974b1d808d57e8ec9fe23b239";
        let certutil = format!(
            "SHA256 hash of C:\\models\\a.gguf:\r\n{hash}\r\nCertUtil: -hashfile command completed successfully.\r\n"
        );
        assert_eq!(parse_hash_output(&certutil).as_deref(), Some(hash));
        let spaced: String = hash
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            parse_hash_output(&format!(
                "SHA256 hash of a.gguf:\n{}\n",
                spaced.to_uppercase()
            ))
            .as_deref(),
            Some(hash)
        );
        assert_eq!(
            parse_hash_output(&format!("{hash}  /m/a.gguf\n")).as_deref(),
            Some(hash)
        );
        assert_eq!(
            parse_hash_output(&format!("\\{hash}  /m/a\\nb.gguf\n")).as_deref(),
            Some(hash)
        );
        assert_eq!(parse_hash_output("shasum: a.gguf: No such file"), None);
    }
    #[test]
    fn parses_meminfo() {
        let text = "MemTotal:       16301332 kB\nMemFree:         1234 kB\n";
        assert_eq!(parse_meminfo(text), Some(16301332 * 1024));
        assert_eq!(parse_meminfo("nothing"), None);
    }
    #[test]
    fn model_table_is_consistent() {
        for m in MODELS.iter().chain([&EMBED_MODEL]) {
            assert!(m.url.ends_with(m.file));
            assert_eq!(m.sha256.len(), 64);
            assert!(m.size > 0);
        }
        assert_eq!(
            part_path(Path::new("/m/a.gguf")),
            PathBuf::from("/m/a.gguf.part")
        );
    }
}
