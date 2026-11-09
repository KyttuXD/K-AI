//! Self-update from the GitHub releases of KyttuXD/Kyttu-AI: look up the latest
//! stable release, download this platform's zip into `data_dir()/updates/<tag>/`
//! (resumable), check it against the release's SHA256SUMS.txt with the system
//! hash tool, unpack it with the system tar/unzip, then swap it in for the
//! running app and start the new one. Debug builds and copies running from a
//! cargo target folder never replace themselves.
use super::config::data_dir;
use anyhow::{Context, Result, bail, ensure};
use reqwest::{
    StatusCode,
    header::{ACCEPT, RANGE},
};
use serde_json::Value;
use std::{
    cmp::Ordering,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

pub const REPO: &str = "KyttuXD/Kyttu-AI";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SUMS: &str = "SHA256SUMS.txt";
const UNPACKED: &str = "unpacked";
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub version: String,
    pub notes: String,
    pub asset_url: String,
    pub asset_name: String,
    pub size: u64,
    pub sums_url: Option<String>,
}

#[derive(Clone, Default)]
pub struct UpdateProgress {
    pub stage: String,
    pub done: u64,
    pub total: u64,
    pub ready: bool,
    pub error: Option<String>,
}

/// Where releases are downloaded and unpacked, one folder per tag.
pub fn updates_dir() -> PathBuf {
    data_dir().join("updates")
}

// ---- versions and release selection ------------------------------------------------------------

/// `v1.2.3-rc.1+build` -> ([1, 2, 3], ["rc", "1"]). A missing minor or patch counts as 0.
fn parse_version(v: &str) -> Option<([u64; 3], Vec<String>)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let v = v.split('+').next()?;
    let (core, pre) = v.split_once('-').unwrap_or((v, ""));
    let mut nums = [0u64; 3];
    let mut parts = core.split('.');
    for (i, part) in parts.by_ref().take(3).enumerate() {
        nums[i] = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    let pre = if pre.is_empty() {
        vec![]
    } else {
        pre.split('.').map(String::from).collect()
    };
    Some((nums, pre))
}

/// Semver precedence: a release outranks its prereleases; numeric identifiers
/// compare as numbers and rank below alphanumeric ones.
fn compare_pre(a: &[String], b: &[String]) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => a
            .iter()
            .zip(b)
            .map(|(x, y)| match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                _ => x.cmp(y),
            })
            .find(|o| o.is_ne())
            .unwrap_or_else(|| a.len().cmp(&b.len())),
    }
}

/// None when either side is not a version.
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let (an, ap) = parse_version(a)?;
    let (bn, bp) = parse_version(b)?;
    Some(an.cmp(&bn).then_with(|| compare_pre(&ap, &bp)))
}

pub fn is_newer(remote: &str, current: &str) -> bool {
    compare_versions(remote, current) == Some(Ordering::Greater)
}

fn is_stable(v: &str) -> bool {
    parse_version(v).is_some_and(|(_, pre)| pre.is_empty())
}

/// The release.yml artifact name for an OS/arch pair (`std::env::consts` values).
pub fn artifact_for(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("windows", "x86_64") => "windows-x64",
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("macos", "aarch64") => "macos-arm64",
        _ => return None,
    })
}

fn this_artifact() -> Option<&'static str> {
    artifact_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// The zip for `artifact`: `kyttu-ai-<tag>-<artifact>.zip` as release.yml names it,
/// else any `kyttu-ai-*-<artifact>.zip`.
fn pick_asset<'a>(assets: &'a [Value], tag: &str, artifact: &str) -> Option<&'a Value> {
    let exact = format!("kyttu-ai-{tag}-{artifact}.zip");
    let suffix = format!("-{artifact}.zip");
    let name = |a: &Value| a["name"].as_str().unwrap_or_default().to_string();
    assets.iter().find(|a| name(a) == exact).or_else(|| {
        assets.iter().find(|a| {
            let n = name(a);
            n.starts_with("kyttu-ai-") && n.ends_with(&suffix)
        })
    })
}

/// A plain file name: no separators, no `..`, nothing hidden.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// The release in GitHub's JSON when it is stable, newer than `current` and has a
/// download for `artifact`.
fn parse_release(v: &Value, current: &str, artifact: &str) -> Result<Option<Release>> {
    if v["draft"].as_bool().unwrap_or(false) || v["prerelease"].as_bool().unwrap_or(false) {
        return Ok(None);
    }
    let tag = v["tag_name"].as_str().unwrap_or_default();
    ensure!(safe_name(tag), "Release tag {tag:?} is not a plain name");
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    if !is_stable(&version) || !is_newer(&version, current) {
        return Ok(None);
    }
    let assets = v["assets"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let asset = pick_asset(assets, tag, artifact)
        .with_context(|| format!("Release {tag} has no download for {artifact} yet"))?;
    let url = |a: &Value| a["browser_download_url"].as_str().map(String::from);
    let asset_name = asset["name"].as_str().unwrap_or_default().to_string();
    ensure!(
        safe_name(&asset_name),
        "Release asset {asset_name:?} is not a plain name"
    );
    Ok(Some(Release {
        tag: tag.into(),
        version,
        notes: v["body"].as_str().unwrap_or_default().replace("\r\n", "\n"),
        asset_url: url(asset).context("The release asset has no download URL")?,
        asset_name,
        size: asset["size"].as_u64().unwrap_or(0),
        sums_url: assets.iter().find(|a| a["name"] == SUMS).and_then(url),
    }))
}

/// Shared clients: one with a request timeout (API, checksums), one without
/// (the release download).
fn client(timeout: Option<Duration>) -> Result<reqwest::blocking::Client> {
    static TIMED: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    static UNTIMED: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    let cell = if timeout.is_some() { &TIMED } else { &UNTIMED };
    super::llama::shared(cell, || {
        reqwest::blocking::Client::builder()
            .user_agent("kyttu-ai")
            .connect_timeout(TIMEOUT)
            .timeout(timeout)
            .build()
    })
}

/// The latest stable release when it is newer than this build.
pub fn check() -> Result<Option<Release>> {
    let artifact = this_artifact().context("Updates are not published for this platform")?;
    let response = client(Some(TIMEOUT))?
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .header(ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .context("Could not reach GitHub")?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None); // nothing published yet
    }
    ensure!(
        response.status().is_success(),
        "GitHub answered {}",
        response.status()
    );
    parse_release(&response.json::<Value>()?, VERSION, artifact)
}

// ---- dev builds ----------------------------------------------------------------------------------

/// Whether `exe` runs from a cargo target folder (`target*/…/debug|release/`).
fn in_cargo_target(exe: &Path) -> bool {
    let names: Vec<String> = exe
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    names.iter().enumerate().any(|(i, n)| {
        (n == "target" || n.starts_with("target-") || n.starts_with("target_"))
            && names[i + 1..]
                .iter()
                .any(|n| n == "debug" || n == "release")
    })
}

/// Why this copy must not replace itself, if it must not.
pub fn self_update_blocked() -> Option<String> {
    if cfg!(debug_assertions) {
        return Some(
            "This is a debug build, so it does not update itself. Download releases from GitHub."
                .into(),
        );
    }
    let exe = std::env::current_exe().ok()?;
    in_cargo_target(&exe).then(|| {
        format!(
            "Running from a cargo build folder ({}), so it does not update itself. Use a packaged release.",
            exe.display()
        )
    })
}

// ---- download, verify, unpack --------------------------------------------------------------------

fn set(progress: &Mutex<UpdateProgress>, stage: &str, done: u64, total: u64) {
    let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
    p.stage = stage.into();
    p.done = done;
    p.total = total;
}

/// Downloads, verifies and unpacks `release`; returns the unpacked folder to pass
/// to [`apply_and_restart`]. Progress (and any error) is mirrored into `progress`.
pub fn download_and_stage(
    release: &Release,
    progress: Arc<Mutex<UpdateProgress>>,
) -> Result<PathBuf> {
    let result = stage(release, &progress);
    let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
    match &result {
        Ok(_) => {
            p.stage = format!("Version {} is ready: restart to update", release.version);
            p.ready = true;
            p.error = None;
        }
        Err(e) => p.error = Some(format!("{e:#}")),
    }
    result
}

fn stage(release: &Release, progress: &Mutex<UpdateProgress>) -> Result<PathBuf> {
    if let Some(why) = self_update_blocked() {
        bail!(why);
    }
    ensure!(
        safe_name(&release.tag) && safe_name(&release.asset_name),
        "The release names are not plain file names"
    );
    let sums_url = release.sums_url.as_deref().context(
        "This release has no SHA256SUMS.txt, so the download cannot be verified; not installing it",
    )?;
    let dir = updates_dir().join(&release.tag);
    fs::create_dir_all(&dir)?;
    set(progress, "Fetching checksums", 0, release.size);
    let sums = client(Some(TIMEOUT))?
        .get(sums_url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .context("Could not download SHA256SUMS.txt")?;
    let expected = parse_sums(&sums, &release.asset_name)
        .with_context(|| format!("SHA256SUMS.txt has no entry for {}", release.asset_name))?;
    let archive = dir.join(&release.asset_name);
    if !(archive.is_file() && (release.size == 0 || file_len(&archive) == release.size)) {
        download(&release.asset_url, &archive, release.size, progress)?;
    }
    let size = file_len(&archive);
    set(progress, "Verifying download", size, size);
    let actual = sha256_file(&archive)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        let _ = fs::remove_file(&archive);
        bail!(
            "The download does not match SHA256SUMS.txt (expected {expected}, got {actual}). It was deleted and nothing was installed."
        );
    }
    set(progress, "Unpacking", size, size);
    let unpacked = dir.join(UNPACKED);
    let _ = fs::remove_dir_all(&unpacked);
    fs::create_dir_all(&unpacked)?;
    extract(&archive, &unpacked)?;
    payload(&unpacked)?;
    Ok(unpacked)
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    s.into()
}

/// Downloads `url` to `<archive>.part`, resuming with a Range request, then renames it.
fn download(url: &str, archive: &Path, size: u64, progress: &Mutex<UpdateProgress>) -> Result<()> {
    let part = with_suffix(archive, ".part");
    let mut have = file_len(&part);
    if size > 0 && have > size {
        fs::remove_file(&part)?;
        have = 0;
    }
    if size == 0 || have < size {
        let mut request = client(None)?
            .get(url)
            .header(ACCEPT, "application/octet-stream");
        if have > 0 {
            request = request.header(RANGE, format!("bytes={have}-"));
        }
        let mut response = request.send().context("The download could not start")?;
        let status = response.status();
        ensure!(status.is_success(), "The download failed: {status}");
        let resumed = status == StatusCode::PARTIAL_CONTENT;
        if !resumed {
            have = 0;
        }
        let total = if size > 0 {
            size
        } else {
            response.content_length().unwrap_or(0) + have
        };
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resumed)
            .truncate(!resumed)
            .open(&part)?;
        let mut buf = vec![0u8; 1 << 16];
        set(progress, "Downloading", have, total);
        loop {
            let n = response
                .read(&mut buf)
                .context("The download was interrupted; try again to resume")?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            have += n as u64;
            set(progress, "Downloading", have, total);
        }
        file.flush()?;
    }
    if size > 0 {
        ensure!(
            have == size,
            "The download stopped at {have} of {size} bytes; try again to resume"
        );
    }
    rename_retry(&part, archive)
}

/// The SHA-256 for `name` in a `sha256sum`-style listing (`<hex>  <name>` or `<hex> *<name>`).
fn parse_sums(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        let hash = hash.trim_start_matches('\\');
        let file = file
            .trim_start()
            .trim_start_matches('*')
            .trim_start_matches("./");
        (file == name && is_hex64(hash)).then(|| hash.to_ascii_lowercase())
    })
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The hash in certutil, sha256sum or shasum output.
fn parse_hash_output(out: &str) -> Option<String> {
    out.lines().find_map(|line| {
        let line = line.trim().trim_start_matches('\\');
        let compact: String = line.chars().filter(|c| *c != ' ').collect();
        if is_hex64(&compact) {
            return Some(compact.to_ascii_lowercase());
        }
        let first = line.split_whitespace().next()?;
        is_hex64(first).then(|| first.to_ascii_lowercase())
    })
}

/// A system tool by name; on Windows the System32 copy (Git for Windows puts a GNU
/// tar first on PATH, which cannot read zips).
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

fn hide(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd
}

fn output(cmd: &mut Command) -> std::io::Result<Output> {
    hide(cmd).stdin(Stdio::null()).output()
}

fn sha256_file(path: &Path) -> Result<String> {
    let commands: Vec<Command> = if cfg!(windows) {
        let mut c = Command::new(system_tool("certutil"));
        c.arg("-hashfile").arg(path).arg("SHA256");
        vec![c]
    } else {
        let mut sha256sum = Command::new("sha256sum");
        sha256sum.arg(path);
        let mut shasum = Command::new("shasum");
        shasum.args(["-a", "256"]).arg(path);
        if cfg!(target_os = "macos") {
            vec![shasum, sha256sum]
        } else {
            vec![sha256sum, shasum]
        }
    };
    let mut tried = vec![];
    for mut cmd in commands {
        let name = cmd.get_program().to_string_lossy().into_owned();
        match output(&mut cmd) {
            Ok(out) if out.status.success() => {
                match parse_hash_output(&String::from_utf8_lossy(&out.stdout)) {
                    Some(hash) => return Ok(hash),
                    None => tried.push(format!("{name}: unreadable output")),
                }
            }
            Ok(out) => tried.push(format!(
                "{name}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => tried.push(format!("{name}: {e}")),
        }
    }
    bail!(
        "Could not compute the download's SHA-256 ({}); not installing it",
        tried.join("; ")
    )
}

/// Unpacks the zip with the system tools: tar (bsdtar) on Windows 10+ and macOS, unzip on Linux.
fn extract(archive: &Path, to: &Path) -> Result<()> {
    let linux = cfg!(target_os = "linux");
    let mut cmd = if linux {
        let mut c = Command::new("unzip");
        c.arg("-q").arg("-o").arg(archive).arg("-d").arg(to);
        c
    } else {
        let mut c = Command::new(system_tool("tar"));
        c.arg("-xf").arg(archive).arg("-C").arg(to);
        c
    };
    let out = output(&mut cmd).map_err(|e| {
        if linux && e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "Installing the update needs `unzip`, which is not installed (for example: sudo apt install unzip)"
            )
        } else {
            anyhow::anyhow!("Could not start the unpacker: {e}")
        }
    })?;
    ensure!(
        out.status.success(),
        "Unpacking the update failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// The first entry under `dir` (breadth first, `depth` levels down) that `want` accepts.
fn find(dir: &Path, depth: usize, want: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let entries: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries
        .iter()
        .find(|p| want(p.as_path()))
        .cloned()
        .or_else(|| {
            (depth > 0)
                .then(|| {
                    entries
                        .iter()
                        .filter(|p| p.is_dir())
                        .find_map(|p| find(p, depth - 1, want))
                })
                .flatten()
        })
}

/// The `.app` for `…/X.app/Contents/MacOS/<binary>`.
fn app_bundle(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app")
        .then_some(app)
}

/// What in the unpacked release takes this app's place: the .app bundle when
/// running from one on macOS, the executable otherwise.
fn payload(unpacked: &Path) -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    if cfg!(target_os = "macos") && app_bundle(&exe).is_some() {
        return find(unpacked, 3, &|p: &Path| {
            p.is_dir() && p.extension().is_some_and(|e| e == "app")
        })
        .context("The update has no .app bundle");
    }
    let names: &[&str] = if cfg!(windows) {
        &["KYTTU - AI.exe", "kyttu-ai.exe"]
    } else {
        &["kyttu-ai"]
    };
    find(unpacked, 3, &|p: &Path| {
        p.is_file()
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| names.contains(&n))
    })
    .with_context(|| format!("The update has no {}", names[0]))
}

// ---- swap and restart ----------------------------------------------------------------------------

/// Windows refuses a rename while a virus scanner still holds a fresh file, so try a few times.
fn rename_retry(from: &Path, to: &Path) -> Result<()> {
    let mut attempt = 0u64;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if attempt >= 7 => {
                return Err(anyhow::Error::from(e).context(format!(
                    "Could not move {} to {}",
                    from.display(),
                    to.display()
                )));
            }
            Err(_) => attempt += 1,
        }
        std::thread::sleep(Duration::from_millis(150 * attempt));
    }
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        #[cfg(unix)]
        {
            if kind.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
                continue;
            }
        }
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    if path.is_dir() && !path.is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Puts the staged update (from [`download_and_stage`]) in place of the running app
/// and starts the new one. The caller closes the app right after Ok. Stop
/// llama-server first: its folder is replaced too.
pub fn apply_and_restart(staged: &Path) -> Result<()> {
    if let Some(why) = self_update_blocked() {
        bail!(why);
    }
    let root = updates_dir()
        .canonicalize()
        .context("No update has been downloaded")?;
    let staged = staged
        .canonicalize()
        .context("The downloaded update is gone")?;
    ensure!(
        staged.starts_with(&root) && staged != root,
        "{} is not an update this app downloaded",
        staged.display()
    );
    let new = payload(&staged)?;
    let exe = std::env::current_exe()?;
    // Unix: resolve symlinks (/Applications aliases, ~/.local/bin links) to the real file.
    let exe = if cfg!(unix) {
        exe.canonicalize().unwrap_or(exe)
    } else {
        exe
    };
    if cfg!(target_os = "macos")
        && let Some(app) = app_bundle(&exe)
    {
        return swap_bundle(&new, app);
    }
    swap_binary(&new, &exe)
}

/// Moves `llama.old` back after a failed swap.
fn restore_llama(llama: &Path, old: &Path) {
    let _ = fs::remove_dir_all(llama);
    let _ = rename_retry(old, llama);
}

/// Windows and Linux (and a bare macOS binary): the running exe steps aside to
/// `<exe>.old`, the new one and its llama/ folder take its place.
fn swap_binary(new: &Path, exe: &Path) -> Result<()> {
    let dir = exe.parent().context("The app has no folder")?;
    let new_llama = new.parent().map(|p| p.join("llama")).filter(|p| p.is_dir());
    let llama = dir.join("llama");
    let llama_old = dir.join("llama.old");
    // llama.cpp first: if it cannot move, the app stays as it was.
    if let Some(src) = &new_llama {
        let _ = fs::remove_dir_all(&llama_old);
        if llama.exists() {
            rename_retry(&llama, &llama_old).context(
                "Could not move the old llama folder aside (is llama-server still running?)",
            )?;
        }
        if let Err(e) = copy_dir(src, &llama) {
            restore_llama(&llama, &llama_old);
            return Err(e.context("Could not install the new llama folder"));
        }
    }
    let undo = || {
        if new_llama.is_some() {
            restore_llama(&llama, &llama_old);
        }
    };
    let fresh = with_suffix(exe, ".new");
    if let Err(e) = fs::copy(new, &fresh) {
        let _ = fs::remove_file(&fresh);
        undo();
        return Err(
            anyhow::Error::from(e).context("Could not copy the new app next to the old one")
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&fresh, fs::Permissions::from_mode(0o755));
    }
    let mut old = with_suffix(exe, ".old");
    if old.exists() && fs::remove_file(&old).is_err() {
        // A previous .old is still in use; cleanup_old removes both later.
        old = with_suffix(exe, &format!(".old-{}", std::process::id()));
    }
    if let Err(e) = rename_retry(exe, &old) {
        let _ = fs::remove_file(&fresh);
        undo();
        return Err(e);
    }
    if let Err(e) = rename_retry(&fresh, exe) {
        let _ = rename_retry(&old, exe);
        let _ = fs::remove_file(&fresh);
        undo();
        return Err(e);
    }
    hide(&mut Command::new(exe))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "The update is installed but {} did not start; start it by hand",
                exe.display()
            )
        })?;
    Ok(())
}

/// macOS: the whole bundle is replaced (Info.plist, resources and binary belong together).
fn swap_bundle(new: &Path, app: &Path) -> Result<()> {
    let fresh = with_suffix(app, ".new");
    let old = with_suffix(app, ".old");
    let _ = fs::remove_dir_all(&fresh);
    // ditto keeps the bundle's symlinks, modes and signature intact.
    let out = output(Command::new("ditto").arg(new).arg(&fresh))?;
    if !out.status.success() {
        let _ = fs::remove_dir_all(&fresh);
        bail!(
            "Could not copy the new app next to {}: {}",
            app.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // Our own download, verified against SHA256SUMS: lift the quarantine flag from it only.
    let _ = output(
        Command::new("xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(&fresh),
    );
    let _ = fs::remove_dir_all(&old);
    if let Err(e) = rename_retry(app, &old) {
        let _ = fs::remove_dir_all(&fresh);
        return Err(e);
    }
    if let Err(e) = rename_retry(&fresh, app) {
        let _ = rename_retry(&old, app);
        let _ = fs::remove_dir_all(&fresh);
        return Err(e);
    }
    Command::new("open")
        .arg("-n")
        .arg(app)
        .spawn()
        .with_context(|| {
            format!(
                "The update is installed but {} did not open; open it by hand",
                app.display()
            )
        })?;
    Ok(())
}

// ---- cleanup ---------------------------------------------------------------------------------------

/// Removes `<exe>.old*` / `<exe>.new*`, `llama.old` and `<App>.app.old/.new`.
/// False while something is still in use (the previous app is still exiting).
fn remove_leftovers() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return true;
    };
    let exe = if cfg!(unix) {
        exe.canonicalize().unwrap_or(exe)
    } else {
        exe
    };
    let mut targets = vec![exe.clone()];
    if cfg!(target_os = "macos")
        && let Some(app) = app_bundle(&exe)
    {
        targets.push(app.to_path_buf());
    }
    let mut clean = true;
    for (i, target) in targets.iter().enumerate() {
        let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
            continue;
        };
        let name = name.to_string_lossy();
        let prefixes = [format!("{name}.old"), format!("{name}.new")];
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            if prefixes.iter().any(|p| file.starts_with(p)) || (i == 0 && file == "llama.old") {
                clean &= remove_any(&entry.path()).is_ok();
            }
        }
    }
    clean
}

/// Removes staging folders for versions at or below `current` (installed or stale)
/// and anything that is not a version folder. Newer ones stay so downloads resume.
fn remove_finished_stages(root: &Path, current: &str) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if compare_versions(&name, current) != Some(Ordering::Greater) {
            let _ = remove_any(&entry.path());
        }
    }
}

/// Called at startup: clears what earlier updates left behind, in the background
/// (on Windows the previous exe stays locked for a moment while it exits).
pub fn cleanup_old() {
    std::thread::spawn(|| {
        for _ in 0..10 {
            if remove_leftovers() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        remove_finished_stages(&updates_dir(), VERSION);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_compare_as_semver() {
        assert!(is_newer("v0.3.0", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("v0.1.9", "0.2.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "1.0.0-rc.1"));
        assert!(is_newer("1.0.0-rc.2", "1.0.0-rc.1"));
        assert!(is_newer("1.0.0-beta.11", "1.0.0-beta.2"));
        assert!(is_newer("1.0.0-alpha.1", "1.0.0-alpha"));
        assert!(is_newer("1.0.0-alpha.beta", "1.0.0-alpha.1"));
        assert_eq!(compare_versions("v1.2", "1.2.0"), Some(Ordering::Equal));
        assert_eq!(
            compare_versions("1.2.3+build.5", "1.2.3"),
            Some(Ordering::Equal)
        );
        assert_eq!(compare_versions("nightly", "0.2.0"), None);
        assert_eq!(compare_versions("1.2.3.4", "0.2.0"), None);
        assert!(!is_newer("garbage", "0.2.0"));
        assert!(is_stable("v1.0.0") && !is_stable("1.0.0-rc.1"));
    }

    #[test]
    fn artifacts_match_release_yml() {
        assert_eq!(artifact_for("windows", "x86_64"), Some("windows-x64"));
        assert_eq!(artifact_for("linux", "x86_64"), Some("linux-x64"));
        assert_eq!(artifact_for("linux", "aarch64"), Some("linux-arm64"));
        assert_eq!(artifact_for("macos", "x86_64"), Some("macos-x64"));
        assert_eq!(artifact_for("macos", "aarch64"), Some("macos-arm64"));
        assert_eq!(artifact_for("windows", "aarch64"), None);
        assert_eq!(artifact_for("freebsd", "x86_64"), None);
    }

    fn release_json(tag: &str) -> Value {
        let asset = |name: String, size: u64| json!({"name": name, "size": size, "browser_download_url": format!("https://example.invalid/{name}")});
        json!({
            "tag_name": tag, "draft": false, "prerelease": false, "body": "Notes\r\nline",
            "assets": [
                asset(format!("kyttu-ai-{tag}-linux-arm64.zip"), 1),
                asset(format!("kyttu-ai-{tag}-linux-x64.zip"), 2),
                asset(format!("kyttu-ai-{tag}-macos-x64.zip"), 3),
                asset(SUMS.into(), 4),
            ]
        })
    }

    #[test]
    fn picks_this_platforms_asset() {
        let r = parse_release(&release_json("v0.3.0"), "0.2.0", "linux-x64")
            .unwrap()
            .unwrap();
        assert_eq!(r.asset_name, "kyttu-ai-v0.3.0-linux-x64.zip");
        assert_eq!(
            (r.tag.as_str(), r.version.as_str(), r.size),
            ("v0.3.0", "0.3.0", 2)
        );
        assert_eq!(r.notes, "Notes\nline");
        assert_eq!(
            r.sums_url.as_deref(),
            Some("https://example.invalid/SHA256SUMS.txt")
        );
        let arm = parse_release(&release_json("v0.3.0"), "0.2.0", "linux-arm64")
            .unwrap()
            .unwrap();
        assert_eq!(arm.asset_name, "kyttu-ai-v0.3.0-linux-arm64.zip");
        assert!(parse_release(&release_json("v0.3.0"), "0.2.0", "windows-x64").is_err());
    }

    #[test]
    fn skips_old_draft_and_prerelease() {
        assert!(
            parse_release(&release_json("v0.2.0"), "0.2.0", "linux-x64")
                .unwrap()
                .is_none()
        );
        assert!(
            parse_release(&release_json("v0.1.0"), "0.2.0", "linux-x64")
                .unwrap()
                .is_none()
        );
        assert!(
            parse_release(&release_json("v0.3.0-rc.1"), "0.2.0", "linux-x64")
                .unwrap()
                .is_none()
        );
        for flag in ["draft", "prerelease"] {
            let mut v = release_json("v0.3.0");
            v[flag] = json!(true);
            assert!(parse_release(&v, "0.2.0", "linux-x64").unwrap().is_none());
        }
        let mut v = release_json("v0.3.0");
        v["tag_name"] = json!("../evil");
        assert!(parse_release(&v, "0.2.0", "linux-x64").is_err());
    }

    #[test]
    fn reads_sha256sums() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let text = format!(
            "{a}  kyttu-ai-v0.3.0-linux-x64.zip\n{b} *kyttu-ai-v0.3.0-windows-x64.zip\nnothex  kyttu-ai-v0.3.0-macos-x64.zip\n"
        );
        assert_eq!(parse_sums(&text, "kyttu-ai-v0.3.0-linux-x64.zip"), Some(a));
        assert_eq!(
            parse_sums(&text, "kyttu-ai-v0.3.0-windows-x64.zip"),
            Some(b.to_lowercase())
        );
        assert_eq!(parse_sums(&text, "kyttu-ai-v0.3.0-macos-x64.zip"), None);
        assert_eq!(parse_sums(&text, "kyttu-ai-v0.3.0-linux-x64"), None);
    }

    #[test]
    fn reads_hash_tool_output() {
        let h = "0123456789abcdef".repeat(4);
        let spaced: String = h
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(" ");
        let certutil = format!(
            "SHA256 hash of C:\\x\\a.zip:\r\n{}\r\nCertUtil: -hashfile command completed successfully.\r\n",
            h.to_uppercase()
        );
        assert_eq!(parse_hash_output(&certutil), Some(h.clone()));
        assert_eq!(
            parse_hash_output(&format!("SHA256 hash of a.zip:\n{spaced}\n")),
            Some(h.clone())
        );
        assert_eq!(
            parse_hash_output(&format!("{h}  /tmp/a.zip\n")),
            Some(h.clone())
        );
        assert_eq!(
            parse_hash_output(&format!("\\{h}  /tmp/a\\\\b.zip\n")),
            Some(h.clone())
        );
        assert_eq!(parse_hash_output("sha256sum: a.zip: No such file\n"), None);
    }

    #[test]
    fn detects_cargo_target_folders() {
        assert!(in_cargo_target(Path::new(
            "/p/KyttuAI/target/release/kyttu-ai"
        )));
        assert!(in_cargo_target(Path::new(
            "/p/KyttuAI/target-pass/x86_64-apple-darwin/debug/kyttu-ai"
        )));
        assert!(in_cargo_target(Path::new(
            "/p/KyttuAI/TARGET/Release/kyttu-ai"
        )));
        assert!(!in_cargo_target(Path::new(
            "/Applications/KYTTU - AI.app/Contents/MacOS/kyttu-ai"
        )));
        assert!(!in_cargo_target(Path::new(
            "/home/me/Apps/release/kyttu-ai"
        )));
        assert!(!in_cargo_target(Path::new(
            "/home/me/targets/release/kyttu-ai"
        )));
    }

    #[test]
    fn finds_app_bundle() {
        let exe = Path::new("/Applications/KYTTU - AI.app/Contents/MacOS/kyttu-ai");
        assert_eq!(
            app_bundle(exe),
            Some(Path::new("/Applications/KYTTU - AI.app"))
        );
        assert_eq!(app_bundle(Path::new("/usr/local/bin/kyttu-ai")), None);
    }

    #[test]
    fn plain_names_only() {
        assert!(safe_name("v0.3.0") && safe_name("kyttu-ai-v0.3.0-macos-arm64.zip"));
        for bad in ["", ".", "..", ".hidden", "a/b", "a\\b", "v1 0"] {
            assert!(!safe_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn removes_finished_stages_only() {
        let root = tempfile::tempdir().unwrap();
        for name in ["v0.1.0", "v0.2.0", "v0.3.0", "junk"] {
            fs::create_dir_all(root.path().join(name).join(UNPACKED)).unwrap();
        }
        remove_finished_stages(root.path(), "0.2.0");
        let mut left: Vec<String> = fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["v0.3.0"]);
    }

    #[test]
    fn finds_payload_breadth_first() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("dist/llama")).unwrap();
        fs::write(root.path().join("dist/kyttu-ai"), b"x").unwrap();
        let want = |p: &Path| p.file_name().is_some_and(|n| n == "kyttu-ai");
        assert_eq!(
            find(root.path(), 3, &want),
            Some(root.path().join("dist/kyttu-ai"))
        );
        assert_eq!(find(root.path(), 0, &want), None);
    }
}
