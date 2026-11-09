//! Character presets: ready-made characters. A preset file is a character (see `Persona`) plus an
//! optional "description". Built-ins are embedded from `assets/presets`; your own live in
//! `<data>/presets/*.json` and win over a built-in with the same name. Nothing is installed
//! automatically: the user picks a preset to copy it into their characters.

use super::{
    config::{data_dir, save_json},
    personas::{self, Persona, same_name},
};
use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};

/// Built-in presets: (file name, JSON).
const BUILTIN: &[(&str, &str)] = &[(
    "cheese-balls-bot.json",
    include_str!("../../assets/presets/cheese-balls-bot.json"),
)];
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_DESCRIPTION: usize = 300;
const MAX_SLUG: usize = 48;

#[derive(Clone, Debug)]
pub struct Preset {
    pub description: String,
    pub persona: Persona,
    pub builtin: bool,
}

pub fn presets_dir() -> PathBuf {
    data_dir().join("presets")
}

/// Built-in presets (minus any a user preset replaces), then the user's, in file name order.
pub fn all() -> Vec<Preset> {
    all_in(&presets_dir())
}

/// Saves `p` as `<slug of its name>.json` in the presets folder (replacing a preset with that name).
pub fn save_user(p: &Persona, description: &str) -> Result<()> {
    save_in(&presets_dir(), p, description)
}

/// Deletes the user preset called `name`. Built-ins cannot be deleted.
pub fn delete_user(name: &str) -> Result<()> {
    delete_in(&presets_dir(), name)
}

/// A file-name-safe version of a character name: lowercase ASCII letters and digits joined by
/// single dashes, never a path, never a reserved device name. A name that loses characters this
/// way gets a short hash so different names keep different files.
pub fn slug(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    let mut out = String::new();
    let mut dash = false;
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else {
            dash = true;
        }
    }
    out.truncate(MAX_SLUG);
    while out.ends_with('-') {
        out.pop();
    }
    // Plain spaces between words are not lossy; anything else dropped is.
    let spaced = lower.split_whitespace().collect::<Vec<_>>().join("-");
    if out != spaced || out.is_empty() {
        // FNV-1a: stable across runs and platforms.
        let hash = lower.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        let short = format!("{:08x}", hash & 0xffff_ffff);
        out = if out.is_empty() {
            format!("preset-{short}")
        } else {
            format!("{out}-{short}")
        };
    }
    const RESERVED: &[&str] = &[
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if RESERVED.contains(&out.as_str()) {
        out = format!("preset-{out}");
    }
    out
}

fn parse(json: &str) -> Option<(String, Persona)> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let description: String = value
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or_default()
        .trim()
        .chars()
        .take(MAX_DESCRIPTION)
        .collect();
    let mut persona: Persona = serde_json::from_value(value).ok()?;
    persona.name = persona.name.trim().to_string();
    personas::validate(std::slice::from_ref(&persona)).ok()?;
    Some((description, persona))
}

/// Regular `.json` files of `dir`, sorted by name. Symlinks, folders and huge files are left out.
fn files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut files: Vec<PathBuf> = read
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_ok_and(|t| t.is_file())
                && e.metadata().is_ok_and(|m| m.len() <= MAX_FILE_BYTES)
        })
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("json"))
        })
        .collect();
    files.sort();
    files
}

fn user_presets(dir: &Path) -> Vec<(PathBuf, Preset)> {
    files_in(dir)
        .into_iter()
        .filter_map(|path| {
            let (description, persona) = parse(&std::fs::read_to_string(&path).ok()?)?;
            Some((
                path,
                Preset {
                    description,
                    persona,
                    builtin: false,
                },
            ))
        })
        .collect()
}

fn all_in(dir: &Path) -> Vec<Preset> {
    let user = user_presets(dir);
    let mut out: Vec<Preset> = BUILTIN
        .iter()
        .filter_map(|(_, json)| parse(json))
        .filter(|(_, p)| {
            !user
                .iter()
                .any(|(_, u)| same_name(&u.persona.name, &p.name))
        })
        .map(|(description, persona)| Preset {
            description,
            persona,
            builtin: true,
        })
        .collect();
    let mut seen: Vec<String> = vec![];
    for (_, preset) in user {
        // Two files with one name: the first (by file name) wins.
        let key = preset.persona.name.trim().to_lowercase();
        if !seen.contains(&key) {
            seen.push(key);
            out.push(preset);
        }
    }
    out
}

fn save_in(dir: &Path, p: &Persona, description: &str) -> Result<()> {
    let mut persona = p.clone();
    persona.name = persona.name.trim().to_string();
    personas::validate(std::slice::from_ref(&persona))?;
    let description: String = description.trim().chars().take(MAX_DESCRIPTION).collect();
    // Reuse the file of an existing preset with this name, else a slug file name.
    let path = user_presets(dir)
        .into_iter()
        .find(|(_, u)| same_name(&u.persona.name, &persona.name))
        .map(|(path, _)| path)
        .unwrap_or_else(|| dir.join(format!("{}.json", slug(&persona.name))));
    ensure!(path.parent() == Some(dir), "Invalid preset name");
    let mut value = serde_json::to_value(&persona)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("description".into(), description.into());
    }
    save_json(&path, &value)
}

fn delete_in(dir: &Path, name: &str) -> Result<()> {
    let found = user_presets(dir)
        .into_iter()
        .find(|(_, u)| same_name(&u.persona.name, name));
    let (path, _) = found.ok_or_else(|| anyhow::anyhow!("No saved preset called {name}"))?;
    ensure!(path.parent() == Some(dir), "Invalid preset");
    std::fs::remove_file(&path)?;
    for suffix in [".bak", ".tmp", ".corrupt"] {
        let mut extra = path.as_os_str().to_owned();
        extra.push(suffix);
        let _ = std::fs::remove_file(extra);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persona(name: &str) -> Persona {
        Persona {
            name: name.into(),
            instructions: "Be brief.".into(),
            ..Default::default()
        }
    }

    #[test]
    fn slugs_are_plain_file_names() {
        assert_eq!(slug("Cheese Balls Bot"), "cheese-balls-bot");
        let s = slug("../../etc/passwd");
        assert!(!s.contains('/') && !s.contains('.') && !s.contains('\\'));
        assert!(s.starts_with("etc-passwd-"));
        assert_ne!(slug("Ada!"), slug("Ada?"));
        assert_eq!(slug("Ada"), slug(" ADA "));
        assert!(slug("日本語").starts_with("preset-"));
        assert_eq!(slug("con"), "preset-con");
        assert!(slug(&"x".repeat(200)).len() <= MAX_SLUG + 9);
        for name in ["", "..", "a/b", "C:\\x", "nul", "a\0b"] {
            let s = slug(name);
            assert!(!s.is_empty());
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            );
        }
    }

    #[test]
    fn built_in_list_parses() {
        for (file, json) in BUILTIN {
            assert!(parse(json).is_some(), "{file} does not parse");
        }
        let presets = all_in(Path::new("this-folder-does-not-exist"));
        assert_eq!(presets.len(), BUILTIN.len());
        let cheese = &presets[0];
        assert!(cheese.builtin);
        assert_eq!(cheese.persona.name, "Cheese Balls Bot");
        assert_eq!(cheese.description, "Says cheese balls.");
        assert_eq!(cheese.persona.instructions, "just says \"cheese balls\"");
        assert!(cheese.persona.in_group && !cheese.persona.discord);
    }

    #[test]
    fn user_preset_hides_built_in_with_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = persona("cheese balls bot");
        p.instructions = "Mine.".into();
        save_in(dir.path(), &p, "My version").unwrap();
        save_in(dir.path(), &persona("Other"), "").unwrap();
        let presets = all_in(dir.path());
        assert_eq!(presets.len(), 2);
        assert!(presets.iter().all(|p| !p.builtin));
        assert_eq!(presets[0].persona.instructions, "Mine.");
        assert_eq!(presets[0].description, "My version");
        delete_in(dir.path(), "Cheese Balls Bot").unwrap();
        let presets = all_in(dir.path());
        assert_eq!(presets.len(), 2);
        assert!(presets[0].builtin);
        assert!(delete_in(dir.path(), "Nobody").is_err());
    }

    #[test]
    fn bad_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("broken.json"), "{ not json").unwrap();
        std::fs::write(d.join("nameless.json"), r#"{"instructions":"x"}"#).unwrap();
        std::fs::write(d.join("array.json"), "[1,2]").unwrap();
        std::fs::write(d.join("notes.txt"), r#"{"name":"Txt"}"#).unwrap();
        std::fs::write(d.join("huge.json"), " ".repeat(MAX_FILE_BYTES as usize + 1)).unwrap();
        std::fs::create_dir(d.join("folder.json")).unwrap();
        std::fs::write(
            d.join("good.json"),
            r#"{"name":"Good","description":"Fine."}"#,
        )
        .unwrap();
        let presets = all_in(d);
        let names: Vec<&str> = presets.iter().map(|p| p.persona.name.as_str()).collect();
        assert_eq!(names, ["Cheese Balls Bot", "Good"]);
        assert_eq!(presets[1].description, "Fine.");
    }

    #[test]
    fn save_never_uses_a_path_from_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("presets");
        save_in(&sub, &persona("../../evil"), "d").unwrap();
        save_in(&sub, &persona("a/b\\c:d"), "").unwrap();
        let found: Vec<String> = std::fs::read_dir(&sub)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(found.len(), 2);
        assert!(
            found
                .iter()
                .all(|f| f.ends_with(".json") && !f.contains(".."))
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert_eq!(all_in(&sub).len(), BUILTIN.len() + 2);
        assert!(save_in(&sub, &persona("   "), "").is_err());
    }

    #[test]
    fn saving_twice_replaces_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = persona("Ada");
        p.memory = vec!["likes tea".into()];
        save_in(dir.path(), &p, " Math. ").unwrap();
        p.instructions = "Changed.".into();
        save_in(dir.path(), &p, "Math 2").unwrap();
        let user: Vec<Preset> = all_in(dir.path())
            .into_iter()
            .filter(|p| !p.builtin)
            .collect();
        assert_eq!(user.len(), 1);
        assert_eq!(user[0].persona.instructions, "Changed.");
        assert_eq!(user[0].persona.memory, ["likes tea"]);
        assert_eq!(user[0].description, "Math 2");
        delete_in(dir.path(), "ada").unwrap();
        assert!(all_in(dir.path()).iter().all(|p| p.builtin));
    }
}
