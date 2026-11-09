//! Secrets (bot tokens, webhook URLs) in a private `.env` file in the data folder. Nothing here
//! is ever written to settings.json, and failures are returned, not panics. Errors never carry a
//! secret value. Entries that older versions kept in the OS keychain are read once, copied into
//! the `.env` file and then removed from the keychain.
use super::config::data_dir;
use anyhow::{Context, Result, anyhow, ensure};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

const SERVICE: &str = "kyttu-ai";
const ENV_FILE: &str = ".env";
const ENV_TMP: &str = ".env.tmp";

/// Serializes every read-modify-write of the `.env` file inside this process.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- keychain (migration only)

fn entry(key: &str) -> Result<keyring::Entry> {
    ensure!(valid_key(key), "Invalid secret key");
    keyring::Entry::new(SERVICE, key).context("Could not open the system keychain")
}

fn valid_key(key: &str) -> bool {
    !key.trim().is_empty() && key.len() <= 200 && !key.chars().any(char::is_control)
}

/// An old keychain value, or None when missing, empty or the keychain is unavailable.
fn legacy_get(key: &str) -> Option<String> {
    entry(key)
        .ok()?
        .get_password()
        .ok()
        .filter(|v| !v.is_empty())
}

/// Best-effort removal of an old keychain entry; every failure is ignored.
fn legacy_delete(key: &str) {
    if let Ok(e) = entry(key) {
        let _ = e.delete_credential();
    }
}

// ---------------------------------------------------------------- .env keys and format

/// Keeps A-Z a-z 0-9 _ - (letters uppercased); anything else becomes `_`. `-` stays so bot names
/// like `a-b` and `a_b` keep separate keys.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The `.env` key for a kind of secret and a name: `KYTTU_<KIND>_<NAME>`.
#[allow(dead_code)] // part of the module API, used by tests and diagnostics
pub fn env_key(kind: &str, name: &str) -> String {
    format!("KYTTU_{}_{}", sanitize(kind), sanitize(name))
}

/// Where the `.env` file lives: the data folder.
#[allow(dead_code)] // part of the module API, used by tests and diagnostics
pub fn env_path() -> PathBuf {
    data_dir().join(ENV_FILE)
}

/// Writes a value as one double-quoted line: `\`, `"` and line breaks are escaped, so the stored
/// line never has a raw break and the value reads back exactly.
fn encode_value(v: &str) -> String {
    let mut o = String::with_capacity(v.len() + 2);
    o.push('"');
    for c in v.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Reads a stored value: double quotes undo `encode_value`, single quotes are stripped as they
/// are, anything else is the trimmed text (hand-written lines).
fn decode_value(raw: &str) -> String {
    let v = raw.trim();
    let b = v.as_bytes();
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        let mut o = String::with_capacity(v.len());
        let mut it = v[1..v.len() - 1].chars();
        while let Some(c) = it.next() {
            if c != '\\' {
                o.push(c);
                continue;
            }
            match it.next() {
                Some('\\') => o.push('\\'),
                Some('"') => o.push('"'),
                Some('n') => o.push('\n'),
                Some('r') => o.push('\r'),
                Some(other) => {
                    o.push('\\');
                    o.push(other);
                }
                None => o.push('\\'),
            }
        }
        o
    } else if b.len() >= 2 && b[0] == b'\'' && b[b.len() - 1] == b'\'' {
        v[1..v.len() - 1].to_owned()
    } else {
        v.to_owned()
    }
}

/// `(key, raw value)` of a `KEY=value` line; None for comments, blanks and lines without `=`.
fn parse_line(line: &str) -> Option<(&str, &str)> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let (k, v) = t.split_once('=')?;
    Some((k.trim(), v))
}

/// File lines with CRLF tolerated; a missing file is no lines.
fn read_lines(path: &Path) -> Result<Vec<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text.lines().map(str::to_owned).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(anyhow!(e).context("Could not read the private secrets file")),
    }
}

// ---------------------------------------------------------------- .env read/write

fn env_get_in(dir: &Path, key: &str) -> Option<String> {
    let key = sanitize(key);
    if key.is_empty() {
        return None;
    }
    let lines = read_lines(&dir.join(ENV_FILE)).ok()?;
    lines
        .iter()
        .filter_map(|l| parse_line(l))
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| decode_value(v))
        .next_back()
        .filter(|v| !v.is_empty())
}

/// Replaces the `.env` file atomically: temp file in the same folder, then rename.
fn write_atomic(dir: &Path, lines: &[String]) -> Result<()> {
    use std::io::Write;
    ensure!(
        dir.is_absolute(),
        "No safe data folder for saved credentials"
    );
    std::fs::create_dir_all(dir).context("Could not create the data folder")?;
    let tmp = dir.join(ENV_TMP);
    let dest = dir.join(ENV_FILE);
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    let result = (|| -> Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        // Create the file empty, lock its access down, and only then write the secret.
        let f = opts
            .open(&tmp)
            .context("Could not write the private secrets file")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))
                .context("Could not protect the private secrets file")?;
        }
        drop(f);
        ensure!(
            restrict_access(&tmp),
            "Could not restrict access to the private secrets file"
        );
        let mut f = opts
            .open(&tmp)
            .context("Could not write the private secrets file")?;
        f.write_all(text.as_bytes())
            .context("Could not write the private secrets file")?;
        f.sync_all().ok();
        drop(f);
        std::fs::rename(&tmp, &dest).context("Could not save the private secrets file")
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Windows: drop inherited access and keep only the current user. False when that did not work
/// (the caller then refuses to write the secret). The renamed file keeps this access list.
#[cfg(windows)]
fn restrict_access(path: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Some(user) = std::env::var_os("USERNAME").filter(|u| !u.is_empty()) else {
        return false;
    };
    let mut grant = std::ffi::OsString::new();
    if let Some(domain) = std::env::var_os("USERDOMAIN").filter(|d| !d.is_empty()) {
        grant.push(domain);
        grant.push("\\");
    }
    grant.push(user);
    grant.push(":F");
    Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(grant)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|s| s.success())
}

/// Other systems: the file is already created with mode 0600 (Unix); nothing more to do.
#[cfg(not(windows))]
fn restrict_access(_path: &Path) -> bool {
    true
}

fn env_set_in(dir: &Path, key: &str, value: &str) -> Result<()> {
    let key = sanitize(key);
    ensure!(!key.is_empty(), "Invalid secret key");
    let encoded = encode_value(value);
    // Last check: the escaped value must never carry a raw line break into the file.
    ensure!(
        !encoded.contains(['\n', '\r']),
        "A secret cannot contain a line break"
    );
    let _guard = lock();
    let mut lines = read_lines(&dir.join(ENV_FILE))?;
    let new_line = format!("{key}={encoded}");
    let mut replaced = false;
    // Replace the first match, drop any duplicates of the same key.
    lines = lines
        .into_iter()
        .filter_map(|l| {
            if parse_line(&l).is_some_and(|(k, _)| k == key) {
                if replaced {
                    None
                } else {
                    replaced = true;
                    Some(new_line.clone())
                }
            } else {
                Some(l)
            }
        })
        .collect();
    if !replaced {
        lines.push(new_line);
    }
    write_atomic(dir, &lines)
}

fn env_delete_in(dir: &Path, key: &str) -> Result<()> {
    let key = sanitize(key);
    if key.is_empty() {
        return Ok(());
    }
    let _guard = lock();
    let path = dir.join(ENV_FILE);
    let lines = read_lines(&path)?;
    let kept: Vec<String> = lines
        .iter()
        .filter(|l| parse_line(l).is_none_or(|(k, _)| k != key))
        .cloned()
        .collect();
    if kept.len() == lines.len() {
        return Ok(());
    }
    write_atomic(dir, &kept)
}

/// The value of a key in the `.env` file, or None when missing or empty.
#[allow(dead_code)] // part of the module API, used by tests and diagnostics
pub fn env_get(key: &str) -> Option<String> {
    env_get_in(&data_dir(), key)
}

/// Saves a value (replacing an earlier one); lines and keys that are not this one stay as they were.
pub fn env_set(key: &str, value: &str) -> Result<()> {
    env_set_in(&data_dir(), key, value)
}

/// Removes a key; a missing one is not an error.
#[allow(dead_code)] // part of the module API, used by tests and diagnostics
pub fn env_delete(key: &str) -> Result<()> {
    env_delete_in(&data_dir(), key)
}

// ---------------------------------------------------------------- public API (same signatures)

/// Keys whose old keychain entry was already asked for in this process (asked at most once).
static LEGACY_TRIED: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn get_in(
    dir: &Path,
    key: &str,
    tried: &Mutex<HashSet<String>>,
    old_get: &dyn Fn(&str) -> Option<String>,
    old_delete: &dyn Fn(&str),
) -> Option<String> {
    if !valid_key(key) {
        return None;
    }
    if let Some(v) = env_get_in(dir, key) {
        return Some(v);
    }
    // One-time migration of an entry an older version kept in the keychain.
    let first = tried
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_owned());
    if !first {
        return None;
    }
    let v = old_get(key).filter(|v| !v.is_empty())?;
    if env_set_in(dir, key, &v).is_ok() {
        old_delete(key);
    } else {
        // Not copied: allow another try later instead of losing the old entry.
        tried.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }
    Some(v)
}

fn delete_in(dir: &Path, key: &str, old_delete: &dyn Fn(&str)) -> Result<()> {
    ensure!(valid_key(key), "Invalid secret key");
    let res = env_delete_in(dir, key);
    old_delete(key);
    res
}

/// The saved value, or None when missing or empty. Reads the `.env` file first; an entry still in
/// the old keychain is copied into the file once and removed from the keychain.
pub fn get(key: &str) -> Option<String> {
    get_in(&data_dir(), key, &LEGACY_TRIED, &legacy_get, &legacy_delete)
}

/// Saves a value in the `.env` file (never in the keychain, settings or exports).
pub fn set(key: &str, value: &str) -> Result<()> {
    ensure!(valid_key(key), "Invalid secret key");
    ensure!(!value.is_empty(), "Nothing to save");
    env_set(key, value)
}

/// Removes a saved value (and any old keychain copy); a missing one is not an error.
pub fn delete(key: &str) -> Result<()> {
    delete_in(&data_dir(), key, &legacy_delete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn read(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(ENV_FILE)).unwrap()
    }

    #[test]
    fn rejects_bad_keys_without_touching_anything() {
        assert!(valid_key("discord-token-bot_1"));
        assert!(!valid_key("  "));
        assert!(!valid_key("a\nb"));
        assert!(!valid_key(&"k".repeat(201)));
        assert!(get("").is_none());
        assert!(set("", "x").is_err() && delete("").is_err());
        assert!(set("discord-token-test", "").is_err());
    }

    #[test]
    fn key_sanitizing() {
        assert_eq!(sanitize("my-bot.1 x"), "MY-BOT_1_X");
        assert_eq!(env_key("token", "My Bot"), "KYTTU_TOKEN_MY_BOT");
        assert_eq!(env_key("web-hook", "a/b"), "KYTTU_WEB-HOOK_A_B");
    }

    #[test]
    fn similar_bot_names_keep_separate_lines() {
        let d = tempfile::tempdir().unwrap();
        env_set_in(d.path(), "discord-token-a-b", "one").unwrap();
        env_set_in(d.path(), "discord-token-a_b", "two").unwrap();
        assert_eq!(
            env_get_in(d.path(), "discord-token-a-b").as_deref(),
            Some("one")
        );
        assert_eq!(
            env_get_in(d.path(), "discord-token-a_b").as_deref(),
            Some("two")
        );
        assert_eq!(read(d.path()).lines().count(), 2);
    }

    #[test]
    fn multi_line_and_quoted_values_round_trip() {
        let d = tempfile::tempdir().unwrap();
        for v in [
            "1=https://x/a\n2=https://x/b",
            "\"x\"",
            "'y'",
            "  spaced  ",
            "back\\slash\\n and \r\nmix",
        ] {
            env_set_in(d.path(), "K", v).unwrap();
            assert_eq!(env_get_in(d.path(), "K").as_deref(), Some(v));
            assert_eq!(read(d.path()).lines().count(), 1);
        }
    }

    #[test]
    fn round_trip_and_overwrite() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(env_get_in(d.path(), "A"), None);
        env_set_in(d.path(), "KYTTU_TOKEN_A", "abc123").unwrap();
        assert_eq!(
            env_get_in(d.path(), "KYTTU_TOKEN_A").as_deref(),
            Some("abc123")
        );
        // Lookup sanitizes the same way (case-insensitive; `-` stays distinct from `_`).
        assert_eq!(
            env_get_in(d.path(), "kyttu_token_a").as_deref(),
            Some("abc123")
        );
        assert_eq!(env_get_in(d.path(), "kyttu-token-a"), None);
        env_set_in(d.path(), "KYTTU_TOKEN_A", "def=456").unwrap();
        assert_eq!(
            env_get_in(d.path(), "KYTTU_TOKEN_A").as_deref(),
            Some("def=456")
        );
        assert_eq!(read(d.path()), "KYTTU_TOKEN_A=\"def=456\"\n");
        assert!(!d.path().join(ENV_TMP).exists());
    }

    #[test]
    fn preserves_comments_unknown_keys_and_other_secrets() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(ENV_FILE),
            "# my notes\n\nOTHER=keep me\nKYTTU_A=old\nweird line\n# end\n",
        )
        .unwrap();
        env_set_in(d.path(), "KYTTU_A", "new").unwrap();
        env_set_in(d.path(), "KYTTU_B", "two").unwrap();
        assert_eq!(
            read(d.path()),
            "# my notes\n\nOTHER=keep me\nKYTTU_A=\"new\"\nweird line\n# end\nKYTTU_B=\"two\"\n"
        );
        env_delete_in(d.path(), "KYTTU_A").unwrap();
        assert_eq!(
            read(d.path()),
            "# my notes\n\nOTHER=keep me\nweird line\n# end\nKYTTU_B=\"two\"\n"
        );
        assert_eq!(env_get_in(d.path(), "OTHER").as_deref(), Some("keep me"));
    }

    #[test]
    fn reads_quotes_and_crlf_and_writes_lf() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(ENV_FILE),
            "# c\r\nA=\"quoted value\"\r\nB='single'\r\nC=plain\r\n",
        )
        .unwrap();
        assert_eq!(env_get_in(d.path(), "A").as_deref(), Some("quoted value"));
        assert_eq!(env_get_in(d.path(), "B").as_deref(), Some("single"));
        assert_eq!(env_get_in(d.path(), "C").as_deref(), Some("plain"));
        env_set_in(d.path(), "D", "x").unwrap();
        let text = read(d.path());
        assert!(!text.contains('\r'));
        // Other lines are kept as they were; new values are written double-quoted.
        assert_eq!(
            text,
            "# c\nA=\"quoted value\"\nB='single'\nC=plain\nD=\"x\"\n"
        );
    }

    #[test]
    fn relative_data_folder_is_refused() {
        let err = env_set_in(Path::new("KyttuAI-relative"), "A", "secretvalue").unwrap_err();
        assert!(!format!("{err:#}").contains("secretvalue"));
        assert!(!Path::new("KyttuAI-relative").exists());
    }

    #[test]
    fn delete_of_absent_key_is_fine() {
        let d = tempfile::tempdir().unwrap();
        env_delete_in(d.path(), "NOPE").unwrap();
        assert!(!d.path().join(ENV_FILE).exists());
        env_set_in(d.path(), "A", "1").unwrap();
        env_delete_in(d.path(), "NOPE").unwrap();
        assert_eq!(read(d.path()), "A=\"1\"\n");
        env_delete_in(d.path(), "A").unwrap();
        assert_eq!(env_get_in(d.path(), "A"), None);
    }

    #[test]
    fn migrates_old_keychain_entry_once() {
        let d = tempfile::tempdir().unwrap();
        let deleted = RefCell::new(Vec::<String>::new());
        let reads = Cell::new(0);
        let old_get = |_: &str| {
            reads.set(reads.get() + 1);
            Some("legacy-token".to_owned())
        };
        let old_delete = |k: &str| deleted.borrow_mut().push(k.to_owned());
        let tried = Mutex::new(HashSet::new());
        let v = get_in(d.path(), "discord-token-bot", &tried, &old_get, &old_delete);
        assert_eq!(v.as_deref(), Some("legacy-token"));
        assert_eq!(deleted.borrow().as_slice(), ["discord-token-bot"]);
        // Second read comes from the file; the keychain is not asked again.
        let v = get_in(d.path(), "discord-token-bot", &tried, &old_get, &old_delete);
        assert_eq!(v.as_deref(), Some("legacy-token"));
        assert_eq!(reads.get(), 1);
        assert_eq!(read(d.path()), "DISCORD-TOKEN-BOT=\"legacy-token\"\n");
        // Nothing anywhere means None, and the keychain is asked only once per key.
        let asked = Cell::new(0);
        let none = |_: &str| {
            asked.set(asked.get() + 1);
            None
        };
        assert!(get_in(d.path(), "other", &tried, &none, &|_: &str| {}).is_none());
        assert!(get_in(d.path(), "other", &tried, &none, &|_: &str| {}).is_none());
        assert_eq!(asked.get(), 1);
    }

    #[test]
    fn delete_also_clears_the_old_keychain_entry() {
        let d = tempfile::tempdir().unwrap();
        env_set_in(d.path(), "k1", "v").unwrap();
        let deleted = RefCell::new(0);
        delete_in(d.path(), "k1", &|_: &str| *deleted.borrow_mut() += 1).unwrap();
        assert_eq!(env_get_in(d.path(), "k1"), None);
        assert_eq!(*deleted.borrow(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        env_set_in(d.path(), "A", "1").unwrap();
        env_set_in(d.path(), "A", "2").unwrap();
        let mode = std::fs::metadata(d.path().join(ENV_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
