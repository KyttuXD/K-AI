//! Built-in llama.cpp runtime: launches `llama-server` with a chosen GGUF file
//! on a loopback port, so the app can run models without Ollama, plus a second
//! server with `--embedding` for document rerank. Defaults are tuned for Intel
//! Macs (CPU inference, no Metal offload) and other low-end machines.
use super::config::{Config, data_dir};
use super::setup;
use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, LazyLock, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// True on x86_64 macOS, where Metal offload is slow or unavailable and
/// llama.cpp should run on the CPU.
pub const INTEL_MAC: bool = cfg!(all(target_os = "macos", target_arch = "x86_64"));

/// Oldest llama.cpp builds that accept these flags; older builds exit on an
/// unknown flag, so the flag is left out for them.
const MIN_JINJA: u32 = 4600;
const MIN_DEVICE: u32 = 4400;
const MIN_CACHE_REUSE: u32 = 4000;
/// From here flash attention takes on/off/auto, which a quantized V cache needs.
const MIN_FLASH_AUTO: u32 = 6325;
const MIN_METRICS: u32 = 2500;
const MIN_SLOT_SAVE: u32 = 3100;
const MIN_DRAFT_MAX: u32 = 4300;
/// llama-server takes images (`--mmproj`, `--no-mmproj-offload`) from here.
const MIN_MMPROJ: u32 = 5400;
/// `--cache-ram` (host-RAM prompt cache, on by default at 8 GiB) and `--fit`:
/// passed only to builds known to have them (the bundled tag), since an older
/// or unknown build would exit on the unknown flag.
const MIN_CACHE_RAM: u32 = 12110;
const MIN_FIT: u32 = 12110;
/// Below this GGUF size a q8_0 KV cache saves little RAM (tens of MB for a 1B
/// model) and costs some decode speed on the CPU, so it stays f16.
const SMALL_MODEL_BYTES: u64 = 2_000_000_000;

/// Written next to a llama-server whose release tag is known (setup's download,
/// the packaged llama/ folders), so its version needs no `--version` probe.
pub(crate) const TAG_FILE: &str = "kyttu-llama-tag";
/// Alias of the embedding server's model.
pub const EMBED_ALIAS: &str = "kyttu-embed";

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

/// One reqwest client per call site, built on first use and shared after.
pub(crate) fn shared(
    cell: &'static OnceLock<Client>,
    build: impl FnOnce() -> reqwest::Result<Client>,
) -> Result<Client> {
    if let Some(client) = cell.get() {
        return Ok(client.clone());
    }
    let client = build()?;
    Ok(cell.get_or_init(|| client).clone())
}

/// Client for the local servers: no proxy, short timeout (slot files override it).
fn local_client() -> Result<Client> {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    shared(&CLIENT, || {
        Client::builder()
            .no_proxy()
            // A bound loopback port connects at once; on Windows a refused one
            // (server still starting) could otherwise stall the whole 2 s.
            .connect_timeout(Duration::from_millis(250))
            .timeout(Duration::from_secs(2))
            .build()
    })
}

/// 64-bit FNV-1a: a stable hash for cache keys and file names.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Per-model overrides of the global llama.cpp settings, keyed in
/// `config.model_presets` by the GGUF file name.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ModelPreset {
    pub context_tokens: Option<usize>,
    pub threads: Option<usize>,
    pub gpu_layers: Option<i32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub repeat_penalty: Option<f32>,
    /// Small GGUF of the same family for speculative decoding.
    pub draft_model: Option<PathBuf>,
    /// Vision projector (mmproj GGUF); found next to the model when unset.
    #[serde(default)]
    pub mmproj: Option<PathBuf>,
}

/// True for a vision projector GGUF (its file name contains "mmproj").
pub fn is_mmproj(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().to_lowercase().contains("mmproj"))
}

/// True for a quantization or precision tag such as Q4_K_M, IQ4_XS, f16, bf16.
pub(crate) fn is_quant(token: &str) -> bool {
    let t = token.to_ascii_lowercase();
    if matches!(t.as_str(), "f16" | "f32" | "bf16" | "fp16" | "fp32" | "q8") {
        return true;
    }
    let rest = t.strip_prefix("iq").or_else(|| t.strip_prefix('q'));
    rest.is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
}

/// Lowercase file stem without trailing quant tags and without "mmproj":
/// "Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf" and
/// "mmproj-Qwen2.5-VL-3B-Instruct-f16.gguf" both give "qwen2.5-vl-3b-instruct".
pub(crate) fn base_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .replace("mmproj", "");
    let seps: &[char] = &['-', '.', '_', ' '];
    let mut base = stem.trim_matches(seps).to_string();
    while let Some(cut) = base.rfind(['-', '.']) {
        if !is_quant(&base[cut + 1..]) {
            break;
        }
        base = base[..cut].trim_end_matches(seps).to_string();
    }
    // "mmproj-f16.gguf" names no model at all.
    if is_quant(&base) {
        base.clear();
    }
    base
}

/// A projector name that names no model ("mmproj-model-f16.gguf", "mmproj.gguf").
fn generic_projector(base: &str) -> bool {
    base.len() < 3 || base == "model"
}

/// The vision projector for `model`: a *.gguf in the same folder whose name
/// contains "mmproj" and shares the model's base name (quant tags stripped),
/// else the only projector in that folder when its name names no model.
/// (The preset's projector wins over this; see `mmproj_for`.)
pub fn find_mmproj(model: &Path) -> Option<PathBuf> {
    if is_mmproj(model) {
        return None;
    }
    let dir = model.parent()?;
    let ggufs: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("gguf"))
        })
        .collect();
    let (projectors, models): (Vec<PathBuf>, Vec<PathBuf>) =
        ggufs.into_iter().partition(|p| is_mmproj(p));
    // A generic projector is only safe to pair when no other model shares the folder.
    let lone_model = models.len() <= 1;
    pick_mmproj(model, projectors, lone_model)
}

/// Whether two base names name the same model: the longer one starts with the
/// shorter one followed by nothing or a separator ("qwen2.5-vl-7b" is not
/// "qwen2.5-vl-72b", but it is "qwen2.5-vl-7b-instruct").
pub(crate) fn same_base(a: &str, b: &str) -> bool {
    let (s, l) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    l.starts_with(s) && (l.len() == s.len() || l[s.len()..].starts_with(['-', '.', '_']))
}

/// The choice behind `find_mmproj`, kept separate so it can be tested.
/// `lone_model`: the model is the only non-projector GGUF in its folder.
fn pick_mmproj(model: &Path, mut projectors: Vec<PathBuf>, lone_model: bool) -> Option<PathBuf> {
    let base = base_name(model);
    if base.is_empty() {
        return None;
    }
    // f16 before q8_0 before others, then by name, so the pick is stable.
    projectors.sort_by_key(|p| {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        (!name.contains("f16"), !name.contains("q8"), name)
    });
    let matching = projectors.iter().find(|p| {
        let b = base_name(p);
        !generic_projector(&b) && same_base(&base, &b)
    });
    if let Some(p) = matching {
        return Some(p.clone());
    }
    match projectors.as_slice() {
        [only] if lone_model && generic_projector(&base_name(only)) => Some(only.clone()),
        _ => None,
    }
}

/// The projector a launch of the selected model uses: the preset's when it is
/// a file, else one found next to the model.
pub fn mmproj_for(config: &Config) -> Option<PathBuf> {
    preset(config)
        .and_then(|p| p.mmproj.clone())
        .filter(|p| p.is_file())
        .or_else(|| find_mmproj(&config.gguf_model))
}

/// The preset of the selected GGUF file, if any.
pub fn preset(config: &Config) -> Option<&ModelPreset> {
    let name = config.gguf_model.file_name()?.to_str()?;
    config.model_presets.get(name)
}

/// The settings a launch uses: the globals, then the low-end profile, then the
/// selected model's preset.
pub fn effective(config: &Config) -> Config {
    let mut c = config.clone();
    if c.low_end {
        c.context_tokens = 2048;
        c.llama_threads = setup::physical_cores();
        // Keep half the context for the prompt, so a few turns of history fit
        // (and stay cached) and prompt plus reply cannot overrun the context.
        c.response_tokens = c.response_tokens.min(c.context_tokens / 2);
    }
    if let Some(p) = preset(config) {
        c.context_tokens = p.context_tokens.unwrap_or(c.context_tokens);
        c.llama_threads = p.threads.unwrap_or(c.llama_threads);
        c.llama_gpu_layers = p.gpu_layers.or(c.llama_gpu_layers);
        c.temperature = p.temperature.unwrap_or(c.temperature);
        c.top_p = p.top_p.unwrap_or(c.top_p);
        c.repeat_penalty = p.repeat_penalty.unwrap_or(c.repeat_penalty);
    }
    c
}

/// Bundled llama/ subfolders next to the app, best first. A CPU without AVX2
/// (older Intel) tries the CPU-only Windows build first.
fn bundled_dirs(avx2: bool) -> &'static [&'static str] {
    const DEFAULT: &[&str] = &["", "llama", "../Resources", "../Resources/llama"];
    const NO_AVX2: &[&str] = &[
        "llama/cpu",
        "",
        "llama",
        "../Resources",
        "../Resources/llama",
    ];
    if cfg!(windows) && !avx2 {
        NO_AVX2
    } else {
        DEFAULT
    }
}

/// Last scan for llama-server, reused for a while: tick() asks every few seconds.
type Found = Option<(Instant, Option<PathBuf>)>;
static FOUND: LazyLock<Mutex<Found>> = LazyLock::new(Default::default);
const FOUND_TTL: Duration = Duration::from_secs(30);

/// Forgets the cached search, e.g. after setup installed a server.
pub fn rescan_server() {
    *FOUND.lock().unwrap() = None;
}

/// Finds `llama-server`: the configured path, next to this app (including a
/// macOS bundle's `Contents/MacOS` or `Contents/Resources`), Homebrew, PATH,
/// then a copy downloaded by first-run setup. The search is cached for 30 s.
pub fn find_server(config: &Config) -> Option<PathBuf> {
    if !config.llama_server.as_os_str().is_empty() {
        return config
            .llama_server
            .is_file()
            .then(|| config.llama_server.clone());
    }
    let mut cache = FOUND.lock().unwrap();
    if let Some((at, found)) = cache.as_ref()
        && at.elapsed() < FOUND_TTL
        && found.as_ref().is_none_or(|p| p.is_file())
    {
        return found.clone();
    }
    let found = scan_server();
    *cache = Some((Instant::now(), found.clone()));
    found
}

fn scan_server() -> Option<PathBuf> {
    let mut candidates = vec![];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for sub in bundled_dirs(setup::has_avx2()) {
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
        .or_else(setup::installed_server)
}

/// Size and modification time of a binary; a changed file is probed again.
type Stamp = (u64, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64);
    Some((meta.len(), modified))
}

#[derive(Serialize, Deserialize)]
struct CachedVersion {
    path: PathBuf,
    len: u64,
    modified: u64,
    version: String,
}

fn versions_file() -> PathBuf {
    data_dir().join("llama-version.json")
}

/// Probed versions per server path. Versions that ran are also saved to disk,
/// so a launch does not start the (slow on Vulkan) `--version` probe again.
type Versions = HashMap<PathBuf, (Stamp, Option<String>)>;
static VERSIONS: LazyLock<Mutex<Versions>> = LazyLock::new(|| {
    let saved: Vec<CachedVersion> = fs::read(versions_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    Mutex::new(
        saved
            .into_iter()
            .map(|v| (v.path, ((v.len, v.modified), Some(v.version))))
            .collect(),
    )
});
/// Held while probing, so two threads never run `--version` at once.
static PROBING: Mutex<()> = Mutex::new(());

fn save_versions(cache: &HashMap<PathBuf, (Stamp, Option<String>)>) {
    let list: Vec<CachedVersion> = cache
        .iter()
        .filter_map(|(path, (stamp, version))| {
            Some(CachedVersion {
                path: path.clone(),
                len: stamp.0,
                modified: stamp.1,
                version: version.clone()?,
            })
        })
        .collect();
    if let Ok(bytes) = serde_json::to_vec(&list) {
        let _ = fs::create_dir_all(data_dir());
        let _ = fs::write(versions_file(), bytes);
    }
}

/// The release tag in a TAG_FILE next to the binary ("b12110").
fn tag_of(path: &Path) -> Option<String> {
    let tag = fs::read_to_string(path.with_file_name(TAG_FILE)).ok()?;
    let tag = tag.trim();
    build_number(tag).map(|_| tag.to_string())
}

/// The build reported by `llama-server --version` (for example "5186 (5fa07c2f)"),
/// a known release tag ("b12110"), or None when the binary does not run.
/// Cached per path, size and modification time, in memory and on disk.
pub fn server_version(path: &Path) -> Option<String> {
    if let Some(tag) = tag_of(path) {
        return Some(tag);
    }
    let stamp = stamp(path)?;
    let cached = || {
        VERSIONS
            .lock()
            .unwrap()
            .get(path)
            .filter(|(s, _)| *s == stamp)
            .map(|(_, v)| v.clone())
    };
    if let Some(version) = cached() {
        return version;
    }
    let _probing = PROBING.lock().unwrap();
    if let Some(version) = cached() {
        return version;
    }
    let version = probe_version(path);
    let mut cache = VERSIONS.lock().unwrap();
    cache.insert(path.to_path_buf(), (stamp, version.clone()));
    if version.is_some() {
        save_versions(&cache);
    }
    version
}

/// Runs `llama-server --version` (uncached). Setup uses it to check that a
/// fresh download runs on this system.
pub(crate) fn probe_version(path: &Path) -> Option<String> {
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

fn slots_dir() -> PathBuf {
    data_dir().join("slots")
}

/// Arguments for `llama-server`, from the effective settings. Kept separate so
/// it can be tested.
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
    let known = |min: u32| launch.build.is_some_and(|b| b >= min);
    if known(MIN_CACHE_RAM) {
        // Bound the host-RAM prompt cache (8 GiB by default) so memory stays
        // flat over many conversations; recent prompts still resume from it.
        args.extend([
            "--cache-ram".into(),
            cache_ram_mib(config.low_end).to_string(),
        ]);
    }
    if supports(MIN_FLASH_AUTO) {
        args.extend(["--flash-attn".into(), "auto".into()]);
    }
    if supports(MIN_METRICS) {
        args.push("--metrics".into());
    }
    if supports(MIN_SLOT_SAVE) {
        // The system prompt's KV cache is saved here and restored next launch.
        args.extend(["--slot-save-path".into(), slots_dir().display().to_string()]);
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
    let on_cpu = layers == Some(0);
    if on_cpu && known(MIN_FIT) {
        // -ngl and -c are set, so auto-fit has nothing to adjust: skip its dry run.
        args.extend(["--fit".into(), "off".into()]);
    }
    // Smaller batches keep the compute buffers small on CPU-only machines.
    let batch = if config.low_end {
        Some(256)
    } else if on_cpu {
        Some(512)
    } else {
        None
    };
    if let Some(n) = batch {
        args.extend(["-b".into(), n.to_string(), "-ub".into(), n.to_string()]);
    }
    // An unreadable path counts as big (the old behaviour).
    let big = !fs::metadata(&config.gguf_model).is_ok_and(|m| m.len() <= SMALL_MODEL_BYTES);
    if big && (config.low_end || (launch.intel_mac && config.context_tokens > 4096)) {
        // An 8-bit KV cache halves its RAM on long contexts.
        args.extend(["--cache-type-k".into(), "q8_0".into()]);
        if supports(MIN_FLASH_AUTO) {
            args.extend(["--cache-type-v".into(), "q8_0".into()]);
        }
    }
    if let Some(draft) = preset(config)
        .and_then(|p| p.draft_model.as_ref())
        .filter(|p| p.is_file())
    {
        // Speculative decoding: the draft model proposes tokens the main one checks.
        args.extend(["-md".into(), draft.display().to_string()]);
        if supports(MIN_DRAFT_MAX) {
            args.extend(["--draft-max".into(), "16".into()]);
        }
        if on_cpu {
            args.extend(["-ngld".into(), "0".into()]);
        }
    }
    if supports(MIN_MMPROJ)
        && let Some(mmproj) = mmproj_for(config)
    {
        // Vision: the projector encodes images for the model.
        args.extend(["--mmproj".into(), mmproj.display().to_string()]);
        if on_cpu || cpu {
            args.push("--no-mmproj-offload".into());
        }
    }
    args
}

/// Size of llama-server's host-RAM prompt cache in MiB: 256 on the low-end
/// profile, else an eighth of installed RAM within 256..=2048.
fn cache_ram_mib(low_end: bool) -> u64 {
    if low_end {
        return 256;
    }
    (setup::total_ram_gb().unwrap_or(4) * 1024 / 8).clamp(256, 2048)
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

fn embed_pid_path() -> PathBuf {
    data_dir().join("llama-embed.pid")
}

pub fn log_path() -> PathBuf {
    data_dir().join("llama-server.log")
}

/// Keeps the previous run's log as llama-server.prev.log.
fn rotate_log(path: &Path) {
    if path.is_file() {
        let _ = fs::rename(path, path.with_file_name("llama-server.prev.log"));
    }
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

/// Resident memory in KiB from `tasklist /FO CSV /NH` ("2,000,000 K", any
/// digit grouping) or `ps -o rss=` output.
#[cfg_attr(windows, allow(dead_code))]
fn parse_ram_kb(text: &str) -> Option<u64> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let field = match line.strip_suffix('"') {
        Some(rest) => rest.rsplit("\",\"").next()?,
        None => line,
    };
    if !field.ends_with('K') && !field.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let digits: String = field.chars().filter(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn is_llama_server(name: &str) -> bool {
    let name = name.trim().to_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name) == "llama-server"
}

fn tasklist(pid: u32) -> Option<String> {
    let filter = format!("PID eq {pid}");
    let out = quiet(Command::new("tasklist").args(["/FI", filter.as_str(), "/FO", "CSV", "/NH"]))
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn ps(pid: u32, field: &str) -> Option<String> {
    let out = Command::new("ps")
        .args(["-p", pid.to_string().as_str(), "-o", field])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn process_name(pid: u32) -> Option<String> {
    let text = if cfg!(windows) {
        tasklist(pid)?
    } else {
        ps(pid, "comm=")?
    };
    parse_process_name(&text)
}

/// Resident memory in KiB from the `VmRSS:` line of /proc/<pid>/status.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_vmrss_kb(text: &str) -> Option<u64> {
    let rest = text.lines().find_map(|l| l.strip_prefix("VmRSS:"))?;
    rest.trim().trim_end_matches("kB").trim().parse().ok()
}

/// Resident memory of `pid` in KiB without spawning a process on Linux
/// (/proc), else from `ps`. Windows reads the child's handle instead
/// (Process::ram_kb).
#[cfg(not(windows))]
fn process_ram_kb(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        if let Some(kb) = fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| parse_vmrss_kb(&s))
        {
            return Some(kb);
        }
    }
    parse_ram_kb(&ps(pid, "rss=")?)
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

/// Kills a llama-server left behind by a crash, using its PID file. Any other
/// process that has since reused the PID is left alone, and so is a server that
/// another running KYTTU instance still owns. (On Windows the Job Object
/// already kills it when the app dies; this covers the other platforms.)
fn kill_stale(pid_file: &Path) {
    let Some((pid, app)) = fs::read_to_string(pid_file)
        .ok()
        .and_then(|s| parse_pid_file(&s))
    else {
        let _ = fs::remove_file(pid_file);
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
    let _ = fs::remove_file(pid_file);
}

/// A Windows Job Object with KILL_ON_JOB_CLOSE: when the app exits or crashes,
/// the OS closes the handle and kills the server with it.
#[cfg(windows)]
struct Job(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a job handle is a kernel object handle, usable from any thread.
#[cfg(windows)]
unsafe impl Send for Job {}

#[cfg(windows)]
impl Job {
    fn assign(child: &Child) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // SAFETY: plain Win32 calls on a fresh job handle and the child's own
        // process handle, which stays open while `child` lives.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const std::ffi::c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0
                && AssignProcessToJobObject(job, child.as_raw_handle() as _) != 0;
            if !ok {
                CloseHandle(job);
                return None;
            }
            Some(Job(job))
        }
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateJobObjectW and is closed once.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(not(windows))]
#[allow(dead_code)]
struct Job;

#[cfg(not(windows))]
impl Job {
    fn assign(_: &Child) -> Option<Self> {
        None
    }
}

// An empty Drop keeps `drop(job)` meaningful on every platform: on Windows dropping the
// job closes its handle (killing the process tree); elsewhere there is nothing to close,
// and without this clippy rejects the call (drop_non_drop) on macOS and Linux CI.
#[cfg(not(windows))]
impl Drop for Job {
    fn drop(&mut self) {}
}

#[cfg(unix)]
fn not_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 == 0)
}

#[cfg(not(unix))]
fn not_executable(_: &Path) -> bool {
    false
}

const MAC_BLOCKED: &str =
    "Make it executable, or allow it in System Settings → Privacy & Security.";

/// Starts llama-server at a slightly lower priority than the app, so the UI
/// stays smooth while every core generates: BELOW_NORMAL on Windows, `nice -n 5`
/// elsewhere (nice execs, so the PID is still llama-server's).
fn spawn_server(server: &Path, args: &[String], log: &File) -> std::io::Result<Child> {
    let build = |nice: bool| -> std::io::Result<Child> {
        let mut cmd = if nice {
            let mut c = Command::new("nice");
            c.args(["-n", "5"]).arg(server);
            c
        } else {
            Command::new(server)
        };
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?);
        if let Some(dir) = server.parent() {
            // Release zips ship shared libraries beside the binary.
            cmd.current_dir(dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000 | 0x00004000);
        }
        cmd.spawn()
    };
    if cfg!(unix) {
        match build(true) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            result => return result,
        }
    }
    build(false)
}

/// A llama-server child process with its Job Object and PID file.
#[derive(Clone, Default)]
struct Process(Arc<Mutex<Option<Running>>>);
type Running = (Child, Option<Job>);

impl Process {
    /// None when nothing was started, Some(false) when it has just exited.
    fn alive(&self, pid_file: &Path) -> Option<bool> {
        let mut guard = self.0.lock().unwrap();
        match guard.as_mut().map(|(c, _)| c.try_wait()) {
            Some(Ok(None)) => Some(true),
            Some(_) => {
                *guard = None;
                let _ = fs::remove_file(pid_file);
                Some(false)
            }
            None => None,
        }
    }

    #[cfg_attr(windows, allow(dead_code))]
    fn pid(&self) -> Option<u32> {
        self.0.lock().unwrap().as_ref().map(|(c, _)| c.id())
    }

    /// Working set of the child in KiB, read from its own process handle, so
    /// no tasklist.exe is spawned for every status-bar sample.
    #[cfg(windows)]
    fn ram_kb(&self) -> Option<u64> {
        use std::os::windows::io::AsRawHandle;
        // PROCESS_MEMORY_COUNTERS: two u32s, then eight SIZE_Ts starting with
        // PeakWorkingSetSize and WorkingSetSize.
        #[repr(C)]
        #[allow(dead_code)]
        struct Counters {
            cb: u32,
            page_faults: u32,
            peak_working_set: usize,
            working_set: usize,
            rest: [usize; 6],
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn K32GetProcessMemoryInfo(
                process: *mut std::ffi::c_void,
                counters: *mut Counters,
                cb: u32,
            ) -> i32;
        }
        let guard = self.0.lock().unwrap();
        let (child, _) = guard.as_ref()?;
        let mut counters = Counters {
            cb: size_of::<Counters>() as u32,
            page_faults: 0,
            peak_working_set: 0,
            working_set: 0,
            rest: [0; 6],
        };
        // SAFETY: the child's handle stays open while the guard holds it, and
        // `counters` is a correctly sized PROCESS_MEMORY_COUNTERS.
        let ok = unsafe {
            K32GetProcessMemoryInfo(child.as_raw_handle() as _, &mut counters, counters.cb)
        } != 0;
        ok.then_some(counters.working_set as u64 / 1024)
    }

    #[cfg(not(windows))]
    fn ram_kb(&self) -> Option<u64> {
        process_ram_kb(self.pid()?)
    }

    fn spawn(&self, server: &Path, args: &[String], log: File, pid_file: &Path) -> Result<()> {
        let mac = cfg!(target_os = "macos");
        if not_executable(server) {
            if mac {
                bail!("macOS blocked llama-server (not executable). {MAC_BLOCKED}");
            }
            bail!("{} is not executable (chmod +x it).", server.display());
        }
        let child = spawn_server(server, args, &log).map_err(|e| {
            if mac && e.kind() == std::io::ErrorKind::PermissionDenied {
                anyhow::anyhow!("macOS blocked llama-server ({e}). {MAC_BLOCKED}")
            } else {
                anyhow::anyhow!("Could not start {}: {e}", server.display())
            }
        })?;
        let job = Job::assign(&child);
        let _ = fs::write(pid_file, format!("{} {}", child.id(), std::process::id()));
        *self.0.lock().unwrap() = Some((child, job));
        Ok(())
    }

    fn kill(&self, pid_file: &Path) {
        if let Some((mut child, job)) = self.0.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
            drop(job);
            let _ = fs::remove_file(pid_file);
        }
    }
}

/// Polls /health and /v1/models until the server serves `alias`: every 100 ms
/// at first, at most 300 ms apart (/health answers cheaply while it loads).
fn wait_ready(port: u16, alias: &str, log_path: &Path, running: &dyn Fn() -> bool) -> Result<()> {
    let client = local_client()?;
    let base = format!("http://127.0.0.1:{port}");
    // Large models on old CPUs can take minutes to map and load.
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut pause = Duration::from_millis(100);
    while Instant::now() < deadline {
        if !running() {
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
            if lists_model(&models, alias) {
                return Ok(());
            }
            bail!("Another server answered on port {port} and does not serve {alias}.");
        }
        std::thread::sleep(pause);
        pause = (pause * 5 / 4).min(Duration::from_millis(300));
    }
    bail!(
        "llama-server did not become ready within 5 minutes.\n{}",
        log_tail(log_path)
    )
}

/// Live numbers for the status bar.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metrics {
    /// Resident memory of llama-server, sampled at most every 5 s.
    pub ram_mb: u64,
    pub ctx_used: usize,
    pub ctx_total: usize,
    /// Average generation speed reported by the server.
    pub tokens_per_sec: Option<f32>,
}

/// Context use of the first slot from GET /slots: (used, total).
fn parse_slots(v: &Value) -> (usize, usize) {
    let Some(slot) = v.as_array().and_then(|a| a.first()) else {
        return (0, 0);
    };
    let n = |v: &Value| v.as_u64().map(|n| n as usize);
    let total = n(&slot["n_ctx"]).unwrap_or(0);
    let used = ["n_past", "n_tokens", "n_cache_tokens"]
        .iter()
        .find_map(|k| n(&slot[*k]))
        .or_else(|| n(&slot["next_token"]["n_past"]))
        .unwrap_or(0);
    (used, total)
}

/// A gauge from Prometheus text (GET /metrics).
fn prom_value(text: &str, name: &str) -> Option<f64> {
    text.lines().filter(|l| !l.starts_with('#')).find_map(|l| {
        let (key, value) = l.split_once(' ')?;
        (key == name).then(|| value.trim().parse().ok()).flatten()
    })
}

/// Slot cache file for a model, context size and fixed prompt prefix.
fn slot_name(alias: &str, ctx: usize, prefix: &str) -> String {
    format!(
        "{}-c{ctx}-{:016x}.bin",
        slot_alias(alias),
        fnv1a(prefix.as_bytes())
    )
}

fn slot_alias(alias: &str) -> String {
    alias
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Saved slot files, newest first.
fn slot_files() -> Vec<(String, SystemTime)> {
    let mut files: Vec<(String, SystemTime)> = fs::read_dir(slots_dir())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let time = e.metadata().ok()?.modified().ok()?;
            name.ends_with(".bin").then_some((name, time))
        })
        .collect();
    files.sort_by_key(|a| std::cmp::Reverse(a.1));
    files
}

/// Keeps `keep`, drops older files of the same model and caps the folder at 4 files.
fn prune_slots(keep: &str) {
    let model = keep.rsplitn(3, '-').nth(2).unwrap_or(keep).to_string();
    let mut kept = 0;
    for (name, _) in slot_files() {
        let same_model = name.rsplitn(3, '-').nth(2) == Some(model.as_str());
        if name == keep || (!same_model && kept < 3) {
            kept += usize::from(name != keep);
            continue;
        }
        let _ = fs::remove_file(slots_dir().join(name));
    }
}

fn slot_action(port: u16, action: &str, name: &str) -> Result<()> {
    let response = local_client()?
        .post(format!("http://127.0.0.1:{port}/slots/0?action={action}"))
        .timeout(Duration::from_secs(300))
        .json(&json!({ "filename": name }))
        .send()?;
    ensure!(
        response.status().is_success(),
        "slot {action} failed ({})",
        response.status()
    );
    Ok(())
}

#[derive(Clone, Default)]
pub struct Runtime {
    proc: Process,
    /// Started successfully and not stopped by us.
    live: Arc<AtomicBool>,
    crashed: Arc<AtomicBool>,
    backend: Arc<Mutex<Option<String>>>,
    /// What the running server was started with, and its port (restart_with).
    current: Arc<Mutex<Option<(String, u16)>>>,
    /// Last RAM sample (taken at most every 5 s), in MiB.
    ram: Arc<Mutex<Option<(Instant, u64)>>>,
    /// The server accepts slot save/restore.
    slots: Arc<AtomicBool>,
    /// The slot file saved during this run (saved once per run).
    saved_prefix: Arc<Mutex<Option<String>>>,
    /// Whether the running server accepts images (GET /props), read once per run.
    vision: Arc<Mutex<Option<bool>>>,
}

/// `modalities.vision` of GET /props; a server without the field has no vision.
fn parse_vision(props: &Value) -> bool {
    props["modalities"]["vision"].as_bool().unwrap_or(false)
}

/// Identifies a launch: the server binary and its arguments (sampling settings
/// are sent per request and do not need a restart).
fn launch_key(config: &Config) -> String {
    let launch = Launch {
        port: config.llama_port,
        intel_mac: INTEL_MAC,
        ..Default::default()
    };
    format!(
        "{:?} {}",
        find_server(config),
        server_args(config, &launch).join(" ")
    )
}

impl Runtime {
    pub fn running(&self) -> bool {
        match self.proc.alive(&pid_path()) {
            Some(true) => true,
            Some(false) => {
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

    /// Switches model or settings: keeps a running server whose launch would
    /// not change, otherwise restarts it. Returns the port.
    pub fn restart_with(&self, config: &Config) -> Result<u16> {
        let key = launch_key(&effective(config));
        let current = self.current.lock().unwrap().clone();
        if self.running()
            && let Some((running_key, port)) = current
            && running_key == key
        {
            return Ok(port);
        }
        self.start(config)
    }

    /// Starts llama-server with the effective settings (low-end profile and the
    /// model's preset applied), blocks until it serves the model, and returns
    /// the port it listens on (the next free one when llama_port is taken).
    pub fn start(&self, config: &Config) -> Result<u16> {
        self.stop();
        let config = &effective(config);
        ensure!(
            config.gguf_model.is_file(),
            "Choose a .gguf model file first"
        );
        rescan_server();
        let server = find_server(config).context(
            "llama-server was not found. Run setup, choose it in Settings, or install llama.cpp (Homebrew: brew install llama.cpp).",
        )?;
        kill_stale(&pid_path());
        let _ = fs::create_dir_all(slots_dir());
        let log_path = log_path();
        rotate_log(&log_path);
        let mut launch = Launch {
            port: 0,
            intel_mac: INTEL_MAC,
            build: server_version(&server).as_deref().and_then(build_number),
            cpu_only: false,
        };
        let already_cpu =
            config.llama_gpu_layers == Some(0) || (INTEL_MAC && config.llama_gpu_layers.is_none());
        let first = self.launch(config, &server, &mut launch, &log_path, false);
        if let Err(e) = first {
            let log = fs::read_to_string(&log_path).unwrap_or_default();
            if already_cpu || !gpu_failed(&log) {
                return Err(e);
            }
            self.stop();
            launch.cpu_only = true;
            // Appended, so the log keeps the GPU failure above the CPU run.
            self.launch(config, &server, &mut launch, &log_path, true)
                .with_context(|| format!("GPU offload failed ({e}); the CPU retry failed too"))?;
        }
        let log = fs::read_to_string(&log_path).unwrap_or_default();
        let mut backend = parse_backend(&log);
        if launch.cpu_only {
            backend.push_str(" (GPU failed, using CPU)");
        }
        if let Some(build) = launch.build {
            backend.push_str(&format!(", build b{build}"));
        }
        let slots = launch.build.is_none_or(|b| b >= MIN_SLOT_SAVE);
        if slots {
            self.restore_prefix(config, launch.port);
        }
        *self.backend.lock().unwrap() = Some(backend);
        *self.current.lock().unwrap() = Some((launch_key(config), launch.port));
        *self.saved_prefix.lock().unwrap() = None;
        *self.vision.lock().unwrap() = None;
        self.slots.store(slots, Ordering::SeqCst);
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
        append: bool,
    ) -> Result<()> {
        launch.port = free_port(config.llama_port)
            .with_context(|| format!("No free port found from {} upwards", config.llama_port))?;
        let log = OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(log_path)?;
        self.proc
            .spawn(server, &server_args(config, launch), log, &pid_path())?;
        wait_ready(launch.port, &model_alias(config), log_path, &|| {
            self.running()
        })
        .inspect_err(|_| self.proc.kill(&pid_path()))
    }

    /// Loads the newest saved prompt cache of this model and context size, so
    /// the first reply skips the system prompt. Errors are ignored.
    fn restore_prefix(&self, config: &Config, port: u16) {
        let start = format!(
            "{}-c{}-",
            slot_alias(&model_alias(config)),
            config.context_tokens
        );
        if let Some((name, _)) = slot_files()
            .into_iter()
            .find(|(n, _)| n.starts_with(&start))
        {
            let _ = slot_action(port, "restore", &name);
        }
    }

    /// True when `save_prefix` has nothing left to do this run.
    pub fn prefix_saved(&self) -> bool {
        !self.slots.load(Ordering::SeqCst) || self.saved_prefix.lock().unwrap().is_some()
    }

    /// Saves the prompt cache once per run, after the first reply, so the next
    /// launch restores it instead of processing the system prompt and tool
    /// definitions again. `prefix` is the fixed text every request starts with.
    /// Blocking (the file can be ~100 MB): call it on a worker thread.
    pub fn save_prefix(&self, config: &Config, port: u16, prefix: &str) {
        if !self.slots.load(Ordering::SeqCst) || !self.running() {
            return;
        }
        let config = effective(config);
        let name = slot_name(&model_alias(&config), config.context_tokens, prefix);
        {
            let mut saved = self.saved_prefix.lock().unwrap();
            if saved.is_some() {
                return;
            }
            *saved = Some(name.clone());
        }
        if slot_action(port, "save", &name).is_ok() {
            prune_slots(&name);
        }
    }

    /// RAM, context use and speed of the running server. Blocking (two local
    /// requests, and a memory query every 5 s): call it off the UI thread.
    pub fn metrics(&self, port: u16) -> Option<Metrics> {
        if !self.running() {
            return None;
        }
        let client = local_client().ok()?;
        let base = format!("http://127.0.0.1:{port}");
        let get = |path: &str| {
            client
                .get(format!("{base}{path}"))
                .send()
                .ok()
                .filter(|r| r.status().is_success())
        };
        let (mut ctx_used, ctx_total) = get("/slots")
            .and_then(|r| r.json::<Value>().ok())
            .map_or((0, 0), |v| parse_slots(&v));
        let text = get("/metrics")
            .and_then(|r| r.text().ok())
            .unwrap_or_default();
        if ctx_used == 0
            && let Some(tokens) = prom_value(&text, "llamacpp:kv_cache_tokens")
        {
            ctx_used = tokens as usize;
        }
        Some(Metrics {
            ram_mb: self.ram_mb(),
            ctx_used,
            ctx_total,
            tokens_per_sec: prom_value(&text, "llamacpp:predicted_tokens_seconds")
                .map(|v| v as f32)
                .filter(|v| *v > 0.0),
        })
    }

    fn ram_mb(&self) -> u64 {
        let mut cache = self.ram.lock().unwrap();
        // tasklist/ps start a process (tasklist walks every process); RSS barely moves, so read it rarely.
        if let Some((at, mb)) = *cache
            && at.elapsed() < Duration::from_secs(30)
        {
            return mb;
        }
        let mb = self.proc.ram_kb().map_or(0, |kb| kb / 1024);
        *cache = Some((Instant::now(), mb));
        mb
    }

    /// Whether the running model accepts images: GET /props → modalities.vision,
    /// asked once per start or restart and cached. None while no server runs or
    /// it did not answer. Blocking the first time after a start (one local
    /// request, 2 s at most): call it off the UI thread, or use `vision_cached`.
    pub fn vision(&self, port: u16) -> Option<bool> {
        if !self.running() {
            return None;
        }
        if let Some(known) = *self.vision.lock().unwrap() {
            return Some(known);
        }
        let props: Value = local_client()
            .ok()?
            .get(format!("http://127.0.0.1:{port}/props"))
            .send()
            .ok()
            .filter(|r| r.status().is_success())?
            .json()
            .ok()?;
        let vision = parse_vision(&props);
        *self.vision.lock().unwrap() = Some(vision);
        Some(vision)
    }

    /// The last `vision` answer of this run, without a request (for the UI thread).
    #[allow(dead_code)]
    pub fn vision_cached(&self) -> Option<bool> {
        if !self.running() {
            return None;
        }
        *self.vision.lock().unwrap()
    }

    pub fn stop(&self) {
        self.live.store(false, Ordering::SeqCst);
        self.crashed.store(false, Ordering::SeqCst);
        *self.vision.lock().unwrap() = None;
        *self.backend.lock().unwrap() = None;
        *self.current.lock().unwrap() = None;
        *self.ram.lock().unwrap() = None;
        self.proc.kill(&pid_path());
    }
}

/// The embedding GGUF to serve: the configured one, else the first known
/// embedding model (setup::EMBED_MODELS, default first) that was downloaded.
pub fn embed_model_path(config: &Config) -> Option<PathBuf> {
    let configured = &config.embedding_gguf;
    if !configured.as_os_str().is_empty() {
        return configured.is_file().then(|| configured.clone());
    }
    setup::EMBED_MODELS
        .iter()
        .map(setup::embed_path)
        .find(|p| p.is_file())
}

/// How the embedding server runs a model: its pooling (from setup::embed_profile)
/// and its context and batch sizes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EmbedRun {
    /// `--pooling` value (mean, cls, last or rank).
    pooling: String,
    ctx: usize,
    /// -b and -ub: one input must fit in a single micro-batch.
    batch: usize,
}

/// The pooling comes from setup::embed_profile for the file. Qwen3-Embedding
/// (32k context) gets a 4096-token batch on 16 GB or more, else 2048 (no input
/// is longer than about 2k tokens). Everything else keeps 8192 context and 2048
/// batch. The context is four slots (`--parallel 4`) of one full batch each, so
/// every slot holds the longest input.
fn embed_run(model: &Path, ram_gb: Option<u64>) -> EmbedRun {
    let name = model
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let pooling = setup::embed_profile(model).pooling;
    if name.to_lowercase().contains("qwen3-embedding") {
        let batch = match ram_gb.unwrap_or(0) {
            r if r >= 16 => 4096,
            _ => 2048,
        };
        return EmbedRun {
            pooling,
            ctx: batch * 4,
            batch,
        };
    }
    EmbedRun {
        pooling,
        ctx: 8192,
        batch: 2048,
    }
}

/// Arguments for the embedding server: CPU only (no VRAM taken from the chat
/// model), four slots, and a batch large enough for one passage.
fn embed_args(
    model: &Path,
    port: u16,
    threads: usize,
    build: Option<u32>,
    run: &EmbedRun,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-m".into(),
        model.display().to_string(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
    ];
    args.extend(["--alias", EMBED_ALIAS, "--embedding"].map(String::from));
    args.extend(["--pooling".into(), run.pooling.clone()]);
    let (ctx, batch) = (run.ctx.to_string(), run.batch.to_string());
    args.extend(
        [
            "-c",
            ctx.as_str(),
            "--parallel",
            "4",
            "-b",
            batch.as_str(),
            "-ub",
            batch.as_str(),
            "-ngl",
            "0",
        ]
        .map(String::from),
    );
    if build.is_none_or(|b| b >= MIN_DEVICE) {
        args.extend(["--device".into(), "none".into()]);
    }
    if threads > 0 {
        args.extend(["-t".into(), threads.to_string()]);
    }
    args
}

/// A second llama-server with `--embedding` for config.embedding_gguf, on its
/// own port. Every handle (Default or clone) shares one server, so the UI and
/// the embedding rerank see the same process.
#[derive(Clone)]
pub struct EmbedRuntime {
    proc: Process,
    /// Served model and port.
    state: Arc<Mutex<Option<(PathBuf, u16)>>>,
    /// Held while starting, so concurrent callers wait for one server.
    starting: Arc<Mutex<()>>,
}

static EMBED: LazyLock<EmbedRuntime> = LazyLock::new(|| EmbedRuntime {
    proc: Process::default(),
    state: Default::default(),
    starting: Default::default(),
});

impl Default for EmbedRuntime {
    fn default() -> Self {
        EMBED.clone()
    }
}

impl EmbedRuntime {
    pub fn running(&self) -> bool {
        self.proc.alive(&embed_pid_path()) == Some(true)
    }

    pub fn port(&self) -> Option<u16> {
        if !self.running() {
            return None;
        }
        self.state.lock().unwrap().as_ref().map(|(_, port)| *port)
    }

    /// Starts the server (or keeps one already serving this model), blocks
    /// until it is ready and returns its port.
    pub fn start(&self, config: &Config) -> Result<u16> {
        let _starting = self.starting.lock().unwrap();
        let model = embed_model_path(config)
            .context("Download an embedding model on the Models page or choose a GGUF first")?;
        let current = self.state.lock().unwrap().clone();
        if self.running()
            && let Some((served, port)) = current
            && served == model
        {
            return Ok(port);
        }
        self.stop();
        let server = find_server(config).context("llama-server was not found. Run setup first.")?;
        kill_stale(&embed_pid_path());
        let _ = fs::create_dir_all(data_dir());
        let log_path = data_dir().join("llama-embed.log");
        let port = free_port(config.llama_port.saturating_add(100))
            .context("No free port for the embedding server")?;
        let build = server_version(&server).as_deref().and_then(build_number);
        let run = embed_run(&model, setup::total_ram_gb());
        let args = embed_args(&model, port, config.llama_threads, build, &run);
        self.proc
            .spawn(&server, &args, File::create(&log_path)?, &embed_pid_path())?;
        if let Err(e) = wait_ready(port, EMBED_ALIAS, &log_path, &|| self.running()) {
            self.stop();
            return Err(e.context("The embedding server did not start"));
        }
        *self.state.lock().unwrap() = Some((model, port));
        Ok(port)
    }

    pub fn stop(&self) {
        *self.state.lock().unwrap() = None;
        self.proc.kill(&embed_pid_path());
    }
}

/// The shared embedding server.
pub fn embed_runtime() -> EmbedRuntime {
    EmbedRuntime::default()
}

fn log_tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
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
        assert!(joined.contains("-b 512 -ub 512"));
    }
    #[test]
    fn other_platforms_keep_server_gpu_default() {
        let args = server_args(&config(), &launch(false)).join(" ");
        assert!(!args.contains("-ngl"));
        assert!(!args.contains("--device"));
        assert!(!args.contains("--cache-type"));
        assert!(!args.contains("-ub"));
        assert!(args.contains("--jinja"));
        assert!(args.contains("--flash-attn auto"));
        assert!(args.contains("--metrics"));
        assert!(args.contains("--slot-save-path"));
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
        assert!(!args.contains("--flash-attn"));
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
    fn kv_quantization_only_for_long_contexts() {
        let mut c = config();
        c.context_tokens = 4096;
        assert!(
            !server_args(&c, &launch(true))
                .join(" ")
                .contains("--cache-type")
        );
    }
    #[test]
    fn low_end_profile() {
        let mut c = config();
        c.low_end = true;
        c.context_tokens = 16384;
        let e = effective(&c);
        assert_eq!(e.context_tokens, 2048);
        assert!(e.llama_threads >= 1);
        let args = server_args(&e, &launch(false)).join(" ");
        assert!(args.contains("-c 2048"));
        assert!(args.contains("--cache-type-k q8_0 --cache-type-v q8_0"));
        assert!(args.contains("-b 256 -ub 256"));
        assert_eq!(e.response_tokens, c.response_tokens.min(1024));
        c.low_end = false;
        assert_eq!(effective(&c).response_tokens, c.response_tokens);
    }
    #[test]
    fn small_models_keep_an_f16_kv_cache() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("llama-3.2-1b-q4_k_m.gguf");
        fs::write(&model, b"x").unwrap();
        let mut c = config();
        c.gguf_model = model;
        c.low_end = true;
        assert!(
            !server_args(&c, &launch(false))
                .join(" ")
                .contains("--cache-type")
        );
        c.low_end = false;
        c.context_tokens = 8192;
        assert!(
            !server_args(&c, &launch(true))
                .join(" ")
                .contains("--cache-type")
        );
    }
    #[test]
    fn known_builds_bound_the_prompt_cache_and_skip_fit_on_cpu() {
        let bundled = Launch {
            build: Some(12110),
            ..launch(true)
        };
        let args = server_args(&config(), &bundled).join(" ");
        assert!(args.contains("--cache-ram "));
        assert!(args.contains("--fit off"));
        let mut low = config();
        low.low_end = true;
        assert!(
            server_args(&low, &bundled)
                .join(" ")
                .contains("--cache-ram 256")
        );
        let gpu = Launch {
            build: Some(12110),
            ..launch(false)
        };
        let args = server_args(&config(), &gpu).join(" ");
        assert!(args.contains("--cache-ram") && !args.contains("--fit"));
        // Unknown or older builds get neither flag.
        let old = Launch {
            build: Some(7000),
            ..launch(true)
        };
        for l in [launch(true), old] {
            let args = server_args(&config(), &l).join(" ");
            assert!(!args.contains("--cache-ram") && !args.contains("--fit"));
        }
        let mib = cache_ram_mib(false);
        assert!((256..=2048).contains(&mib));
    }
    #[test]
    fn reads_proc_status_rss() {
        let status = "Name:\tllama-server\nVmPeak:\t  900000 kB\nVmRSS:\t  523456 kB\n";
        assert_eq!(parse_vmrss_kb(status), Some(523456));
        assert_eq!(parse_vmrss_kb("Name:\tx\n"), None);
    }
    #[test]
    fn perf_server_args_and_response_parsing() {
        let mut c = config();
        c.low_end = true;
        let l = Launch {
            build: Some(12110),
            ..launch(true)
        };
        let slots = serde_json::json!([{"id":0,"n_ctx":2048,"n_past":1500}]);
        let mut prom = String::new();
        for i in 0..40 {
            prom.push_str(&format!(
                "# HELP llamacpp:metric_{i} x\nllamacpp:metric_{i} {i}\n"
            ));
        }
        prom.push_str("llamacpp:predicted_tokens_seconds 12.5\nllamacpp:kv_cache_tokens 300\n");
        // server_args does a metadata call and a known-folder lookup: keep n small.
        let n = 500;
        let start = Instant::now();
        let mut total = 0usize;
        for _ in 0..n {
            let e = effective(&c);
            total += server_args(&e, &l).len();
            total += parse_slots(&slots).0;
            let speed = prom_value(&prom, "llamacpp:predicted_tokens_seconds");
            let kv = prom_value(&prom, "llamacpp:kv_cache_tokens");
            total += speed.zip(kv).map_or(0, |(a, b)| (a + b) as usize);
        }
        let took = start.elapsed();
        eprintln!(
            "perf: effective + server_args + slots/metrics parse x{n}: {took:?} ({:?} each)",
            took / n
        );
        assert!(total > 0);
        assert!(took < Duration::from_secs(5), "took {took:?}");
    }
    #[test]
    fn presets_override_globals() {
        let mut c = config();
        c.low_end = true;
        let preset = ModelPreset {
            context_tokens: Some(4096),
            gpu_layers: Some(12),
            temperature: Some(0.7),
            ..Default::default()
        };
        c.model_presets
            .insert("qwen2.5-coder-3b-q4_k_m.gguf".into(), preset);
        let e = effective(&c);
        assert_eq!(e.context_tokens, 4096);
        assert_eq!(e.llama_gpu_layers, Some(12));
        assert_eq!(e.temperature, 0.7);
        assert_eq!(e.top_p, c.top_p);
        c.gguf_model = PathBuf::from("/models/other.gguf");
        assert_eq!(effective(&c).llama_gpu_layers, c.llama_gpu_layers);
    }
    #[test]
    fn draft_model_enables_speculative_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let draft = dir.path().join("draft.gguf");
        fs::write(&draft, b"x").unwrap();
        let mut c = config();
        c.model_presets.insert(
            "qwen2.5-coder-3b-q4_k_m.gguf".into(),
            ModelPreset {
                draft_model: Some(draft.clone()),
                ..Default::default()
            },
        );
        let args = server_args(&c, &launch(true)).join(" ");
        assert!(args.contains(&format!("-md {}", draft.display())));
        assert!(args.contains("--draft-max 16") && args.contains("-ngld 0"));
        c.model_presets.clear();
        assert!(!server_args(&c, &launch(true)).join(" ").contains("-md"));
    }
    #[test]
    fn embedding_server_args() {
        // A file name that no known model uses, with no GGUF metadata: mean pooling.
        let model = Path::new("/m/nomic.gguf");
        let run = embed_run(model, Some(16));
        let args = embed_args(model, 8180, 4, None, &run).join(" ");
        assert!(args.contains("--embedding") && args.contains("--port 8180"));
        assert!(args.contains(&format!("--alias {EMBED_ALIAS}")));
        assert!(args.contains("-c 8192 --parallel 4 -b 2048 -ub 2048"));
        assert!(args.contains("-ngl 0 --device none -t 4"));
        assert!(args.contains("--pooling mean"));
        assert!(
            !embed_args(Path::new("a"), 1, 0, Some(3000), &run)
                .join(" ")
                .contains("--device")
        );
    }
    #[test]
    fn known_embedding_models_get_pooling_and_batches() {
        let nomic = Path::new("/m").join(setup::EMBED_MODEL.file);
        let p = embed_run(&nomic, Some(4));
        assert_eq!((p.pooling.as_str(), p.ctx, p.batch), ("mean", 8192, 2048));
        let qwen = Path::new("/m/Qwen3-Embedding-4B-Q4_K_M.gguf");
        let big = embed_run(qwen, Some(32));
        assert_eq!(
            (big.pooling.as_str(), big.ctx, big.batch),
            ("last", 16384, 4096)
        );
        let mid = embed_run(qwen, Some(8));
        assert_eq!((mid.ctx, mid.batch), (8192, 2048));
        let small = embed_run(qwen, None);
        assert_eq!((small.ctx, small.batch), (8192, 2048));
        // Each of the four slots holds one full batch.
        for p in [&big, &mid, &small] {
            assert_eq!(p.ctx, p.batch * 4);
        }
        // Another Qwen3-Embedding quant still pools on the last token.
        let other = embed_run(Path::new("Qwen3-Embedding-0.6B-f16.gguf"), Some(16));
        assert_eq!(other.pooling, "last");
        let args = embed_args(qwen, 1, 0, None, &big).join(" ");
        assert!(args.contains("--pooling last -c 16384 --parallel 4 -b 4096 -ub 4096"));
    }
    #[test]
    fn embedding_server_pools_by_the_file_it_serves() {
        let dir = tempfile::tempdir().unwrap();
        // A custom GGUF whose metadata says CLS pooling keeps it.
        let mut bge = b"GGUF".to_vec();
        bge.extend(3u32.to_le_bytes());
        bge.extend(0u64.to_le_bytes());
        bge.extend(2u64.to_le_bytes());
        for (key, kind, value) in [
            ("general.architecture", 8u32, b"bert".to_vec()),
            ("bert.pooling_type", 4u32, 2u32.to_le_bytes().to_vec()),
        ] {
            bge.extend((key.len() as u64).to_le_bytes());
            bge.extend(key.as_bytes());
            bge.extend(kind.to_le_bytes());
            if kind == 8 {
                bge.extend((value.len() as u64).to_le_bytes());
            }
            bge.extend(value);
        }
        let path = dir.path().join("bge-custom.gguf");
        fs::write(&path, bge).unwrap();
        let args = embed_args(&path, 1, 0, None, &embed_run(&path, Some(8))).join(" ");
        assert!(args.contains("--pooling cls"));
    }
    #[test]
    fn finds_the_matching_vision_projector() {
        let p = |s: &str| PathBuf::from(format!("/m/{s}"));
        let model = p("Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf");
        assert_eq!(base_name(&model), "qwen2.5-vl-3b-instruct");
        assert_eq!(
            base_name(&p("mmproj-Qwen2.5-VL-3B-Instruct-f16.gguf")),
            "qwen2.5-vl-3b-instruct"
        );
        assert_eq!(
            base_name(&p("nomic-embed-text-v1.5.Q8_0.gguf")),
            "nomic-embed-text-v1.5"
        );
        let own = p("mmproj-Qwen2.5-VL-3B-Instruct-f16.gguf");
        let own_q8 = p("mmproj-Qwen2.5-VL-3B-Instruct-q8_0.gguf");
        let other = p("mmproj-gemma-3-4b-it-f16.gguf");
        assert_eq!(
            pick_mmproj(
                &model,
                vec![other.clone(), own_q8.clone(), own.clone()],
                true
            ),
            Some(own.clone())
        );
        assert_eq!(
            pick_mmproj(&model, vec![other.clone(), own_q8.clone()], true),
            Some(own_q8)
        );
        // A projector of another model is never used, even when it is the only one.
        assert_eq!(pick_mmproj(&model, vec![other.clone()], true), None);
        assert_eq!(
            pick_mmproj(&p("Llama-3.2-1B-Q4_K_M.gguf"), vec![own], true),
            None
        );
        // Base names match on a word boundary: 7B is not 72B.
        let seven = p("Qwen2.5-VL-7B-Instruct-Q4_K_M.gguf");
        let big = p("mmproj-Qwen2.5-VL-72B-Instruct-f16.gguf");
        assert_eq!(pick_mmproj(&seven, vec![big], true), None);
        assert_eq!(
            pick_mmproj(
                &p("Qwen2.5-VL-7B-Q4_K_M.gguf"),
                vec![p("mmproj-Qwen2.5-VL-7B-Instruct-f16.gguf")],
                true
            ),
            Some(p("mmproj-Qwen2.5-VL-7B-Instruct-f16.gguf"))
        );
        assert!(same_base("qwen2.5-vl-7b", "qwen2.5-vl-7b"));
        assert!(same_base("qwen2.5-vl-7b-instruct", "qwen2.5-vl-7b"));
        assert!(!same_base("qwen2.5-vl-7b", "qwen2.5-vl-72b-instruct"));
        // A generic projector is used when it is the only one in the folder.
        let generic = p("mmproj-model-f16.gguf");
        assert_eq!(
            pick_mmproj(&model, vec![generic.clone()], true),
            Some(generic.clone())
        );
        // ...and only when the model is the only one there too.
        assert_eq!(pick_mmproj(&model, vec![generic.clone()], false), None);
        assert_eq!(
            pick_mmproj(&model, vec![generic, p("mmproj-F16.gguf")], true),
            None
        );
        assert_eq!(base_name(&p("mmproj-F16.gguf")), "");
        assert!(is_mmproj(&other) && !is_mmproj(&model));
        assert_eq!(find_mmproj(&other), None);
        assert!(is_quant("Q4_K_M") && is_quant("iq4_xs") && is_quant("BF16"));
        assert!(!is_quant("qwen3") && !is_quant("instruct") && !is_quant("3b"));
    }
    #[test]
    fn projector_next_to_the_model_is_passed_to_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("gemma-3-4b-it-Q4_K_M.gguf");
        let proj = dir.path().join("mmproj-gemma-3-4b-it-f16.gguf");
        fs::write(&model, b"x").unwrap();
        let mut c = config();
        c.gguf_model = model;
        assert!(
            !server_args(&c, &launch(false))
                .join(" ")
                .contains("--mmproj")
        );
        fs::write(&proj, b"x").unwrap();
        let gpu = server_args(&c, &launch(false)).join(" ");
        assert!(gpu.contains(&format!("--mmproj {}", proj.display())));
        assert!(!gpu.contains("--no-mmproj-offload"));
        let cpu = server_args(&c, &launch(true)).join(" ");
        assert!(cpu.contains("--no-mmproj-offload"));
        // A preset projector wins over the one found in the folder.
        let chosen = dir.path().join("custom-projector.gguf");
        fs::write(&chosen, b"x").unwrap();
        c.model_presets.insert(
            "gemma-3-4b-it-Q4_K_M.gguf".into(),
            ModelPreset {
                mmproj: Some(chosen.clone()),
                ..Default::default()
            },
        );
        assert_eq!(mmproj_for(&c), Some(chosen));
    }
    #[test]
    fn reads_vision_from_props() {
        let v = serde_json::json!({"modalities":{"vision":true,"audio":false}});
        assert!(parse_vision(&v));
        assert!(!parse_vision(
            &serde_json::json!({"modalities":{"vision":false}})
        ));
        assert!(!parse_vision(
            &serde_json::json!({"default_generation_settings":{}})
        ));
        assert_eq!(Runtime::default().vision_cached(), None);
    }
    #[test]
    fn non_avx2_windows_prefers_cpu_build() {
        assert_eq!(bundled_dirs(true)[0], "");
        assert_eq!(bundled_dirs(false)[0] == "llama/cpu", cfg!(windows));
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
    fn tag_file_skips_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join(binary_name());
        fs::write(&server, b"not a binary").unwrap();
        fs::write(dir.path().join(TAG_FILE), "b12110\n").unwrap();
        assert_eq!(server_version(&server).as_deref(), Some("b12110"));
        fs::write(dir.path().join(TAG_FILE), "junk").unwrap();
        assert_eq!(tag_of(&server), None);
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
    fn reads_process_ram() {
        let csv = "\"llama-server.exe\",\"1234\",\"Console\",\"1\",\"2,000,000 K\"\r\n";
        assert_eq!(parse_ram_kb(csv), Some(2_000_000));
        let dotted = "\"llama-server.exe\",\"1\",\"Console\",\"1\",\"1.234.567 K\"";
        assert_eq!(parse_ram_kb(dotted), Some(1_234_567));
        assert_eq!(parse_ram_kb("  523456\n"), Some(523456));
        assert_eq!(
            parse_ram_kb("INFO: No tasks are running which match the specified criteria."),
            None
        );
        assert_eq!(parse_ram_kb(""), None);
    }
    #[test]
    fn reads_slots_and_metrics() {
        let slots = serde_json::json!([{"id":0,"n_ctx":8192,"n_past":1500}]);
        assert_eq!(parse_slots(&slots), (1500, 8192));
        let nested = serde_json::json!([{"n_ctx":4096,"next_token":{"n_past":12}}]);
        assert_eq!(parse_slots(&nested), (12, 4096));
        assert_eq!(parse_slots(&serde_json::json!({"error":"x"})), (0, 0));
        let text = "# HELP llamacpp:predicted_tokens_seconds Average\n# TYPE llamacpp:predicted_tokens_seconds gauge\nllamacpp:predicted_tokens_seconds 12.5\nllamacpp:kv_cache_tokens 300\n";
        assert_eq!(
            prom_value(text, "llamacpp:predicted_tokens_seconds"),
            Some(12.5)
        );
        assert_eq!(prom_value(text, "llamacpp:kv_cache_tokens"), Some(300.0));
        assert_eq!(prom_value(text, "llamacpp:missing"), None);
    }
    #[test]
    fn slot_files_are_named_per_model_context_and_prefix() {
        let a = slot_name("qwen 3b/q4", 8192, "system prompt");
        assert!(a.starts_with("qwen_3b_q4-c8192-") && a.ends_with(".bin"));
        assert_ne!(a, slot_name("qwen 3b/q4", 8192, "other prompt"));
        assert_ne!(a, slot_name("qwen 3b/q4", 4096, "system prompt"));
        assert_eq!(a.rsplitn(3, '-').nth(2), Some("qwen_3b_q4"));
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
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
