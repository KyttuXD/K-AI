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

pub const MODELS: &[ModelChoice] = &[
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

/// Installed RAM in whole GiB (rounded, so a "16 GB" machine reporting 15.8 counts as 16).
pub fn total_ram_gb() -> Option<u64> {
    static RAM: OnceLock<Option<u64>> = OnceLock::new();
    *RAM.get_or_init(|| total_ram_bytes().map(|b| (b + (1 << 29)) >> 30))
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

/// The largest model that fits this machine's RAM.
pub fn recommended() -> &'static ModelChoice {
    pick_model(total_ram_gb(), llama::INTEL_MAC)
}

/// Unknown RAM picks the smallest model; Intel Macs never go above 3B (CPU only).
fn pick_model(ram_gb: Option<u64>, intel_mac: bool) -> &'static ModelChoice {
    let ram = ram_gb.unwrap_or(0);
    let cap = if intel_mac { 8 } else { u64::MAX };
    MODELS
        .iter()
        .filter(|m| m.min_ram_gb <= ram.min(cap))
        .max_by_key(|m| m.min_ram_gb)
        .unwrap_or(&MODELS[0])
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

fn install_dir() -> PathBuf {
    data_dir().join("llama").join(LLAMA_TAG)
}

/// llama-server from a previous setup download, if any.
pub fn installed_server() -> Option<PathBuf> {
    let dir = install_dir();
    dir.is_dir().then(|| find_binary(&dir)).flatten()
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
fn asset_for(os: &str, arch: &str) -> Option<String> {
    let suffix = match (os, arch) {
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
/// SHA-256 of an official asset at LLAMA_TAG (the same pins as scripts/fetch-llama.sh), or
/// None for assets not pinned yet.
fn pinned_sha256(asset: &str) -> Option<&'static str> {
    let suffix = asset.strip_prefix(&format!("llama-{LLAMA_TAG}-"))?;
    Some(match suffix {
        "bin-win-vulkan-x64.zip" => {
            "9732a5e48887ca54d0bfe4af4324dc85fd56cd8cda19b52df3542fa6ebe6874f"
        }
        "bin-ubuntu-x64.tar.gz" => {
            "368227edcc229be5f9bed16a2223d79b5e41d0ac2e19bcf5825e82291c060e55"
        }
        "bin-ubuntu-arm64.tar.gz" => {
            "4758b51335e37b4346251f2687d64841ff1c07ecb27fc46323945bb2ec9c44d2"
        }
        "bin-macos-arm64.tar.gz" => {
            "4f5b8de12e0b9516658ca6a5764c6ffb3d7c659c3dd538556b1413c7b4050662"
        }
        _ => return None,
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
    let client = Client::builder()
        .user_agent("kyttu-ai")
        .connect_timeout(Duration::from_secs(30))
        .timeout(None::<Duration>)
        .build()?;
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
    let asset = asset_for(OS, ARCH).with_context(|| {
        format!("No prebuilt llama.cpp for {OS} {ARCH}. Install llama.cpp and choose llama-server in Settings.")
    })?;
    let url =
        format!("https://github.com/ggml-org/llama.cpp/releases/download/{LLAMA_TAG}/{asset}");
    let root = data_dir().join("llama");
    fs::create_dir_all(&root)?;
    let archive = root.join(&asset);
    if !archive.is_file() {
        let part = download(
            client,
            &url,
            &archive,
            None,
            "Downloading llama.cpp",
            progress,
            cancel,
        )?;
        fs::rename(&part, &archive)?;
    }
    check_cancel(cancel)?;
    // Verify both a fresh download and an archive left from an earlier run.
    let extracting = match pinned_sha256(&asset) {
        Some(want) => {
            set_stage(progress, "Verifying llama.cpp");
            let got = sha256_file(&archive);
            if !got.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(want)) {
                let _ = fs::remove_file(&archive);
                match got {
                    Some(got) => bail!(
                        "{asset} has SHA-256 {got}, expected {want}; it was deleted. Run setup again."
                    ),
                    None => bail!(
                        "Could not compute the SHA-256 of {asset} (no certutil, sha256sum or shasum found)."
                    ),
                }
            }
            "Extracting llama.cpp"
        }
        None => "Extracting llama.cpp (unverified: no checksum is pinned for this platform)",
    };
    set_stage(progress, extracting);
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
    if llama::server_version(&server).is_none() {
        bail!(not_runnable(&server));
    }
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
    // Only verified downloads are renamed to the final name.
    if fs::metadata(&dest).is_ok_and(|m| m.len() == choice.size) {
        return Ok((dest, None));
    }
    let stage = format!("Downloading {}", choice.name);
    let part = download(
        client,
        choice.url,
        &dest,
        Some(choice.size),
        &stage,
        progress,
        cancel,
    )?;
    set_stage(progress, "Verifying checksum");
    let note = match sha256_file(&part) {
        Some(hash) if hash.eq_ignore_ascii_case(choice.sha256) => None,
        Some(hash) => {
            let _ = fs::remove_file(&part);
            bail!(
                "Checksum mismatch for {} (got {hash}). The partial file was removed; run setup again.",
                choice.file
            );
        }
        None => Some("no checksum tool found; verified by file size"),
    };
    fs::rename(&part, &dest)?;
    Ok((dest, note))
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
        assert_eq!(
            asset_for("windows", "x86_64").as_deref(),
            Some("llama-b12110-bin-win-vulkan-x64.zip")
        );
        assert_eq!(
            asset_for("windows", "aarch64").as_deref(),
            Some("llama-b12110-bin-win-cpu-arm64.zip")
        );
        assert_eq!(
            asset_for("linux", "x86_64").as_deref(),
            Some("llama-b12110-bin-ubuntu-x64.tar.gz")
        );
        assert_eq!(
            asset_for("linux", "aarch64").as_deref(),
            Some("llama-b12110-bin-ubuntu-arm64.tar.gz")
        );
        assert_eq!(
            asset_for("macos", "aarch64").as_deref(),
            Some("llama-b12110-bin-macos-arm64.tar.gz")
        );
        assert_eq!(
            asset_for("macos", "x86_64").as_deref(),
            Some("llama-b12110-bin-macos-x64.tar.gz")
        );
        assert_eq!(asset_for("freebsd", "x86_64"), None);
    }
    #[test]
    fn pins_official_asset_hashes() {
        let pinned = |os, arch| asset_for(os, arch).and_then(|a| pinned_sha256(&a));
        for (os, arch) in [
            ("windows", "x86_64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "aarch64"),
        ] {
            let hash = pinned(os, arch).unwrap_or_default();
            assert!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "{os} {arch}"
            );
        }
        assert_eq!(pinned("windows", "aarch64"), None);
        assert_eq!(pinned("macos", "x86_64"), None);
    }
    #[test]
    fn recommends_by_ram() {
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
        for m in MODELS {
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
