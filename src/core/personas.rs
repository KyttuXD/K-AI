use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Persona {
    pub name: String,
    pub instructions: String,
    pub avatar_url: String,
    pub model: String,
    pub in_group: bool,
    /// "Available on Discord": only these characters answer in Discord bots.
    pub discord: bool,
}
impl Default for Persona {
    fn default() -> Self {
        Self {
            name: "Assistant".into(),
            instructions: "A thoughtful, practical coding partner. Explain decisions clearly."
                .into(),
            avatar_url: String::new(),
            model: String::new(),
            in_group: true,
            discord: false,
        }
    }
}

pub fn validate(personas: &[Persona]) -> anyhow::Result<()> {
    anyhow::ensure!(personas.len() <= 32, "Maximum 32 characters");
    let mut names = std::collections::HashSet::new();
    for p in personas {
        anyhow::ensure!(
            !p.name.trim().is_empty() && p.name.chars().count() <= 64,
            "Character names must be 1–64 characters"
        );
        anyhow::ensure!(
            names.insert(p.name.trim().to_lowercase()),
            "Character names must be unique"
        );
        anyhow::ensure!(
            p.instructions.chars().count() <= 4000,
            "Character instructions exceed 4000 characters"
        );
        anyhow::ensure!(
            p.avatar_url.is_empty()
                || (p.avatar_url.starts_with("https://") && p.avatar_url.len() <= 1000),
            "Avatar must be an HTTPS URL"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discord_flag_defaults_off_for_old_files() {
        let p: Persona = serde_json::from_str(r#"{"name":"Old"}"#).unwrap();
        assert!(!p.discord && p.in_group);
        assert!(validate(&[p]).is_ok());
    }
}
