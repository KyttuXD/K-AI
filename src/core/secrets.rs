//! Secrets in the OS keychain (Windows Credential Manager, macOS Keychain, Secret Service on
//! Linux). Nothing here is ever written to settings.json, and failures are returned, not panics.
use anyhow::{Context, Result, anyhow, ensure};

const SERVICE: &str = "kyttu-ai";

fn entry(key: &str) -> Result<keyring::Entry> {
    ensure!(valid_key(key), "Invalid keychain key");
    keyring::Entry::new(SERVICE, key).context("Could not open the system keychain")
}

fn valid_key(key: &str) -> bool {
    !key.trim().is_empty() && key.len() <= 200 && !key.chars().any(char::is_control)
}

/// The stored value, or None when it is missing, empty or the keychain is unavailable.
pub fn get(key: &str) -> Option<String> {
    entry(key)
        .ok()?
        .get_password()
        .ok()
        .filter(|v| !v.is_empty())
}

pub fn set(key: &str, value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "Nothing to save");
    entry(key)?
        .set_password(value)
        .context("Could not save to the system keychain")
}

/// Removes a stored value; a missing one is not an error.
pub fn delete(key: &str) -> Result<()> {
    match entry(key)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow!(e).context("Could not remove it from the system keychain")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_bad_keys_without_touching_the_keychain() {
        assert!(valid_key("discord-token-bot_1"));
        assert!(!valid_key("  "));
        assert!(!valid_key("a\nb"));
        assert!(!valid_key(&"k".repeat(201)));
        assert!(get("").is_none());
        assert!(set("", "x").is_err() && delete("").is_err());
        assert!(set("discord-token-test", "").is_err());
    }
}
