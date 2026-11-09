use serde::{Deserialize, Serialize};

/// Characters that take part in one group round.
pub const MAX_GROUP: usize = 6;
/// Matched lore added to one prompt, in characters.
pub const LORE_CAP: usize = 1500;
pub const MAX_LORE: usize = 200;
pub const MAX_LORE_TEXT: usize = 2000;
pub const MAX_NOTES: usize = 100;
pub const MAX_NOTE: usize = 500;
pub const MAX_NAME: usize = 64;
pub const MAX_INSTRUCTIONS: usize = 4000;

/// Character names compare trimmed and case-insensitively (Unicode).
pub fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// A lorebook entry: its text joins the prompt when one of its keys is mentioned.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LoreEntry {
    pub keys: Vec<String>,
    pub text: String,
}

/// A character. An empty `name` means "no character" (`Persona::default()`).
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
    pub lorebook: Vec<LoreEntry>,
    /// Notes the character always keeps in mind.
    pub memory: Vec<String>,
    /// Group chat order, ascending; ties keep list order.
    pub turn_order: i32,
}
impl Default for Persona {
    fn default() -> Self {
        Self {
            // An empty name means "no character": the plain agent.
            name: String::new(),
            instructions: String::new(),
            avatar_url: String::new(),
            model: String::new(),
            in_group: true,
            discord: false,
            lorebook: vec![],
            memory: vec![],
            turn_order: 0,
        }
    }
}

impl Persona {
    /// True for a real character (non-empty name); false for the plain agent.
    pub fn is_character(&self) -> bool {
        !self.name.trim().is_empty()
    }
}

pub fn validate(personas: &[Persona]) -> anyhow::Result<()> {
    anyhow::ensure!(personas.len() <= 32, "Maximum 32 characters");
    let mut names = std::collections::HashSet::new();
    for p in personas {
        anyhow::ensure!(
            !p.name.trim().is_empty() && p.name.chars().count() <= MAX_NAME,
            "Character names must be 1–{MAX_NAME} characters"
        );
        anyhow::ensure!(
            names.insert(p.name.trim().to_lowercase()),
            "Character names must be unique"
        );
        anyhow::ensure!(
            p.instructions.chars().count() <= MAX_INSTRUCTIONS,
            "Character instructions exceed {MAX_INSTRUCTIONS} characters"
        );
        anyhow::ensure!(
            p.avatar_url.is_empty()
                || (p.avatar_url.starts_with("https://") && p.avatar_url.len() <= 1000),
            "Avatar must be an HTTPS URL"
        );
        anyhow::ensure!(
            p.lorebook.len() <= MAX_LORE,
            "{}: at most {MAX_LORE} lore entries",
            p.name
        );
        for entry in &p.lorebook {
            anyhow::ensure!(
                entry.keys.len() <= 20 && entry.keys.iter().all(|k| k.chars().count() <= 64),
                "{}: lore entries take up to 20 keys of 64 characters",
                p.name
            );
            anyhow::ensure!(
                entry.text.chars().count() <= MAX_LORE_TEXT,
                "{}: a lore entry exceeds {MAX_LORE_TEXT} characters",
                p.name
            );
        }
        anyhow::ensure!(
            p.memory.len() <= MAX_NOTES && p.memory.iter().all(|n| n.chars().count() <= MAX_NOTE),
            "{}: up to {MAX_NOTES} memory notes of {MAX_NOTE} characters",
            p.name
        );
    }
    Ok(())
}

/// The group chat lineup: characters in group chats, by `turn_order` (ties keep list order),
/// at most `MAX_GROUP`.
pub fn group_order<'a>(personas: impl IntoIterator<Item = &'a Persona>) -> Vec<&'a Persona> {
    let mut members: Vec<&Persona> = personas.into_iter().filter(|p| p.in_group).collect();
    members.sort_by_key(|p| p.turn_order);
    members.truncate(MAX_GROUP);
    members
}

/// True when `key` (already lowercase) appears in `text` (already lowercase) as a whole word:
/// no letter or digit right before or after it.
fn mentions(text: &str, key: &str) -> bool {
    !key.is_empty()
        && text.match_indices(key).any(|(i, _)| {
            let before = text[..i].chars().next_back();
            let after = text[i + key.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
}

/// Lore entries whose keys appear (case-insensitive, whole words) in `message`, one per line,
/// in lorebook order and capped at `LORE_CAP` characters.
pub fn matching_lore(lorebook: &[LoreEntry], message: &str) -> String {
    let text = message.to_lowercase();
    let mut out = String::new();
    let mut used = 0;
    for entry in lorebook {
        let body = entry.text.trim();
        if body.is_empty()
            || !entry
                .keys
                .iter()
                .any(|k| mentions(&text, &k.trim().to_lowercase()))
        {
            continue;
        }
        let gap = usize::from(!out.is_empty());
        let room = LORE_CAP.saturating_sub(used + gap);
        if room == 0 {
            break;
        }
        if gap == 1 {
            out.push('\n');
        }
        let part: String = body.chars().take(room).collect();
        used += gap + part.chars().count();
        out.push_str(&part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lore(keys: &[&str], text: &str) -> LoreEntry {
        LoreEntry {
            keys: keys.iter().map(|k| k.to_string()).collect(),
            text: text.into(),
        }
    }
    #[test]
    fn discord_flag_defaults_off_for_old_files() {
        let p: Persona = serde_json::from_str(r#"{"name":"Old"}"#).unwrap();
        assert!(!p.discord && p.in_group);
        assert!(p.lorebook.is_empty() && p.memory.is_empty() && p.turn_order == 0);
        assert!(validate(&[p]).is_ok());
    }
    #[test]
    fn lore_matches_whole_words_case_insensitively() {
        let p = Persona {
            lorebook: vec![
                lore(&["Dragon"], "Dragons hoard gold."),
                lore(&["cat"], "The cat is named Mo."),
                lore(&["new york", " "], "A big city."),
                lore(&["ÉLAN"], "Élan is a ship."),
                lore(&["dragon"], "   "),
            ],
            ..Default::default()
        };
        assert_eq!(
            matching_lore(&p.lorebook, "Tell me about the DRAGON in New York!"),
            "Dragons hoard gold.\nA big city."
        );
        assert_eq!(matching_lore(&p.lorebook, "concatenate dragons"), "");
        assert_eq!(matching_lore(&p.lorebook, "élan?"), "Élan is a ship.");
        assert!(matching_lore(&p.lorebook, "").is_empty());
    }
    #[test]
    fn lore_is_capped() {
        let p = Persona {
            lorebook: vec![
                lore(&["a"], &"x".repeat(1000)),
                lore(&["a"], &"y".repeat(1000)),
            ],
            ..Default::default()
        };
        let text = matching_lore(&p.lorebook, "a");
        assert_eq!(text.chars().count(), LORE_CAP);
        assert!(text.starts_with('x') && text.ends_with('y'));
    }
    #[test]
    fn group_follows_turn_order_then_list_order() {
        let p = |name: &str, turn_order: i32, in_group: bool| Persona {
            name: name.into(),
            turn_order,
            in_group,
            ..Default::default()
        };
        let list = vec![
            p("A", 2, true),
            p("B", 1, true),
            p("C", 2, true),
            p("D", 0, false),
        ];
        let names: Vec<&str> = group_order(&list).iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["B", "A", "C"]);
        let many: Vec<Persona> = (0..9).map(|i| p(&i.to_string(), 0, true)).collect();
        assert_eq!(group_order(&many).len(), MAX_GROUP);
    }
    #[test]
    fn names_compare_trimmed_and_case_insensitive() {
        assert!(same_name(" Ada ", "ada"));
        assert!(same_name("ÉLODIE", "élodie"));
        assert!(!same_name("Ada", "Adam"));
    }
    #[test]
    fn validates_lore_and_notes() {
        let mut p = Persona {
            name: "Ada".into(),
            lorebook: vec![lore(&["k"], &"x".repeat(MAX_LORE_TEXT + 1))],
            ..Default::default()
        };
        assert!(validate(std::slice::from_ref(&p)).is_err());
        p.lorebook.clear();
        p.memory = vec!["n".repeat(MAX_NOTE + 1)];
        assert!(validate(&[p]).is_err());
    }
}
