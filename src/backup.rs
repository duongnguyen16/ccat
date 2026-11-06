//! Portable, private backups of ccat-owned accounts and preferences (excluding Gemini Web and CLI logins).
use crate::providers::gemini_web::store::write_atomic_secure;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Backup {
    format: String,
    version: u32,
    files: BTreeMap<String, String>,
}

const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FILES: usize = 4096;

fn root() -> Result<PathBuf, String> {
    if crate::remote::attached().is_some() {
        return Err("Switch to This PC before backing up or restoring.".into());
    }
    Ok(crate::config_path().with_file_name(""))
}

pub fn export(path: &Path) -> Result<usize, String> {
    let root = root()?;
    // Never let a save-file choice overwrite the live config or a credential file.
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    if let Ok(base) = fs::canonicalize(&root) {
        if fs::canonicalize(parent)
            .map_err(|e| e.to_string())?
            .starts_with(base)
        {
            return Err("Save the backup outside ccat's data folder.".into());
        }
    }
    if fs::symlink_metadata(path).is_ok_and(|m| linked(&m)) {
        return Err("The backup destination must not be a link.".into());
    }
    let backup = collect(&root)?;
    let count = backup.files.len();
    let bytes = serde_json::to_vec_pretty(&backup).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("Backup exceeds the 32 MiB limit.".into());
    }
    write_atomic_secure(path, &bytes)?;
    Ok(count)
}

pub fn inspect(path: &Path) -> Result<usize, String> {
    Ok(parse(&read_text(path)?)?.files.len())
}

pub fn import(path: &Path) -> Result<usize, String> {
    restore(&root()?, parse(&read_text(path)?)?)
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("Backup exceeds the 32 MiB limit.".into());
    }
    Ok(bytes)
}

fn read_text(path: &Path) -> Result<String, String> {
    String::from_utf8(read_bytes(path)?).map_err(|_| "Backup data must be UTF-8.".into())
}

fn safe_name(s: &str) -> bool {
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    !s.is_empty()
        && s.len() <= 240
        && !s.ends_with([' ', '.'])
        && !s.starts_with('.')
        && !s
            .chars()
            .any(|c| c.is_control() || "/\\:*?\"<>|".contains(c))
        && !matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        && !matches!(
            stem.strip_prefix("COM")
                .or_else(|| stem.strip_prefix("LPT")),
            Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
        )
}

fn allowed(name: &str) -> bool {
    if matches!(
        name,
        "config.json" | "machines.json" | "agent-token" | "chatgpt/host-id"
    ) {
        return true;
    }
    if let Some(n) = name.strip_prefix("profiles/") {
        return safe_name(n) && n.ends_with(".json");
    }
    if let Some(n) = name.strip_prefix("chatgpt/") {
        return safe_name(n) && n.ends_with(".json");
    }
    false
}

fn linked(m: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return true;
        } // junctions as well as symlinks
    }
    m.file_type().is_symlink()
}

/// Reject links inside the data root, including directory junctions on Windows.
/// Redirected user-profile ancestors are outside this boundary and remain supported.
fn target(root: &Path, name: &str) -> Result<PathBuf, String> {
    let p = root.join(name);
    for ancestor in p.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(m) if linked(&m) => {
                return Err(format!(
                    "Refusing linked backup path: {}",
                    ancestor.display()
                ))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Cannot access {}: {e}", ancestor.display())),
        }
        if ancestor == root {
            break;
        }
    }
    Ok(p)
}

fn collect(root: &Path) -> Result<Backup, String> {
    let mut backup = Backup {
        format: "ccat-backup".into(),
        version: 1,
        files: BTreeMap::new(),
    };
    let mut names: Vec<String> = [
        "config.json",
        "machines.json",
        "agent-token",
        "chatgpt/host-id",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    for folder in ["profiles", "chatgpt"] {
        let dir = target(root, folder)?;
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("Cannot read {}: {e}", dir.display())),
        };
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let file = entry
                .file_name()
                .into_string()
                .map_err(|_| "A backup filename is not valid Unicode.")?;
            let name = format!("{folder}/{file}");
            if allowed(&name) {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    let mut size = 0;
    for name in names {
        let path = target(root, &name)?;
        match fs::metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.to_string()),
            Ok(m) if !m.is_file() => return Err(format!("Not a regular file: {name}")),
            Ok(_) => {}
        }
        let text = read_text(&path)?;
        size += text.len() as u64;
        if size > MAX_BYTES || backup.files.len() >= MAX_FILES {
            return Err("Too much data for one backup.".into());
        }
        backup.files.insert(name, text);
    }
    exclude_web(&mut backup)?;
    validate(&backup)?;
    Ok(backup)
}

/// Old backups may contain Gemini files; ignore them without reading cookies or validating browser paths.
fn exclude_web(backup: &mut Backup) -> Result<(), String> {
    backup
        .files
        .retain(|name, _| !name.starts_with("gemini_web/"));
    if let Some(text) = backup.files.get_mut("config.json") {
        let mut config: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("Invalid config.json: {e}"))?;
        if let Some(providers) = config
            .get_mut("providers")
            .and_then(serde_json::Value::as_array_mut)
        {
            providers.retain(|p| p["gemini_web"] != true);
        }
        *text = serde_json::to_string(&config).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn parse(text: &str) -> Result<Backup, String> {
    if text.len() as u64 > MAX_BYTES {
        return Err("Backup exceeds the 32 MiB limit.".into());
    }
    let mut b: Backup =
        serde_json::from_str(text).map_err(|e| format!("Not a ccat backup: {e}"))?;
    exclude_web(&mut b)?;
    validate(&b)?;
    Ok(b)
}

fn validate(b: &Backup) -> Result<(), String> {
    if b.format != "ccat-backup" || b.version != 1 {
        return Err("Unsupported ccat backup format or version.".into());
    }
    if b.files.len() > MAX_FILES || !b.files.contains_key("config.json") {
        return Err("Backup must contain config.json (at most 4096 files).".into());
    }
    let mut seen = std::collections::HashSet::new();
    let mut size = 0;
    for (name, text) in &b.files {
        if !allowed(name) || !seen.insert(name.to_lowercase()) {
            return Err(format!("Unsafe or duplicate backup path: {name}"));
        }
        size += text.len() as u64;
        if size > MAX_BYTES {
            return Err("Backup exceeds the 32 MiB limit.".into());
        }
        if !name.ends_with(".json") {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("Invalid JSON in {name}: {e}"))?;
        let check = || -> Result<(), String> {
            if name == "config.json" {
                let cfg: crate::Config =
                    serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
                if !cfg.profile.is_empty() && !safe_name(&format!("{}.json", cfg.profile)) {
                    return Err("Unsafe active profile name.".into());
                }
                for p in cfg.providers {
                    for a in p.chatgpt_accounts {
                        if !safe_name(&format!("{}.json", a.key)) {
                            return Err("Unsafe ChatGPT account key.".into());
                        }
                    }
                }
            } else if name.starts_with("profiles/") {
                let _: [crate::Route; 4] =
                    serde_json::from_value(value.get("tiers").cloned().ok_or("Missing tiers")?)
                        .map_err(|e| e.to_string())?;
            } else if name == "machines.json" {
                let _: Vec<crate::remote::Machine> =
                    serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
            } else if !value.is_object() {
                return Err("Expected a JSON object.".into());
            }
            Ok(())
        };
        check().map_err(|e| format!("Invalid data in {name}: {e}"))?;
    }
    Ok(())
}

fn restore(root: &Path, mut backup: Backup) -> Result<usize, String> {
    exclude_web(&mut backup)?;
    validate(&backup)?;
    // Prepare and validate everything before touching live files.
    let mut writes = Vec::new();
    let mut preserved = 0u64;
    for (name, text) in &backup.files {
        let mut bytes = text.as_bytes().to_vec();
        let path = target(root, name)?;
        let old = match fs::symlink_metadata(&path) {
            Ok(_) => Some(read_bytes(&path).map_err(|e| format!("Cannot preserve {name}: {e}"))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("Cannot preserve {name}: {e}")),
        };
        if name == "config.json" {
            if let Some(local) = &old {
                let local: serde_json::Value = serde_json::from_slice(local)
                    .map_err(|e| format!("Cannot preserve local Gemini settings: {e}"))?;
                let web: Vec<_> = local["providers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|p| p["gemini_web"] == true)
                    .cloned()
                    .collect();
                if !web.is_empty() {
                    let mut config: serde_json::Value =
                        serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                    if config.get("providers").is_none() {
                        config["providers"] = serde_json::json!([]);
                    }
                    config["providers"]
                        .as_array_mut()
                        .ok_or("Invalid providers list")?
                        .extend(web);
                    bytes = serde_json::to_vec(&config).map_err(|e| e.to_string())?;
                }
            }
        }
        preserved += old.as_ref().map_or(0, |b| b.len() as u64);
        if preserved > MAX_BYTES {
            return Err(
                "Existing data exceeds the 32 MiB rollback limit; no files were changed.".into(),
            );
        }
        writes.push((path, bytes, old));
    }
    // Config last: sessions see the new account references only once credentials are in place.
    writes.sort_by_key(|(p, _, _)| p == &root.join("config.json"));
    for (i, (path, bytes, _)) in writes.iter().enumerate() {
        if let Err(e) = write_atomic_secure(path, bytes) {
            let mut failures = Vec::new();
            for (p, _, old) in writes[..=i].iter().rev() {
                // The failed writer may have renamed before its final ACL check; compare rather than assume.
                if old
                    .as_ref()
                    .is_some_and(|b| read_bytes(p).is_ok_and(|current| current == *b))
                {
                    continue;
                }
                let result = match old {
                    Some(b) => write_atomic_secure(p, b),
                    None => match fs::remove_file(p) {
                        Ok(()) => Ok(()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(e) => Err(e.to_string()),
                    },
                };
                if let Err(err) = result {
                    failures.push(format!("{}: {err}", p.display()));
                }
            }
            return Err(if failures.is_empty() {
                format!("Restore failed; saved data was rolled back: {e}")
            } else {
                format!(
                    "Restore failed: {e}. Rollback also failed: {}",
                    failures.join("; ")
                )
            });
        }
    }
    Ok(writes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("ccat-backup-{}", crate::common::random_hex(8)));
        fs::create_dir_all(&p).unwrap();
        p
    }
    fn put(root: &Path, name: &str, text: &str) {
        let p = root.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    #[test]
    fn round_trip_other_providers_ignores_web_and_preserves_local_web() {
        let (src, dst) = (dir(), dir());
        put(
            &src,
            "config.json",
            r#"{"bypass":true,"providers":[{"name":"Antigravity","antigravity":true,"accounts":[{"email":"test@example.invalid","refresh":"test-refresh"}]},{"name":"ChatGPT","chatgpt":true},{"name":"Ollama","ollama":true,"key":"test-ollama-key"},{"name":"Example","key":"test-only-key"},{"name":"Gemini Web","gemini_web":true,"key":"must-not-export"}],"future_preference":42}"#,
        );
        put(&src, "profiles/Work.json", r#"{"tiers":[{},{},{},{}]}"#);
        put(&src, "machines.json", "[]");
        put(&src, "agent-token", "test-only-agent-token");
        put(&src, "chatgpt/host-id", "urn:uuid:test");
        put(&src, "chatgpt/account.json", "{}");
        put(
            &src,
            "gemini_web/accounts/accounts.json",
            r#"[{"id":"one","name":"Test","profile":".gemini_profiles\\\\existing"}]"#,
        );
        put(&src, "gemini_web/accounts/settings.json", "{}");
        put(
            &src,
            "gemini_web/accounts/one.dpapi",
            "not-even-a-valid-cookie",
        );
        put(
            &dst,
            "config.json",
            r#"{"providers":[{"name":"Local Gemini","gemini_web":true,"models":["keep-local-model"]}]}"#,
        );
        put(
            &dst,
            "gemini_web/accounts/accounts.json",
            "leave-local-web-alone",
        );
        put(&src, "usage.json", "{}");
        put(&src, "chatgpt/account.lock", "locked");
        put(&src, "gemini_web/profiles/one/secret", "browser-data");
        put(&dst, "profiles/Keep.json", r#"{"tiers":[{},{},{},{}]}"#);
        let mut backup = collect(&src).unwrap();
        assert_eq!(backup.files.len(), 6);
        assert!(backup
            .files
            .keys()
            .all(|name| !name.starts_with("gemini_web/")));
        assert!(!backup.files["config.json"].contains("must-not-export"));
        // Earlier backups may contain invalid browser paths or cookies from another Windows user.
        backup.files.insert(
            "gemini_web/accounts/accounts.json".into(),
            "invalid-web-json".into(),
        );
        backup.files.insert(
            "gemini_web/accounts/one.cookie".into(),
            "must-not-import".into(),
        );
        let text = serde_json::to_string(&backup).unwrap();
        assert_eq!(restore(&dst, parse(&text).unwrap()).unwrap(), 6);
        let restored: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dst.join("config.json")).unwrap()).unwrap();
        assert_eq!(restored["bypass"], true);
        assert_eq!(restored["future_preference"], 42);
        assert_eq!(
            restored["providers"][0]["accounts"][0]["refresh"],
            "test-refresh"
        );
        assert_eq!(restored["providers"][1]["chatgpt"], true);
        assert_eq!(restored["providers"][2]["key"], "test-ollama-key");
        assert_eq!(restored["providers"][3]["key"], "test-only-key");
        assert_eq!(restored["providers"][4]["name"], "Local Gemini");
        assert_eq!(restored["providers"][4]["models"][0], "keep-local-model");
        assert_eq!(
            fs::read_to_string(dst.join("gemini_web/accounts/accounts.json")).unwrap(),
            "leave-local-web-alone"
        );
        assert_eq!(
            fs::read_to_string(dst.join("chatgpt/account.json")).unwrap(),
            "{}"
        );
        assert!(dst.join("profiles/Keep.json").exists());
        assert!(!dst.join("usage.json").exists());
        assert!(!dst.join("gemini_web/profiles").exists());
        fs::remove_dir_all(src).unwrap();
        fs::remove_dir_all(dst).unwrap();
    }
    #[test]
    fn rejects_windows_device_aliases_before_opening_files() {
        for name in ["COM¹", "LPT²", "COM³", "CONIN$", "CONOUT$"] {
            assert!(!allowed(&format!("profiles/{name}.json")), "{name}");
        }
    }

    #[test]
    fn invalid_backups_do_not_change_existing_data() {
        let root = dir();
        put(&root, "config.json", "{}");
        for name in [
            "../escape.json",
            "profiles/../../escape.json",
            "C:/escape.json",
            "profiles/CON.json",
            "profiles/A.json:stream",
            "chatgpt/a.lock",
            "unknown/accounts.json",
            "chatgpt/NUL.json",
        ] {
            let mut files = BTreeMap::from([("config.json".into(), "{}".into())]);
            files.insert(name.into(), "{}".into());
            let b = Backup {
                format: "ccat-backup".into(),
                version: 1,
                files,
            };
            assert!(restore(&root, b).is_err(), "{name}");
            assert_eq!(fs::read_to_string(root.join("config.json")).unwrap(), "{}");
        }
        assert!(
            parse(r#"{"format":"ccat-backup","version":2,"files":{"config.json":"{}"}}"#).is_err()
        );
        assert!(
            parse(r#"{"format":"ccat-backup","version":1,"files":{"config.json":"{broken"}}"#)
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn restores_roll_back_on_filesystem_failure() {
        let root = dir();
        put(&root, "config.json", "{}");
        put(&root, "profiles", "not-a-directory");
        let b = Backup {
            format: "ccat-backup".into(),
            version: 1,
            files: BTreeMap::from([
                ("config.json".into(), r#"{"bypass":true}"#.into()),
                (
                    "profiles/Work.json".into(),
                    r#"{"tiers":[{},{},{},{}]}"#.into(),
                ),
            ]),
        };
        assert!(restore(&root, b).is_err());
        assert_eq!(fs::read_to_string(root.join("config.json")).unwrap(), "{}");
        fs::remove_dir_all(root).unwrap();
    }
}
