//! Storage for Gemini Web accounts, settings, DPAPI credentials, and runtime state.
//!
//! Handles:
//! - Multi-instance process-safe file locking using native Windows file locks.
//! - Atomic mutations with unknown-field retention.
//! - Path and ID validation (prevents directory traversal).
//! - DPAPI credential encryption.
//! - Cookie-free import/export.
//! - Safe history metadata (never stores prompts or secrets).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- Data Models

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Account {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub request_count: u64,
    #[serde(default)]
    pub error_count: u64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    #[serde(default = "default_true")]
    pub rotation_enabled: bool,
    #[serde(default)]
    pub preferred_account_id: Option<String>,
    #[serde(default = "default_strategy")]
    pub rotation_strategy: String,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: u32,
    #[serde(default)]
    pub max_requests_per_minute: u32,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn default_strategy() -> String {
    "round_robin".to_string()
}

fn default_max_concurrent() -> u32 {
    1
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            rotation_enabled: true,
            preferred_account_id: None,
            rotation_strategy: default_strategy(),
            max_concurrent_requests: 1,
            max_requests_per_minute: 0,
            extra: BTreeMap::new(),
        }
    }
}

/// Request history metadata. Never logs prompts, cookies, or secrets.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    pub id: String,
    pub account_id: String,
    #[serde(default)]
    pub requested_model: String,
    #[serde(default)]
    pub actual_model: String,
    pub started_at: f64,
    pub ended_at: f64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub latency_ms: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ExportData {
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default, rename = "preferredAccountName")]
    pub preferred_account_name: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub version: Option<u32>,
}

// ---------------------------------------------------------------- Validation

/// Validates an account ID: must be 1..=64 characters of [a-zA-Z0-9_-], no path traversal.
pub fn validate_id(id: &str) -> Result<(), String> {
    let s = id.trim();
    if s.is_empty() || s.len() > 64 {
        return Err("Account ID must be between 1 and 64 characters".into());
    }
    if s.contains('/') || s.contains('\\') || s.contains("..") {
        return Err("Account ID cannot contain slashes or path traversal elements".into());
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("Account ID can only contain alphanumeric characters, hyphens, and underscores".into());
    }
    Ok(())
}

/// Validates an account name: 1..=48 characters matching alphanumeric, space, dot, underscore, or dash.
pub fn validate_name(name: &str) -> Result<(), String> {
    let s = name.trim();
    if s.is_empty() || s.len() > 48 {
        return Err("Account name must be between 1 and 48 characters".into());
    }
    if !s.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '.' || c == '_' || c == '-') {
        return Err("Account name can only contain letters, numbers, spaces, dots, underscores, or hyphens".into());
    }
    Ok(())
}

/// Validates a browser profile path: must be a safe relative path, no `..` or absolute drive roots.
pub fn validate_profile_path(path: &str) -> Result<PathBuf, String> {
    let s = path.trim();
    if s.is_empty() {
        return Err("Profile path cannot be empty".into());
    }
    let p = Path::new(s);
    if p.is_absolute() {
        return Err("Profile path cannot be absolute".into());
    }
    for comp in p.components() {
        match comp {
            std::path::Component::Normal(n) => {
                let seg = n.to_string_lossy();
                if seg.contains(':') || seg.contains('/') || seg.contains('\\') {
                    return Err(format!("Invalid profile segment: {seg}"));
                }
            }
            std::path::Component::CurDir => {}
            _ => return Err(format!("Profile path contains forbidden component: {s}")),
        }
    }
    Ok(p.to_path_buf())
}

// ---------------------------------------------------------------- Process-Safe File Lock

/// A process-safe cross-process file lock. Releases automatically on drop or process crash.
pub struct StoreLock {
    #[cfg(windows)]
    file: std::fs::File,
}

impl StoreLock {
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| format!("Failed to open lockfile {}: {e}", path.display()))?;

        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::LockFile;

            let handle = file.as_raw_handle();
            let start = Instant::now();
            loop {
                // Lock 1 byte at offset 0
                let ok = unsafe { LockFile(handle as _, 0, 0, 1, 0) };
                if ok != 0 {
                    return Ok(Self { file });
                }
                if start.elapsed() >= timeout {
                    return Err(format!(
                        "Timed out waiting for store lock on {}",
                        path.display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        #[cfg(not(windows))]
        {
            let _ = (file, timeout);
            Ok(Self {})
        }
    }
}

#[cfg(windows)]
impl Drop for StoreLock {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::UnlockFile;
        let handle = self.file.as_raw_handle();
        unsafe {
            UnlockFile(handle as _, 0, 0, 1, 0);
        }
    }
}

// ---------------------------------------------------------------- Filesystem Helpers

pub fn accounts_dir_in(root: &Path) -> PathBuf {
    root.join("accounts")
}

pub fn accounts_file_in(root: &Path) -> PathBuf {
    accounts_dir_in(root).join("accounts.json")
}

pub fn settings_file_in(root: &Path) -> PathBuf {
    accounts_dir_in(root).join("settings.json")
}

pub fn lock_file_in(root: &Path) -> PathBuf {
    accounts_dir_in(root).join(".store.lock")
}

pub const MAX_STORE_FILE_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB

pub(crate) fn read_bounded_string(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("Failed to read metadata for {}: {e}", path.display()))?;
    if meta.len() > MAX_STORE_FILE_BYTES {
        return Err(format!("File {} exceeds size limit of {} bytes", path.display(), MAX_STORE_FILE_BYTES));
    }
    std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))
}

pub(crate) fn read_bounded_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("Failed to read metadata for {}: {e}", path.display()))?;
    if meta.len() > MAX_STORE_FILE_BYTES {
        return Err(format!("File {} exceeds size limit of {} bytes", path.display(), MAX_STORE_FILE_BYTES));
    }
    std::fs::read(path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))
}

pub fn history_file_in(root: &Path) -> PathBuf {
    root.join("logs").join("history.json")
}

pub fn profiles_dir_in(root: &Path) -> PathBuf {
    root.join("profiles")
}

pub(crate) fn write_atomic_secure(target: &Path, content: &[u8]) -> Result<(), String> {
    use std::io::Write;
    if let Some(parent) = target.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            let _ = crate::common::secure_directory(parent);
        }
    }
    let rand = crate::common::random_hex(6);
    let tmp = target.with_extension(format!("tmp.{}.{}", std::process::id(), rand));

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| format!("Failed to create temp file {}: {e}", tmp.display()))?;

    // Secure permissions before writing sensitive content
    crate::common::secure_file(&tmp).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Failed to secure permissions for {}: {e}", tmp.display())
    })?;

    file.write_all(content).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Failed to write content to {}: {e}", tmp.display())
    })?;

    file.sync_all().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Failed to sync file {}: {e}", tmp.display())
    })?;

    drop(file);

    // Atomic replacement without destructive removal of existing target on failure
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("Failed to rename {} to {}: {e}", tmp.display(), target.display()));
    }

    crate::common::secure_file(target).map_err(|e| {
        format!("Failed to secure permissions for {}: {e}", target.display())
    })?;

    Ok(())
}

// ---------------------------------------------------------------- Accounts API

/// Reads accounts from the given root directory.
/// Returns Ok(empty) if the file doesn't exist.
/// Returns Err if the file exists but has malformed JSON (never overwrites corrupt data!).
pub fn list_in(root: &Path) -> Result<Vec<Account>, String> {
    let p = accounts_file_in(root);
    if !p.is_file() {
        return Ok(Vec::new());
    }
    let text = read_bounded_string(&p)
        .map_err(|e| format!("Failed to read accounts file {}: {e}", p.display()))?;
    serde_json::from_str(&text)
        .map_err(|e| format!("Malformed accounts file at {}: {e}", p.display()))
}

/// Atomically saves accounts into the given root directory.
pub fn save_in(root: &Path, accounts: &[Account]) -> Result<(), String> {
    for a in accounts {
        validate_id(&a.id)?;
        validate_name(&a.name)?;
        if !a.profile.is_empty() {
            validate_profile_path(&a.profile)?;
        }
    }
    let lock_path = lock_file_in(root);
    let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(10))?;

    let p = accounts_file_in(root);
    let text = serde_json::to_string_pretty(accounts).map_err(|e| e.to_string())?;
    write_atomic_secure(&p, text.as_bytes())
}

/// Atomically mutates the accounts registry under a process-safe lock.
pub fn mutate_accounts_in<F, R>(root: &Path, f: F) -> Result<R, String>
where
    F: FnOnce(&mut Vec<Account>) -> Result<R, String>,
{
    let lock_path = lock_file_in(root);
    let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(10))?;

    let mut accounts = list_in(root)?;
    let result = f(&mut accounts)?;
    let p = accounts_file_in(root);
    let text = serde_json::to_string_pretty(&accounts).map_err(|e| e.to_string())?;
    write_atomic_secure(&p, text.as_bytes())?;
    Ok(result)
}

fn cookie_unlocked(root: &Path, id: &str) -> Result<String, String> {
    validate_id(id)?;
    let dpapi_path = accounts_dir_in(root).join(format!("{id}.dpapi"));
    if !dpapi_path.is_file() {
        return Err(format!("Credential file not found for account {id}"));
    }
    let bytes = read_bounded_bytes(&dpapi_path)
        .map_err(|e| format!("Failed to read credential {}: {e}", dpapi_path.display()))?;
    let decrypted = crate::providers::gemini_web::dpapi_unprotect(&bytes)?;
    String::from_utf8(decrypted).map_err(|e| format!("Decrypted cookie is not valid UTF-8: {e}"))
}

fn save_cookie_unlocked(root: &Path, id: &str, value: &str) -> Result<(), String> {
    validate_id(id)?;
    let encrypted = crate::providers::gemini_web::dpapi_protect(value.as_bytes())?;
    let dpapi_path = accounts_dir_in(root).join(format!("{id}.dpapi"));
    write_atomic_secure(&dpapi_path, &encrypted)
}

/// Reads and decrypts DPAPI cookie for an account.
pub fn cookie_in(root: &Path, id: &str) -> Result<String, String> {
    cookie_unlocked(root, id)
}

/// Encrypts and writes DPAPI cookie for an account under store lock.
pub fn save_cookie_in(root: &Path, id: &str, value: &str) -> Result<(), String> {
    validate_id(id)?;
    let lock_path = lock_file_in(root);
    let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(10))?;
    save_cookie_unlocked(root, id, value)
}

/// Commits login credentials and relative profile under a single StoreLock.
/// Verifies the account still exists in storage (preventing orphan/resurrected writes after deletion).
pub fn commit_login_in(
    root: &Path,
    id: &str,
    cookie: &str,
    relative_profile: &str,
) -> Result<(), String> {
    validate_id(id)?;
    if !relative_profile.is_empty() {
        validate_profile_path(relative_profile)?;
    }

    let lock_path = lock_file_in(root);
    let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(10))?;

    let mut accounts = list_in(root)?;
    let acc = accounts
        .iter_mut()
        .find(|a| a.id == id)
        .ok_or_else(|| format!("Account {id} was deleted during login"))?;

    if !relative_profile.is_empty() {
        acc.profile = relative_profile.to_string();
    }

    let p_acc = accounts_file_in(root);
    let text_acc = serde_json::to_string_pretty(&accounts).map_err(|e| e.to_string())?;
    write_atomic_secure(&p_acc, text_acc.as_bytes())?;

    save_cookie_unlocked(root, id, cookie)?;
    Ok(())
}

/// Atomically updates an account's cookie using Compare-And-Swap (CAS).
/// Under lock: checks account still exists, verifies current cookie matches `expected`.
/// Returns Ok(true) if updated, Ok(false) if account missing or cookie changed (never overwrites newer re-login).
pub fn save_cookie_if_current_in(
    root: &Path,
    id: &str,
    expected: &str,
    updated: &str,
) -> Result<bool, String> {
    validate_id(id)?;
    let lock_path = lock_file_in(root);
    let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(10))?;

    let accounts = list_in(root)?;
    if !accounts.iter().any(|a| a.id == id) {
        return Ok(false);
    }

    let current = match cookie_unlocked(root, id) {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };

    if current != expected {
        return Ok(false);
    }

    save_cookie_unlocked(root, id, updated)?;
    Ok(true)
}

/// Deletes an account and its credentials under a single StoreLock.
pub fn delete_account_in(root: &Path, id: &str) -> Result<(), String> {
    validate_id(id)?;
    let lock_path = lock_file_in(root);
    let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(10))?;

    let mut accounts = list_in(root)?;
    accounts.retain(|a| a.id != id);

    let p = accounts_file_in(root);
    let text = serde_json::to_string_pretty(&accounts).map_err(|e| e.to_string())?;
    write_atomic_secure(&p, text.as_bytes())?;

    let dpapi_path = accounts_dir_in(root).join(format!("{id}.dpapi"));
    if dpapi_path.is_file() {
        let _ = std::fs::remove_file(&dpapi_path);
    }
    Ok(())
}

// ---------------------------------------------------------------- Settings API

pub fn settings_in(root: &Path) -> Result<Settings, String> {
    let p = settings_file_in(root);
    if !p.is_file() {
        return Ok(Settings::default());
    }
    let text = read_bounded_string(&p)
        .map_err(|e| format!("Failed to read settings file {}: {e}", p.display()))?;
    serde_json::from_str(&text)
        .map_err(|e| format!("Malformed settings file at {}: {e}", p.display()))
}

pub fn save_settings_in(root: &Path, settings: &Settings) -> Result<(), String> {
    let lock_path = lock_file_in(root);
    let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(10))?;

    let p = settings_file_in(root);
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    write_atomic_secure(&p, text.as_bytes())
}

// ---------------------------------------------------------------- History / Runtime State

pub fn record_history_in(root: &Path, entry: &HistoryEntry) -> Result<(), String> {
    let lock_path = lock_file_in(root);
    let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(10))?;

    let file = history_file_in(root);
    let mut history: Vec<HistoryEntry> = if file.is_file() {
        let text = read_bounded_string(&file)
            .map_err(|e| format!("Failed to read history file {}: {e}", file.display()))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("Corrupt history file at {}: {e}", file.display()))?
    } else {
        Vec::new()
    };

    history.push(entry.clone());
    // Keep at most 2000 entries
    if history.len() > 2000 {
        let remove_count = history.len() - 2000;
        history.drain(0..remove_count);
    }

    let text = serde_json::to_string(&history).map_err(|e| e.to_string())?;
    write_atomic_secure(&file, text.as_bytes())
}

pub fn recent_history_in(root: &Path, limit: usize) -> Result<Vec<HistoryEntry>, String> {
    let file = history_file_in(root);
    if !file.is_file() {
        return Ok(Vec::new());
    }
    let text = read_bounded_string(&file)
        .map_err(|e| format!("Failed to read history {}: {e}", file.display()))?;
    let history: Vec<HistoryEntry> = serde_json::from_str(&text)
        .map_err(|e| format!("Malformed history {}: {e}", file.display()))?;
    let start = history.len().saturating_sub(limit);
    let mut recent = history[start..].to_vec();
    recent.reverse();
    Ok(recent)
}

/// Checks if an older argon_runtime.sqlite3 file exists and preserves it untouched.
pub fn check_legacy_runtime_sqlite(root: &Path) -> Option<String> {
    let sqlite_path = root.join("logs").join("argon_runtime.sqlite3");
    if sqlite_path.is_file() {
        Some(format!(
            "Legacy argon_runtime.sqlite3 preserved at {}; native history stored in JSON",
            sqlite_path.display()
        ))
    } else {
        None
    }
}

// ---------------------------------------------------------------- Cookie-free Import / Export

/// Exports accounts and settings without sensitive credentials/cookies.
/// Whitelists only known safe fields for transfer; never exports extra/nested unknown fields.
pub fn export_config_in(root: &Path) -> Result<ExportData, String> {
    let accounts = list_in(root)?;
    let settings = settings_in(root)?;

    let preferred_account_name = accounts
        .iter()
        .find(|a| Some(&a.id) == settings.preferred_account_id.as_ref())
        .map(|a| a.name.clone());

    // Explicit whitelist: drop all extra / unknown metadata from transfer payload
    let sanitized_accounts: Vec<Account> = accounts
        .into_iter()
        .map(|a| Account {
            id: a.id,
            name: a.name,
            port: a.port,
            enabled: a.enabled,
            profile: a.profile,
            request_count: a.request_count,
            error_count: a.error_count,
            extra: BTreeMap::new(),
        })
        .collect();

    let sanitized_settings = Settings {
        rotation_enabled: settings.rotation_enabled,
        preferred_account_id: settings.preferred_account_id,
        rotation_strategy: settings.rotation_strategy,
        max_concurrent_requests: settings.max_concurrent_requests,
        max_requests_per_minute: settings.max_requests_per_minute,
        extra: BTreeMap::new(),
    };

    Ok(ExportData {
        accounts: sanitized_accounts,
        settings: sanitized_settings,
        preferred_account_name,
        format: Some("argon-local-config".into()),
        version: Some(1),
    })
}

/// Imports accounts and settings safely:
/// - Validates all inputs before any changes (no partial imports).
/// - Merges by stable ID / name (NEVER deletes or replaces existing accounts).
/// - Preserves existing unknown metadata in storage.
/// - Accepts original Argon name-based export schema and preferredAccountName.
pub fn import_config_in(root: &Path, data: &ExportData) -> Result<(), String> {
    // 1. Validation pass: validate all incoming settings and accounts BEFORE writing anything
    if data.settings.max_concurrent_requests < 1 || data.settings.max_concurrent_requests > 20 {
        return Err("max_concurrent_requests must be between 1 and 20".into());
    }
    if data.settings.max_requests_per_minute > 120 {
        return Err("max_requests_per_minute must be between 0 and 120".into());
    }
    for a in &data.accounts {
        validate_name(&a.name)?;
        if !a.id.is_empty() {
            validate_id(&a.id)?;
        }
        if !a.profile.is_empty() {
            validate_profile_path(&a.profile)?;
        }
    }

    // 2. Lock and merge under lock
    let lock_path = lock_file_in(root);
    let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(10))?;

    let mut current_accounts = list_in(root)?;
    let mut current_settings = settings_in(root)?;

    let mut reserved_ports: std::collections::HashSet<u16> = current_accounts.iter().map(|a| a.port).collect();
    reserved_ports.insert(4981);
    reserved_ports.insert(4988);

    for incoming in &data.accounts {
        // Match existing by ID first, then by case-insensitive name
        let existing = current_accounts.iter_mut().find(|a| {
            (!incoming.id.is_empty() && a.id == incoming.id)
                || a.name.eq_ignore_ascii_case(&incoming.name)
        });

        if let Some(existing_acc) = existing {
            // Update known fields, keep existing ID, counters, and storage extra metadata!
            existing_acc.enabled = incoming.enabled;
            if !incoming.profile.is_empty() {
                existing_acc.profile = incoming.profile.clone();
            }
            if incoming.port != 0 && existing_acc.port == 0 {
                existing_acc.port = incoming.port;
            }
        } else {
            // Add new account
            let id = if !incoming.id.is_empty() {
                incoming.id.clone()
            } else {
                crate::common::random_hex(16)
            };
            let port = if incoming.port != 0 && !reserved_ports.contains(&incoming.port) {
                incoming.port
            } else {
                let next_port = (4982..65535).find(|p| !reserved_ports.contains(p)).unwrap_or(4982);
                reserved_ports.insert(next_port);
                next_port
            };
            let profile = if !incoming.profile.is_empty() {
                incoming.profile.clone()
            } else {
                format!("profiles/{id}")
            };

            current_accounts.push(Account {
                id,
                name: incoming.name.clone(),
                port,
                enabled: incoming.enabled,
                profile,
                request_count: 0,
                error_count: 0,
                extra: BTreeMap::new(),
            });
        }
    }

    // Merge settings
    current_settings.rotation_enabled = data.settings.rotation_enabled;
    if data.settings.rotation_strategy == "least_used" || data.settings.rotation_strategy == "round_robin" {
        current_settings.rotation_strategy = data.settings.rotation_strategy.clone();
    }
    current_settings.max_concurrent_requests = data.settings.max_concurrent_requests;
    current_settings.max_requests_per_minute = data.settings.max_requests_per_minute;

    if let Some(ref pref_name) = data.preferred_account_name {
        if let Some(matched) = current_accounts.iter().find(|a| a.name.eq_ignore_ascii_case(pref_name)) {
            current_settings.preferred_account_id = Some(matched.id.clone());
        }
    } else if let Some(ref pref_id) = data.settings.preferred_account_id {
        if current_accounts.iter().any(|a| &a.id == pref_id) {
            current_settings.preferred_account_id = Some(pref_id.clone());
        }
    }

    let p_acc = accounts_file_in(root);
    let text_acc = serde_json::to_string_pretty(&current_accounts).map_err(|e| e.to_string())?;
    write_atomic_secure(&p_acc, text_acc.as_bytes())?;

    let p_set = settings_file_in(root);
    let text_set = serde_json::to_string_pretty(&current_settings).map_err(|e| e.to_string())?;
    write_atomic_secure(&p_set, text_set.as_bytes())?;

    Ok(())
}

// ---------------------------------------------------------------- Default Root Wrappers

pub fn list() -> Result<Vec<Account>, String> {
    list_in(&crate::providers::gemini_web::dir())
}

pub fn save(accounts: &[Account]) -> Result<(), String> {
    save_in(&crate::providers::gemini_web::dir(), accounts)
}

pub fn cookie(id: &str) -> Result<String, String> {
    cookie_in(&crate::providers::gemini_web::dir(), id)
}

pub fn save_cookie(id: &str, value: &str) -> Result<(), String> {
    save_cookie_in(&crate::providers::gemini_web::dir(), id, value)
}

pub fn delete_account(id: &str) -> Result<(), String> {
    delete_account_in(&crate::providers::gemini_web::dir(), id)
}

pub fn settings() -> Result<Settings, String> {
    settings_in(&crate::providers::gemini_web::dir())
}

pub fn save_settings(s: &Settings) -> Result<(), String> {
    save_settings_in(&crate::providers::gemini_web::dir(), s)
}

pub fn export_config() -> Result<ExportData, String> {
    export_config_in(&crate::providers::gemini_web::dir())
}

pub fn import_config(data: &ExportData) -> Result<(), String> {
    import_config_in(&crate::providers::gemini_web::dir(), data)
}

pub fn commit_login(id: &str, cookie: &str, relative_profile: &str) -> Result<(), String> {
    commit_login_in(&crate::providers::gemini_web::dir(), id, cookie, relative_profile)
}

pub fn save_cookie_if_current(id: &str, expected: &str, updated: &str) -> Result<bool, String> {
    save_cookie_if_current_in(&crate::providers::gemini_web::dir(), id, expected, updated)
}

// ---------------------------------------------------------------- Unit Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn test_temp_root(prefix: &str) -> PathBuf {
        let temp = std::env::temp_dir().join(format!("ccat_test_gemini_{prefix}_{}", crate::common::random_hex(6)));
        let _ = std::fs::create_dir_all(&temp);
        temp
    }

    #[test]
    fn corrupt_history_is_not_overwritten() {
        let root = test_temp_root("bad_history");
        std::fs::create_dir_all(history_file_in(&root).parent().unwrap()).unwrap();
        std::fs::write(history_file_in(&root), "corrupt-but-must-survive").unwrap();
        let entry = HistoryEntry { id:"r".into(), account_id:"a".into(), requested_model:String::new(), actual_model:String::new(),
            started_at:1.0, ended_at:2.0, status:"success".into(), error:None, latency_ms:1000.0 };
        assert!(record_history_in(&root, &entry).is_err());
        assert_eq!(std::fs::read_to_string(history_file_in(&root)).unwrap(), "corrupt-but-must-survive");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transfer_never_exports_unknown_nested_credentials() {
        let root = test_temp_root("safe_transfer");
        let account: Account = serde_json::from_value(serde_json::json!({"id":"a","name":"A","port":4982,
            "extension":{"headers":{"Authorization":"never-export"}}})).unwrap();
        save_in(&root, &[account]).unwrap();
        let mut settings = Settings::default();
        settings.extra.insert("extension".into(), serde_json::json!({"Cookie":"never-export"}));
        save_settings_in(&root, &settings).unwrap();
        assert!(!serde_json::to_string(&export_config_in(&root).unwrap()).unwrap().contains("never-export"));
        assert!(list_in(&root).unwrap()[0].extra.contains_key("extension"));
        assert!(settings_in(&root).unwrap().extra.contains_key("extension"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn test_account_serde_and_unknown_fields() {
        let json_str = r#"{
            "id": "acc-123",
            "name": "Work Account",
            "port": 4983,
            "custom_metadata_tag": "keep-me",
            "another_field": 42
        }"#;
        let acc: Account = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(acc.id, "acc-123");
        assert_eq!(acc.name, "Work Account");
        assert_eq!(acc.port, 4983);
        assert!(acc.enabled);
        assert_eq!(acc.request_count, 0);
        assert_eq!(acc.error_count, 0);
        assert_eq!(acc.extra.get("custom_metadata_tag").and_then(|v| v.as_str()), Some("keep-me"));
        assert_eq!(acc.extra.get("another_field").and_then(|v| v.as_i64()), Some(42));

        let reserialized = serde_json::to_string(&acc).expect("serialize");
        assert!(reserialized.contains("custom_metadata_tag"));
        assert!(reserialized.contains("keep-me"));
    }

    #[test]
    fn test_settings_serde_and_defaults() {
        let empty_json = "{}";
        let s: Settings = serde_json::from_str(empty_json).expect("deserialize empty");
        assert!(s.rotation_enabled);
        assert_eq!(s.preferred_account_id, None);
        assert_eq!(s.rotation_strategy, "round_robin");
        assert_eq!(s.max_concurrent_requests, 1);
        assert_eq!(s.max_requests_per_minute, 0);

        let custom_json = r#"{
            "rotation_strategy": "least_used",
            "max_requests_per_minute": 15,
            "extra_key": "extra_val"
        }"#;
        let s2: Settings = serde_json::from_str(custom_json).expect("deserialize custom");
        assert_eq!(s2.rotation_strategy, "least_used");
        assert_eq!(s2.max_requests_per_minute, 15);
        assert_eq!(s2.extra.get("extra_key").and_then(|v| v.as_str()), Some("extra_val"));
    }

    #[test]
    fn test_id_validation() {
        assert!(validate_id("valid-id_123").is_ok());
        assert!(validate_id("acc_1").is_ok());
        assert!(validate_id("../bad-id").is_err());
        assert!(validate_id("bad/id").is_err());
        assert!(validate_id("bad\\id").is_err());
        assert!(validate_id("").is_err());
        assert!(validate_id("id with spaces").is_err());
    }

    #[test]
    fn test_name_validation() {
        assert!(validate_name("Personal Account").is_ok());
        assert!(validate_name("work.email_2-test").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("a".repeat(49).as_str()).is_err());
        assert!(validate_name("bad<name>").is_err());
    }

    #[test]
    fn test_profile_path_validation() {
        assert!(validate_profile_path("profiles/acc1").is_ok());
        assert!(validate_profile_path(".gemini_profiles/key1").is_ok());
        assert!(validate_profile_path("my_profile").is_ok());
        assert!(validate_profile_path("../escape").is_err());
        assert!(validate_profile_path("C:\\Windows").is_err());
        assert!(validate_profile_path("/root").is_err());
    }

    #[test]
    fn test_list_save_and_corrupt_registry() {
        let root = test_temp_root("registry");
        // Empty registry when missing
        let initial = list_in(&root).expect("initial empty");
        assert!(initial.is_empty());

        // Save accounts
        let acc = Account {
            id: "test1".into(),
            name: "Test Account".into(),
            port: 4982,
            enabled: true,
            profile: "profiles/test1".into(),
            request_count: 5,
            error_count: 1,
            extra: BTreeMap::new(),
        };
        save_in(&root, &[acc.clone()]).expect("save");

        let loaded = list_in(&root).expect("reload");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], acc);

        // Corrupt registry must return Err, never empty defaults!
        let acc_file = accounts_file_in(&root);
        std::fs::write(&acc_file, b"corrupted { json !!").expect("corrupt");
        assert!(list_in(&root).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_mutate_accounts_in() {
        let root = test_temp_root("mutate");
        let acc = Account {
            id: "m1".into(),
            name: "Mutate Account".into(),
            port: 4982,
            enabled: true,
            profile: "".into(),
            request_count: 0,
            error_count: 0,
            extra: BTreeMap::new(),
        };
        save_in(&root, &[acc]).expect("save");

        mutate_accounts_in(&root, |accs| {
            accs[0].request_count += 10;
            Ok(())
        }).expect("mutate");

        let loaded = list_in(&root).expect("list");
        assert_eq!(loaded[0].request_count, 10);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_settings_save_and_load() {
        let root = test_temp_root("settings");
        let mut s = settings_in(&root).expect("default");
        assert_eq!(s.max_concurrent_requests, 1);

        s.max_concurrent_requests = 4;
        s.rotation_strategy = "least_used".into();
        save_settings_in(&root, &s).expect("save");

        let loaded = settings_in(&root).expect("load");
        assert_eq!(loaded.max_concurrent_requests, 4);
        assert_eq!(loaded.rotation_strategy, "least_used");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_cookie_dpapi_roundtrip() {
        let root = test_temp_root("cookie");
        let id = "test_cookie_account";
        let cookie_val = "__Secure-1PSID=val1; __Secure-1PSIDTS=ts1";

        save_cookie_in(&root, id, cookie_val).expect("save cookie");
        let loaded = cookie_in(&root, id).expect("load cookie");
        assert_eq!(loaded, cookie_val);

        delete_account_in(&root, id).expect("delete account");
        assert!(cookie_in(&root, id).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_history_recording_and_retrieval() {
        let root = test_temp_root("history");
        let entry = HistoryEntry {
            id: "req-1".into(),
            account_id: "acc-1".into(),
            requested_model: "gemini-3.1-pro".into(),
            actual_model: "gemini-3.1-pro".into(),
            started_at: 100.0,
            ended_at: 102.5,
            status: "success".into(),
            error: None,
            latency_ms: 2500.0,
        };
        record_history_in(&root, &entry).expect("record");

        let recent = recent_history_in(&root, 10).expect("recent");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0], entry);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_export_and_import_config_cookie_free() {
        let root = test_temp_root("import_export");
        let mut extra = BTreeMap::new();
        extra.insert("custom_tag".into(), serde_json::json!("tag1"));
        extra.insert("my_cookie_leak".into(), serde_json::json!("secret_cookie"));

        let acc = Account {
            id: "exp1".into(),
            name: "Export Acc".into(),
            port: 4985,
            enabled: true,
            profile: "profiles/exp1".into(),
            request_count: 1,
            error_count: 0,
            extra,
        };
        save_in(&root, &[acc]).expect("save");

        let export_data = export_config_in(&root).expect("export");
        assert_eq!(export_data.accounts.len(), 1);
        // Explicit whitelist for transfer: all extra/unknown fields dropped from export
        assert!(export_data.accounts[0].extra.is_empty());
        // Storage retains unknown metadata
        assert!(list_in(&root).unwrap()[0].extra.contains_key("custom_tag"));

        let root2 = test_temp_root("import_target");
        import_config_in(&root2, &export_data).expect("import");
        let imported = list_in(&root2).expect("list imported");
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].name, "Export Acc");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root2);
    }

    #[test]
    fn test_commit_login_atomicity() {
        let root = test_temp_root("commit_login");
        let acc = Account {
            id: "c1".into(),
            name: "Commit Acc".into(),
            port: 4982,
            enabled: true,
            profile: "".into(),
            request_count: 0,
            error_count: 0,
            extra: BTreeMap::new(),
        };
        save_in(&root, &[acc]).unwrap();

        // Commit login updates profile and stores cookie atomically
        commit_login_in(&root, "c1", "cookie_val_1", "profiles/c1").unwrap();
        assert_eq!(cookie_in(&root, "c1").unwrap(), "cookie_val_1");
        assert_eq!(list_in(&root).unwrap()[0].profile, "profiles/c1");

        // Delete account
        delete_account_in(&root, "c1").unwrap();
        assert!(cookie_in(&root, "c1").is_err());

        // Attempting to commit login for deleted account must fail and not create orphan credentials
        let res = commit_login_in(&root, "c1", "cookie_val_orphan", "profiles/c1");
        assert!(res.is_err());
        assert!(cookie_in(&root, "c1").is_err());
        let orphan_file = accounts_dir_in(&root).join("c1.dpapi");
        assert!(!orphan_file.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_save_cookie_cas() {
        let root = test_temp_root("cookie_cas");
        let acc = Account {
            id: "cas1".into(),
            name: "CAS Acc".into(),
            port: 4982,
            enabled: true,
            profile: "".into(),
            request_count: 0,
            error_count: 0,
            extra: BTreeMap::new(),
        };
        save_in(&root, &[acc]).unwrap();
        save_cookie_in(&root, "cas1", "initial_token").unwrap();

        // 1. CAS succeeds when current cookie matches expected
        let ok = save_cookie_if_current_in(&root, "cas1", "initial_token", "updated_token_v2").unwrap();
        assert!(ok);
        assert_eq!(cookie_in(&root, "cas1").unwrap(), "updated_token_v2");

        // 2. CAS fails when current cookie does not match stale expected (never overwrites newer re-login)
        let failed = save_cookie_if_current_in(&root, "cas1", "initial_token", "stale_overwrite").unwrap();
        assert!(!failed);
        assert_eq!(cookie_in(&root, "cas1").unwrap(), "updated_token_v2");

        // 3. CAS fails when account is deleted
        delete_account_in(&root, "cas1").unwrap();
        let deleted_cas = save_cookie_if_current_in(&root, "cas1", "updated_token_v2", "recreate_attempt").unwrap();
        assert!(!deleted_cas);
        assert!(cookie_in(&root, "cas1").is_err());

        let _ = std::fs::remove_dir_all(&root);
    }
}
