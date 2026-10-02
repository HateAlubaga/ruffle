//! Mod files on disk: `<config dir>/mods/*.toml`.
//!
//! The core knows nothing about files. This module lists, reads and edits the
//! mod files, hashes the SWF the mods are bound to, and hands the result to
//! `Player::install_mods`.

use ruffle_core::Player;
use ruffle_core::modkit::mods::{ModSource, RuleSpec, normalize_class};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use toml_edit::{DocumentMut, Item, Table, Value, value};

const MODS_DIR: &str = "mods";
/// A bigger "mod file" is certainly not one.
const MAX_MOD_BYTES: u64 = 1024 * 1024;

pub fn mods_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(MODS_DIR)
}

// ---------------------------------------------------------------------------
// Hashing the SWF
// ---------------------------------------------------------------------------

/// SHA-256 of a file, remembered until the file changes.
#[derive(Default)]
pub struct HashCache {
    cached: Option<((PathBuf, Option<SystemTime>, u64), String)>,
}

impl HashCache {
    pub fn hash(&mut self, path: &Path) -> Result<String, String> {
        let meta =
            std::fs::metadata(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
        let key = (path.to_path_buf(), meta.modified().ok(), meta.len());
        if let Some((cached_key, sha)) = &self.cached
            && *cached_key == key
        {
            return Ok(sha.clone());
        }
        let mut file =
            std::fs::File::open(path).map_err(|e| format!("Can't open {}: {e}", path.display()))?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)
            .map_err(|e| format!("Can't read {}: {e}", path.display()))?;
        let sha = format!("{:x}", hasher.finalize());
        self.cached = Some((key, sha.clone()));
        Ok(sha)
    }
}

// ---------------------------------------------------------------------------
// Listing and reading
// ---------------------------------------------------------------------------

/// Names, modification times and sizes of the mod files. Two equal values
/// mean nothing was added, removed or edited in between.
pub type Fingerprint = Vec<(String, Option<SystemTime>, u64)>;

struct Entry {
    name: String,
    path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
}

fn list(dir: &Path) -> Vec<Entry> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read_dir
        .flatten()
        .filter_map(|item| {
            let path = item.path();
            let is_toml = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"));
            let meta = item.metadata().ok()?;
            if !is_toml || !meta.is_file() {
                return None;
            }
            Some(Entry {
                name: item.file_name().to_string_lossy().into_owned(),
                path,
                modified: meta.modified().ok(),
                len: meta.len(),
            })
        })
        .collect();
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries
}

fn fingerprint_of(entries: &[Entry]) -> Fingerprint {
    entries
        .iter()
        .map(|e| (e.name.clone(), e.modified, e.len))
        .collect()
}

pub fn fingerprint(dir: &Path) -> Fingerprint {
    fingerprint_of(&list(dir))
}

fn read_sources(dir: &Path) -> (Vec<ModSource>, Fingerprint) {
    let entries = list(dir);
    let fingerprint = fingerprint_of(&entries);
    let sources = entries
        .into_iter()
        .map(|entry| {
            let text = if entry.len > MAX_MOD_BYTES {
                Err(format!("file is too big ({} bytes)", entry.len))
            } else {
                std::fs::read_to_string(&entry.path).map_err(|e| e.to_string())
            };
            ModSource {
                file: entry.name,
                text,
            }
        })
        .collect();
    (sources, fingerprint)
}

// ---------------------------------------------------------------------------
// Installing into the player
// ---------------------------------------------------------------------------

pub struct Loaded {
    pub files: usize,
    pub fingerprint: Fingerprint,
}

/// Reads all mod files and replaces the mods of the running game.
pub fn reload(
    player: &mut Player,
    config_dir: &Path,
    hashes: &mut HashCache,
    movie_url: &str,
) -> Loaded {
    let dir = mods_dir(config_dir);
    // A movie that was not loaded from a file has no hash; mods bound to a
    // SWF then show up as "other SWF" instead of applying to the wrong game.
    let sha = super::ffdec::swf_path(movie_url)
        .and_then(|path| hashes.hash(&path))
        .map_err(|e| tracing::debug!("No SWF hash for mods: {e}"))
        .ok();
    let (sources, fingerprint) = read_sources(&dir);
    let files = sources.len();
    player.install_mods(
        sources,
        sha,
        movie_url.to_string(),
        dir.to_string_lossy().into_owned(),
    );
    player
        .debug_ui()
        .modkit()
        .set_mods_loaded_for(Some(movie_url.to_string()));
    Loaded { files, fingerprint }
}

// ---------------------------------------------------------------------------
// Editing mod files
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub struct Saved {
    pub file: String,
    /// The file did not exist before.
    pub created: bool,
    /// A rule with the same key existed and got the new values.
    pub replaced: bool,
}

fn json_to_toml(json: &Json) -> Result<Value, String> {
    match json {
        Json::Bool(b) => Ok(Value::from(*b)),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::from(i))
            } else if let Some(f) = n.as_f64().filter(|f| f.is_finite()) {
                Ok(Value::from(f))
            } else {
                Err(format!("number {n} can't be stored"))
            }
        }
        Json::String(s) => Ok(Value::from(s.as_str())),
        other => Err(format!("{other} is not a boolean, number or string")),
    }
}

fn validate(rule: &RuleSpec) -> Result<(), String> {
    if rule.normalized_class().is_empty() {
        return Err("the rule has no class".into());
    }
    if rule.set.is_empty() {
        return Err("the rule sets nothing".into());
    }
    Ok(())
}

fn table_matches(table: &Table, rule: &RuleSpec) -> bool {
    let text = |key: &str| table.get(key).and_then(Item::as_str);
    let class = text("class").map(normalize_class);
    let exact = table.get("exact").and_then(Item::as_bool).unwrap_or(false);
    class.as_deref() == Some(rule.normalized_class().as_str())
        && text("on") == Some(rule.on.name())
        && text("target").unwrap_or("instance") == rule.target.name()
        && exact == rule.exact
}

fn rule_tables<'a>(
    doc: &'a mut DocumentMut,
    file: &str,
) -> Result<Option<&'a mut toml_edit::ArrayOfTables>, String> {
    match doc.get_mut("rule") {
        None => Ok(None),
        Some(item) => item
            .as_array_of_tables_mut()
            .map(Some)
            .ok_or_else(|| format!("{file}: `rule` is not a list of [[rule]] tables")),
    }
}

fn file_sha(doc: &DocumentMut) -> Option<String> {
    doc.get("mod")
        .and_then(|m| m.get("swf_sha256"))
        .and_then(Item::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("Can't write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Can't replace {}: {e}", path.display())
    })
}

/// A name that is safe to use as a file name on every platform.
fn sanitize_stem(stem: &str) -> String {
    let cleaned: String = stem
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let cleaned = cleaned.trim_matches('_');
    let reserved = ["CON", "PRN", "AUX", "NUL"];
    let is_port = |prefix: &str| {
        let bytes = cleaned.as_bytes();
        bytes.len() == 4
            && bytes[..3].eq_ignore_ascii_case(prefix.as_bytes())
            && bytes[3].is_ascii_digit()
    };
    if cleaned.is_empty()
        || reserved.iter().any(|r| cleaned.eq_ignore_ascii_case(r))
        || is_port("COM")
        || is_port("LPT")
    {
        format!("swf_{cleaned}")
    } else {
        cleaned.to_string()
    }
}

fn merge_rule(doc: &mut DocumentMut, file: &str, rule: &RuleSpec) -> Result<bool, String> {
    let values = rule
        .set
        .iter()
        .map(|(name, json)| {
            json_to_toml(json)
                .map(|v| (name.as_str(), v))
                .map_err(|e| format!("`{name}`: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if doc.get("rule").is_none() {
        doc.insert("rule", Item::ArrayOfTables(Default::default()));
    }
    let tables = rule_tables(doc, file)?.expect("just inserted");
    let existing = tables.iter_mut().find(|t| table_matches(t, rule));
    let replaced = existing.is_some();
    let table = match existing {
        Some(table) => table,
        None => {
            let mut table = Table::new();
            table["class"] = value(rule.normalized_class());
            table["on"] = value(rule.on.name());
            if rule.target.name() != "instance" {
                table["target"] = value(rule.target.name());
            }
            if rule.exact {
                table["exact"] = value(true);
            }
            tables.push(table);
            tables.iter_mut().last().expect("just pushed")
        }
    };
    let set = table
        .entry("set")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| format!("{file}: `set` is not a table"))?;
    for (name, v) in values {
        match set.get_mut(name) {
            Some(item) => {
                // Keep the comments around the old value.
                let decor = item.as_value().map(|old| old.decor().clone());
                *item = value(v);
                if let (Some(decor), Some(new)) = (decor, item.as_value_mut()) {
                    *new.decor_mut() = decor;
                }
            }
            None => {
                set.insert(name, value(v));
            }
        }
    }
    Ok(replaced)
}

/// Adds `rule` to the mod file of the SWF with hash `sha`, creating the file
/// if needed. An existing rule with the same class, trigger, target and
/// `exact` gets the new values; its other values stay.
pub fn save_rule(dir: &Path, swf_stem: &str, sha: &str, rule: &RuleSpec) -> Result<Saved, String> {
    validate(rule)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("Can't create {}: {e}", dir.display()))?;

    let stem = sanitize_stem(swf_stem);
    let short = &sha[..8.min(sha.len())];
    let candidates = [format!("{stem}.toml"), format!("{stem}-{short}.toml")];
    for name in &candidates {
        let path = dir.join(name);
        let (mut doc, created) = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let doc: DocumentMut = text
                    .parse()
                    .map_err(|e| format!("{name} is not valid TOML: {e}"))?;
                if file_sha(&doc).as_deref() != Some(sha) {
                    continue; // another SWF has this name
                }
                (doc, false)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut doc = DocumentMut::new();
                let mut header = Table::new();
                header["name"] = value(swf_stem);
                header["swf_sha256"] = value(sha);
                doc.insert("mod", Item::Table(header));
                (doc, true)
            }
            Err(e) => return Err(format!("Can't read {name}: {e}")),
        };
        let replaced = merge_rule(&mut doc, name, rule)?;
        write_atomic(&path, &doc.to_string())?;
        return Ok(Saved {
            file: name.clone(),
            created,
            replaced,
        });
    }
    Err(format!(
        "{} and {} exist, but are mods for other SWFs",
        candidates[0], candidates[1]
    ))
}

/// Removes the rule with the same key as `rule` from the mod file `file`.
pub fn remove_rule(dir: &Path, file: &str, rule: &RuleSpec) -> Result<(), String> {
    let plain_name = Path::new(file).file_name().and_then(|n| n.to_str()) == Some(file);
    if !plain_name || !file.to_ascii_lowercase().ends_with(".toml") {
        return Err(format!("{file:?} is not a mod file name"));
    }
    let path = dir.join(file);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("Can't read {file}: {e}"))?;
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| format!("{file} is not valid TOML: {e}"))?;
    let tables = rule_tables(&mut doc, file)?.ok_or_else(|| format!("{file} has no rules"))?;
    // Only the first match goes: `save_rule` merges into the first match too.
    let position = tables
        .iter()
        .position(|table| table_matches(table, rule))
        .ok_or_else(|| format!("{file} has no such rule (edited meanwhile?)"))?;
    tables.remove(position);
    if tables.is_empty() {
        doc.remove("rule");
    }
    write_atomic(&path, &doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruffle_core::modkit::mods::{Target, Trigger, parse_mod};

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    /// A fresh directory that is removed at the end of the test.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("ruffle-modkit-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rule(class: &str, on: Trigger, set: &[(&str, Json)]) -> RuleSpec {
        RuleSpec {
            class: class.into(),
            on,
            target: Target::Instance,
            exact: false,
            set: set
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }

    fn parsed(dir: &Path, file: &str) -> ruffle_core::modkit::mods::ModFile {
        parse_mod(&std::fs::read_to_string(dir.join(file)).unwrap()).unwrap()
    }

    #[test]
    fn save_creates_a_bound_file_and_parses_back() {
        let tmp = TempDir::new("create");
        let dir = tmp.0.join("mods");
        let boss = rule(
            "game::Boss",
            Trigger::Create,
            &[("hp", Json::from(10)), ("speed", Json::from(0.5))],
        );
        let saved = save_rule(&dir, "Epic Boss", SHA, &boss).unwrap();
        assert_eq!(
            saved,
            Saved {
                file: "Epic_Boss.toml".into(),
                created: true,
                replaced: false
            }
        );
        let file = parsed(&dir, "Epic_Boss.toml");
        assert_eq!(file.swf_sha256.as_deref(), Some(SHA));
        assert_eq!(file.rules.len(), 1);
        assert_eq!(file.rules[0].class, "game.Boss");
        assert_eq!(file.rules[0].on, Trigger::Create);
        assert_eq!(file.rules[0].set.len(), 2);
        assert!(file.rules[0].set.contains(&("hp".into(), Json::from(10))));
        assert!(
            file.rules[0]
                .set
                .contains(&("speed".into(), Json::from(0.5)))
        );
    }

    #[test]
    fn save_merges_by_key_and_keeps_comments() {
        let tmp = TempDir::new("merge");
        let dir = tmp.0.clone();
        let text = format!(
            "# my notes\n[mod]\nname = \"x\"\nswf_sha256 = \"{SHA}\"\n\n\
             [[rule]]\nclass = \"Boss\"\non = \"create\"\n[rule.set]\nhp = 1\nsize = 2\n"
        );
        std::fs::write(dir.join("game.toml"), text).unwrap();

        // Same key (`Boss` equals `a.Boss` only when written the same way, so
        // reuse the exact class): values merge.
        let again = rule(
            "Boss",
            Trigger::Create,
            &[("hp", Json::from(5)), ("armor", Json::Bool(true))],
        );
        let saved = save_rule(&dir, "game", SHA, &again).unwrap();
        assert!(saved.replaced && !saved.created);

        // Another trigger is another rule.
        let frozen = rule("Boss", Trigger::EveryFrame, &[("hp", Json::from(99))]);
        assert!(!save_rule(&dir, "game", SHA, &frozen).unwrap().replaced);
        let first = rule("Boss", Trigger::FirstFrame, &[("hp", Json::from(7))]);
        assert!(!save_rule(&dir, "game", SHA, &first).unwrap().replaced);

        let text = std::fs::read_to_string(dir.join("game.toml")).unwrap();
        assert!(text.contains("# my notes"), "{text}");
        let file = parse_mod(&text).unwrap();
        assert_eq!(file.rules.len(), 3);
        let create = &file.rules[0];
        assert!(create.set.contains(&("hp".into(), Json::from(5))));
        assert!(create.set.contains(&("size".into(), Json::from(2))));
        assert!(create.set.contains(&("armor".into(), Json::Bool(true))));
        assert_eq!(file.rules[1].on, Trigger::EveryFrame);
        assert_eq!(file.rules[2].on, Trigger::FirstFrame);
        assert!(text.contains("on = \"first_frame\""), "{text}");
    }

    #[test]
    fn save_never_touches_a_mod_of_another_swf() {
        let tmp = TempDir::new("foreign");
        let dir = tmp.0.clone();
        let foreign = format!("[mod]\nswf_sha256 = \"{OTHER_SHA}\"\n");
        std::fs::write(dir.join("game.toml"), &foreign).unwrap();
        let r = rule("Boss", Trigger::Create, &[("hp", Json::from(1))]);
        let saved = save_rule(&dir, "game", SHA, &r).unwrap();
        assert_eq!(saved.file, format!("game-{}.toml", &SHA[..8]));
        assert!(saved.created);
        assert_eq!(
            std::fs::read_to_string(dir.join("game.toml")).unwrap(),
            foreign
        );

        // Both names taken by other SWFs: refuse.
        std::fs::write(
            dir.join(&saved.file),
            format!("[mod]\nswf_sha256 = \"{OTHER_SHA}\"\n"),
        )
        .unwrap();
        assert!(save_rule(&dir, "game", SHA, &r).is_err());
    }

    #[test]
    fn save_refuses_broken_files_and_empty_rules() {
        let tmp = TempDir::new("broken");
        let dir = tmp.0.clone();
        std::fs::write(dir.join("game.toml"), "[mod\n").unwrap();
        let r = rule("Boss", Trigger::Create, &[("hp", Json::from(1))]);
        assert!(save_rule(&dir, "game", SHA, &r).is_err());
        // The broken file is left alone.
        assert_eq!(
            std::fs::read_to_string(dir.join("game.toml")).unwrap(),
            "[mod\n"
        );

        assert!(save_rule(&dir, "x", SHA, &rule("Boss", Trigger::Create, &[])).is_err());
        assert!(
            save_rule(
                &dir,
                "x",
                SHA,
                &rule(" ", Trigger::Create, &[("a", Json::from(1))])
            )
            .is_err()
        );
        let bad = rule("Boss", Trigger::Create, &[("a", Json::Null)]);
        assert!(save_rule(&dir, "x", SHA, &bad).is_err());
        assert!(!dir.join("x.toml").exists());
    }

    #[test]
    fn static_and_exact_are_written_and_matched() {
        let tmp = TempDir::new("static");
        let dir = tmp.0.clone();
        let mut r = rule(
            "Stats",
            Trigger::EveryFrame,
            &[("bulletNum", Json::from(9))],
        );
        r.target = Target::Static;
        r.exact = true;
        save_rule(&dir, "g", SHA, &r).unwrap();
        let file = parsed(&dir, "g.toml");
        assert_eq!(file.rules[0].target, Target::Static);
        assert!(file.rules[0].exact);
        // Same key again: replaced, not duplicated.
        assert!(save_rule(&dir, "g", SHA, &r).unwrap().replaced);
        assert_eq!(parsed(&dir, "g.toml").rules.len(), 1);
    }

    #[test]
    fn remove_deletes_only_the_matching_rule() {
        let tmp = TempDir::new("remove");
        let dir = tmp.0.clone();
        let a = rule("A", Trigger::Create, &[("x", Json::from(1))]);
        let b = rule("B", Trigger::Create, &[("y", Json::from(2))]);
        save_rule(&dir, "g", SHA, &a).unwrap();
        save_rule(&dir, "g", SHA, &b).unwrap();
        remove_rule(&dir, "g.toml", &a).unwrap();
        let file = parsed(&dir, "g.toml");
        assert_eq!(file.rules.len(), 1);
        assert_eq!(file.rules[0].class, "B");
        assert!(remove_rule(&dir, "g.toml", &a).is_err());
        remove_rule(&dir, "g.toml", &b).unwrap();
        let file = parsed(&dir, "g.toml");
        assert!(file.rules.is_empty());
        assert_eq!(file.swf_sha256.as_deref(), Some(SHA));
    }

    #[test]
    fn remove_deletes_one_of_two_equal_keys() {
        let tmp = TempDir::new("dupes");
        let dir = tmp.0.clone();
        let text = format!(
            "[mod]\nswf_sha256 = \"{SHA}\"\n\n\
             [[rule]]\nclass = \"A\"\non = \"create\"\n[rule.set]\nx = 1\n\n\
             [[rule]]\nclass = \"A\"\non = \"create\"\n[rule.set]\nx = 2\n"
        );
        std::fs::write(dir.join("g.toml"), text).unwrap();
        let a = rule("A", Trigger::Create, &[("x", Json::from(1))]);
        remove_rule(&dir, "g.toml", &a).unwrap();
        let file = parsed(&dir, "g.toml");
        assert_eq!(file.rules.len(), 1);
        assert!(file.rules[0].set.contains(&("x".into(), Json::from(2))));
    }

    #[test]
    fn remove_rejects_paths() {
        let tmp = TempDir::new("paths");
        let dir = tmp.0.join("mods");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let r = rule("A", Trigger::Create, &[("x", Json::from(1))]);
        let body = format!(
            "[mod]\nswf_sha256 = \"{SHA}\"\n\n[[rule]]\nclass = \"A\"\non = \"create\"\n[rule.set]\nx = 1\n"
        );
        // Real files, each with a matching rule, that the call must not touch.
        let files = [
            tmp.0.join("outside.toml"),
            dir.join("sub").join("x.toml"),
            dir.join("x.txt"),
        ];
        for file in &files {
            std::fs::write(file, &body).unwrap();
        }
        for bad in [
            "../outside.toml",
            "sub/x.toml",
            "x.txt",
            "",
            "..",
            "a\\b.toml",
        ] {
            let err = remove_rule(&dir, bad, &r).unwrap_err();
            assert!(err.contains("is not a mod file name"), "{bad}: {err}");
        }
        for file in &files {
            assert_eq!(std::fs::read_to_string(file).unwrap(), body);
        }
    }

    #[test]
    fn stems_are_safe_file_names() {
        assert_eq!(sanitize_stem("Epic Boss Fighter"), "Epic_Boss_Fighter");
        assert_eq!(sanitize_stem("../../evil"), "evil");
        assert_eq!(sanitize_stem("a/b\\c:d"), "a_b_c_d");
        assert_eq!(sanitize_stem("CON"), "swf_CON");
        assert_eq!(sanitize_stem("lpt1"), "swf_lpt1");
        assert_eq!(sanitize_stem("..."), "swf_");
        assert_eq!(sanitize_stem(""), "swf_");
        // Multi-byte names have a byte length of 4 too; they must not panic.
        assert_eq!(sanitize_stem("Ку"), "Ку");
        assert_eq!(sanitize_stem("a日"), "a日");
        assert_eq!(sanitize_stem("abé"), "abé");
        assert_eq!(sanitize_stem("Игра"), "Игра");
    }

    #[test]
    fn listing_ignores_other_files_and_fingerprints_changes() {
        let tmp = TempDir::new("list");
        let dir = tmp.0.clone();
        std::fs::write(dir.join("b.toml"), "[mod]\n").unwrap();
        std::fs::write(dir.join("A.TOML"), "[mod]\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::write(dir.join("c.toml.tmp"), "x").unwrap();
        std::fs::create_dir(dir.join("d.toml")).unwrap();
        let (sources, before) = read_sources(&dir);
        let names: Vec<_> = sources.iter().map(|s| s.file.as_str()).collect();
        assert_eq!(names, ["A.TOML", "b.toml"]);
        assert_eq!(fingerprint(&dir), before);
        std::fs::write(dir.join("b.toml"), "[mod]\nname = \"changed\"\n").unwrap();
        assert_ne!(fingerprint(&dir), before);
        assert!(read_sources(&tmp.0.join("missing")).0.is_empty());
    }

    /// Rewrites `path` with `bytes` and moves its modification time on, so the
    /// test does not depend on the file system clock resolution.
    fn rewrite_later(path: &Path, bytes: &[u8]) {
        let before = std::fs::metadata(path).unwrap().modified().unwrap();
        std::fs::write(path, bytes).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(before + std::time::Duration::from_secs(5))
            .unwrap();
    }

    #[test]
    fn same_size_edits_change_the_fingerprint() {
        let tmp = TempDir::new("same-size");
        let path = tmp.0.join("a.toml");
        std::fs::write(&path, "[mod]\nname = \"aaaa\"\n").unwrap();
        let before = fingerprint(&tmp.0);
        rewrite_later(&path, b"[mod]\nname = \"bbbb\"\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            before[0].2,
            "the edit must keep the size"
        );
        assert_ne!(fingerprint(&tmp.0), before);
    }

    #[test]
    fn oversized_files_are_reported_not_read() {
        let tmp = TempDir::new("big");
        let big = vec![b' '; MAX_MOD_BYTES as usize + 1];
        std::fs::write(tmp.0.join("big.toml"), big).unwrap();
        let (sources, _) = read_sources(&tmp.0);
        assert!(sources[0].text.is_err());
    }

    #[test]
    fn hashes_are_correct_and_cached_until_the_file_changes() {
        let tmp = TempDir::new("hash");
        let path = tmp.0.join("a.swf");
        std::fs::write(&path, b"abc").unwrap();
        let mut cache = HashCache::default();
        // SHA-256("abc")
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(cache.hash(&path).unwrap(), abc);
        assert_eq!(cache.hash(&path).unwrap(), abc);
        std::fs::write(&path, b"abcd").unwrap();
        assert_ne!(cache.hash(&path).unwrap(), abc);
        assert!(cache.hash(&tmp.0.join("missing")).is_err());
    }

    #[test]
    fn hash_cache_sees_same_size_edits() {
        let tmp = TempDir::new("hash-same-size");
        let path = tmp.0.join("a.swf");
        std::fs::write(&path, b"abc").unwrap();
        let mut cache = HashCache::default();
        let abc = cache.hash(&path).unwrap();
        rewrite_later(&path, b"abd");
        let abd = cache.hash(&path).unwrap();
        assert_ne!(abc, abd);
        // SHA-256("abd")
        let mut hasher = Sha256::new();
        hasher.update(b"abd");
        assert_eq!(abd, format!("{:x}", hasher.finalize()));
    }

    #[test]
    fn save_keeps_comments_on_overwritten_values() {
        let tmp = TempDir::new("value-comments");
        let dir = tmp.0.clone();
        let text = format!(
            "[mod]\nswf_sha256 = \"{SHA}\"\n\n\
             [[rule]]\nclass = \"Boss\"\non = \"create\"\n[rule.set]\n# the health\nhp = 1 # tail\n"
        );
        std::fs::write(dir.join("g.toml"), text).unwrap();
        let r = rule("Boss", Trigger::Create, &[("hp", Json::from(500))]);
        assert!(save_rule(&dir, "g", SHA, &r).unwrap().replaced);
        let text = std::fs::read_to_string(dir.join("g.toml")).unwrap();
        assert!(text.contains("# the health"), "{text}");
        assert!(text.contains("# tail"), "{text}");
        assert!(text.contains("hp = 500"), "{text}");
        assert!(
            parsed(&dir, "g.toml").rules[0]
                .set
                .contains(&("hp".into(), Json::from(500)))
        );
    }
}
