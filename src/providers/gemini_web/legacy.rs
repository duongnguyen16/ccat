//! Native Windows read-only importer for legacy Python runtime SQLite database (`logs/argon_runtime.sqlite3`).
//!
//! Dynamically binds to Windows System32 `winsqlite3.dll` via `LoadLibraryExW` + `GetProcAddress`
//! so no import library, Python runtime, or external SQLite crate is needed.

use super::store::{self, HistoryEntry};
use std::collections::HashSet;
use std::path::Path;

#[cfg(windows)]
extern "system" {
    fn LoadLibraryExW(
        lpLibFileName: *const u16,
        hFile: *mut std::ffi::c_void,
        dwFlags: u32,
    ) -> *mut std::ffi::c_void;
    fn GetProcAddress(
        hModule: *mut std::ffi::c_void,
        lpProcName: *const u8,
    ) -> *mut std::ffi::c_void;
    fn FreeLibrary(hLibModule: *mut std::ffi::c_void) -> i32;
}

#[cfg(windows)]
const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x00000800;
#[cfg(windows)]
const SQLITE_OK: i32 = 0;
#[cfg(windows)]
const SQLITE_ROW: i32 = 100;
#[cfg(windows)]
const SQLITE_DONE: i32 = 101;
#[cfg(windows)]
const SQLITE_OPEN_READONLY: i32 = 0x00000001;

#[cfg(windows)]
type FnSqlite3OpenV2 = unsafe extern "C" fn(*const u8, *mut *mut std::ffi::c_void, i32, *const u8) -> i32;
#[cfg(windows)]
type FnSqlite3PrepareV2 = unsafe extern "C" fn(*mut std::ffi::c_void, *const u8, i32, *mut *mut std::ffi::c_void, *mut *const u8) -> i32;
#[cfg(windows)]
type FnSqlite3Step = unsafe extern "C" fn(*mut std::ffi::c_void) -> i32;
#[cfg(windows)]
type FnSqlite3ColumnText = unsafe extern "C" fn(*mut std::ffi::c_void, i32) -> *const u8;
#[cfg(windows)]
type FnSqlite3ColumnBytes = unsafe extern "C" fn(*mut std::ffi::c_void, i32) -> i32;
#[cfg(windows)]
type FnSqlite3ColumnInt64 = unsafe extern "C" fn(*mut std::ffi::c_void, i32) -> i64;
#[cfg(windows)]
type FnSqlite3ColumnDouble = unsafe extern "C" fn(*mut std::ffi::c_void, i32) -> f64;
#[cfg(windows)]
type FnSqlite3Finalize = unsafe extern "C" fn(*mut std::ffi::c_void) -> i32;
#[cfg(windows)]
type FnSqlite3Close = unsafe extern "C" fn(*mut std::ffi::c_void) -> i32;
#[cfg(windows)]
type FnSqlite3Exec = unsafe extern "C" fn(*mut std::ffi::c_void, *const u8, *mut std::ffi::c_void, *mut std::ffi::c_void, *mut *mut u8) -> i32;

#[cfg(windows)]
struct WinSqlite3 {
    module: *mut std::ffi::c_void,
    open_v2: FnSqlite3OpenV2,
    prepare_v2: FnSqlite3PrepareV2,
    step: FnSqlite3Step,
    column_text: FnSqlite3ColumnText,
    column_bytes: FnSqlite3ColumnBytes,
    column_int64: FnSqlite3ColumnInt64,
    column_double: FnSqlite3ColumnDouble,
    finalize: FnSqlite3Finalize,
    close: FnSqlite3Close,
    #[allow(dead_code)]
    exec: FnSqlite3Exec,
}

#[cfg(windows)]
impl Drop for WinSqlite3 {
    fn drop(&mut self) {
        if !self.module.is_null() {
            unsafe { FreeLibrary(self.module); }
        }
    }
}

#[cfg(windows)]
impl WinSqlite3 {
    fn load() -> Result<Self, String> {
        let dll_name: Vec<u16> = "winsqlite3.dll\0".encode_utf16().collect();
        let module = unsafe {
            LoadLibraryExW(
                dll_name.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            return Err("Failed to load winsqlite3.dll from System32".into());
        }

        unsafe fn get_sym<T>(module: *mut std::ffi::c_void, name: &[u8]) -> Result<T, String> {
            let addr = GetProcAddress(module, name.as_ptr());
            if addr.is_null() {
                let s = String::from_utf8_lossy(name);
                return Err(format!("Symbol {s} not found in winsqlite3.dll"));
            }
            Ok(std::mem::transmute_copy(&addr))
        }

        unsafe {
            let open_v2: FnSqlite3OpenV2 = get_sym(module, b"sqlite3_open_v2\0")?;
            let prepare_v2: FnSqlite3PrepareV2 = get_sym(module, b"sqlite3_prepare_v2\0")?;
            let step: FnSqlite3Step = get_sym(module, b"sqlite3_step\0")?;
            let column_text: FnSqlite3ColumnText = get_sym(module, b"sqlite3_column_text\0")?;
            let column_bytes: FnSqlite3ColumnBytes = get_sym(module, b"sqlite3_column_bytes\0")?;
            let column_int64: FnSqlite3ColumnInt64 = get_sym(module, b"sqlite3_column_int64\0")?;
            let column_double: FnSqlite3ColumnDouble = get_sym(module, b"sqlite3_column_double\0")?;
            let finalize: FnSqlite3Finalize = get_sym(module, b"sqlite3_finalize\0")?;
            let close: FnSqlite3Close = get_sym(module, b"sqlite3_close\0")?;
            let exec: FnSqlite3Exec = get_sym(module, b"sqlite3_exec\0")?;

            Ok(Self {
                module,
                open_v2,
                prepare_v2,
                step,
                column_text,
                column_bytes,
                column_int64,
                column_double,
                finalize,
                close,
                exec,
            })
        }
    }
}

pub fn import_legacy_runtime(root: &Path) -> Result<(usize, usize), String> {
    let sqlite_path = root.join("logs").join("argon_runtime.sqlite3");
    if !sqlite_path.is_file() {
        return Ok((0, 0));
    }

    #[cfg(windows)]
    {
        import_legacy_windows(&sqlite_path, root)
    }

    #[cfg(not(windows))]
    {
        let _ = root;
        Ok((0, 0))
    }
}

#[cfg(windows)]
fn import_legacy_windows(db_path: &Path, root: &Path) -> Result<(usize, usize), String> {
    let sqlite = WinSqlite3::load()?;
    let path_str = db_path.to_string_lossy().to_string();
    let c_path = std::ffi::CString::new(path_str.as_str()).map_err(|e| e.to_string())?;

    let mut db: *mut std::ffi::c_void = std::ptr::null_mut();
    let rc = unsafe {
        (sqlite.open_v2)(
            c_path.as_ptr() as *const u8,
            &mut db,
            SQLITE_OPEN_READONLY,
            std::ptr::null(),
        )
    };
    if rc != SQLITE_OK || db.is_null() {
        return Err(format!("Failed to open legacy sqlite at {}: error code {rc}", db_path.display()));
    }

    struct DbGuard<'a>(&'a WinSqlite3, *mut std::ffi::c_void);
    impl<'a> Drop for DbGuard<'a> {
        fn drop(&mut self) {
            if !self.1.is_null() {
                unsafe { (self.0.close)(self.1); }
            }
        }
    }
    let _guard = DbGuard(&sqlite, db);

    // 1. Read newest request history (bounded to 2000 newest entries)
    let sql_history = b"SELECT request_id, started_at, ended_at, event_json FROM request_history ORDER BY ended_at DESC LIMIT 2000\0";
    let mut stmt: *mut std::ffi::c_void = std::ptr::null_mut();
    let rc = unsafe {
        (sqlite.prepare_v2)(
            db,
            sql_history.as_ptr(),
            sql_history.len() as i32,
            &mut stmt,
            std::ptr::null_mut(),
        )
    };
    if rc != SQLITE_OK || stmt.is_null() {
        return Err(format!("Failed to prepare history query: code {rc}"));
    }

    let mut imported_history = Vec::new();
    let mut total_metadata_bytes: usize = 0;
    const MAX_METADATA_BUDGET: usize = 16 * 1024 * 1024; // 16 MiB

    loop {
        let step_rc = unsafe { (sqlite.step)(stmt) };
        if step_rc == SQLITE_ROW {
            let req_id_len = unsafe { (sqlite.column_bytes)(stmt, 0) };
            if req_id_len <= 0 || req_id_len > 256 {
                continue;
            }
            let req_id_ptr = unsafe { (sqlite.column_text)(stmt, 0) };
            if req_id_ptr.is_null() {
                continue;
            }
            let req_id = {
                let slice = unsafe { std::slice::from_raw_parts(req_id_ptr, req_id_len as usize) };
                String::from_utf8_lossy(slice).to_string()
            };

            let started_at = unsafe { (sqlite.column_double)(stmt, 1) };
            let ended_at = unsafe { (sqlite.column_double)(stmt, 2) };
            if !started_at.is_finite() || !ended_at.is_finite() {
                continue;
            }

            let json_len = unsafe { (sqlite.column_bytes)(stmt, 3) };
            if json_len < 0 || json_len > 65536 {
                continue;
            }
            let json_ptr = unsafe { (sqlite.column_text)(stmt, 3) };
            let json_str = if !json_ptr.is_null() && json_len > 0 {
                let slice = unsafe { std::slice::from_raw_parts(json_ptr, json_len as usize) };
                String::from_utf8_lossy(slice).to_string()
            } else {
                String::new()
            };

            total_metadata_bytes += req_id_len as usize + json_len as usize;
            if total_metadata_bytes > MAX_METADATA_BUDGET {
                break;
            }

            let event: serde_json::Value = serde_json::from_str(&json_str).unwrap_or_default();
            let account_id = event.get("attempts")
                .and_then(|a| a.as_array())
                .and_then(|a| a.first())
                .and_then(|att| att.get("accountId"))
                .and_then(|v| v.as_str())
                .unwrap_or("legacy")
                .to_string();

            let requested_model = event.get("requestedModel")
                .or_else(|| event.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let actual_model = event.get("actualModel")
                .or_else(|| event.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let status = event.get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("success")
                .to_string();

            // Do not copy arbitrary legacy error/body strings into ordinary history
            let error = if event.get("error").is_some() || status == "error" {
                Some("Legacy request failed".to_string())
            } else {
                None
            };
            let latency_ms = (ended_at - started_at).max(0.0) * 1000.0;

            imported_history.push(HistoryEntry {
                id: req_id,
                account_id,
                requested_model,
                actual_model,
                started_at,
                ended_at,
                status,
                error,
                latency_ms,
            });
        } else if step_rc == SQLITE_DONE {
            break;
        } else {
            unsafe { (sqlite.finalize)(stmt); }
            return Err(format!("Error reading legacy request_history: step code {step_rc}"));
        }
    }
    unsafe { (sqlite.finalize)(stmt); }

    // 2. Read account usage counters
    let sql_usage = b"SELECT account_id, request_count, error_count FROM account_usage\0";
    let mut usage_map = std::collections::HashMap::new();
    let rc = unsafe {
        (sqlite.prepare_v2)(
            db,
            sql_usage.as_ptr(),
            sql_usage.len() as i32,
            &mut stmt,
            std::ptr::null_mut(),
        )
    };
    if rc != SQLITE_OK || stmt.is_null() {
        return Err(format!("Failed to prepare usage query: code {rc}"));
    }

    loop {
        let step_rc = unsafe { (sqlite.step)(stmt) };
        if step_rc == SQLITE_ROW {
            let id_len = unsafe { (sqlite.column_bytes)(stmt, 0) };
            if id_len > 0 && id_len <= 256 {
                let id_ptr = unsafe { (sqlite.column_text)(stmt, 0) };
                if !id_ptr.is_null() {
                    let slice = unsafe { std::slice::from_raw_parts(id_ptr, id_len as usize) };
                    let acct_id = String::from_utf8_lossy(slice).to_string();
                    let req_count = unsafe { (sqlite.column_int64)(stmt, 1) }.max(0) as u64;
                    let err_count = unsafe { (sqlite.column_int64)(stmt, 2) }.max(0) as u64;
                    usage_map.insert(acct_id, (req_count, err_count));
                }
            }
        } else if step_rc == SQLITE_DONE {
            break;
        } else {
            unsafe { (sqlite.finalize)(stmt); }
            return Err(format!("Error reading legacy account_usage: step code {step_rc}"));
        }
    }
    unsafe { (sqlite.finalize)(stmt); }

    // 3. Merge history under StoreLock (released before account mutation to prevent deadlock!)
    let new_history_count = {
        let lock_path = store::lock_file_in(root);
        let _lock = store::StoreLock::acquire(&lock_path, std::time::Duration::from_secs(10))?;

        let history_file = store::history_file_in(root);
        let mut existing_history = if history_file.is_file() {
            let text = store::read_bounded_string(&history_file)?;
            serde_json::from_str::<Vec<HistoryEntry>>(&text)
                .map_err(|e| format!("Corrupt history at {}: {e}", history_file.display()))?
        } else {
            Vec::new()
        };

        let existing_ids: HashSet<String> = existing_history.iter().map(|h| h.id.clone()).collect();
        let mut added = 0;
        for entry in imported_history {
            if !existing_ids.contains(&entry.id) {
                existing_history.push(entry);
                added += 1;
            }
        }

        if added > 0 {
            // Sort ascending by ended_at, retain newest 2000
            existing_history.sort_by(|a, b| a.ended_at.partial_cmp(&b.ended_at).unwrap_or(std::cmp::Ordering::Equal));
            if existing_history.len() > 2000 {
                let overflow = existing_history.len() - 2000;
                existing_history.drain(0..overflow);
            }
            let text = serde_json::to_string(&existing_history).map_err(|e| e.to_string())?;
            store::write_atomic_secure(&history_file, text.as_bytes())?;
        }
        added
    };

    // 4. Merge usage counters into accounts.json under separate mutate_accounts_in lock
    let mut updated_accounts_count = 0;
    if !usage_map.is_empty() {
        store::mutate_accounts_in(root, |accs| {
            for a in accs.iter_mut() {
                if let Some(&(reqs, errs)) = usage_map.get(&a.id) {
                    if reqs > a.request_count || errs > a.error_count {
                        a.request_count = a.request_count.max(reqs);
                        a.error_count = a.error_count.max(errs);
                        updated_accounts_count += 1;
                    }
                }
            }
            Ok(())
        })?;
    }

    Ok((new_history_count, updated_accounts_count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::gemini_web::store::Account;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn test_temp_root(prefix: &str) -> PathBuf {
        let temp = std::env::temp_dir().join(format!("ccat_test_legacy_{prefix}_{}", crate::common::random_hex(6)));
        let _ = std::fs::create_dir_all(&temp);
        temp
    }

    #[test]
    #[cfg(windows)]
    fn test_legacy_sqlite_import_idempotent() {
        let root = test_temp_root("sqlite_import");
        let logs_dir = root.join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let db_path = logs_dir.join("argon_runtime.sqlite3");

        // Create synthetic SQLite DB using winsqlite3.dll
        let sqlite = WinSqlite3::load().expect("load winsqlite3");
        let c_path = std::ffi::CString::new(db_path.to_str().unwrap()).unwrap();
        let mut db: *mut std::ffi::c_void = std::ptr::null_mut();
        // SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE = 0x06
        let rc = unsafe { (sqlite.open_v2)(c_path.as_ptr() as *const u8, &mut db, 0x06, std::ptr::null()) };
        assert_eq!(rc, 0);

        let ddl = b"CREATE TABLE request_history (request_id TEXT PRIMARY KEY, started_at REAL NOT NULL, ended_at REAL NOT NULL, event_json TEXT NOT NULL);\
                    CREATE TABLE account_usage (account_id TEXT PRIMARY KEY, request_count INTEGER NOT NULL DEFAULT 0, error_count INTEGER NOT NULL DEFAULT 0);\
                    INSERT INTO request_history VALUES ('req-legacy-1', 10.0, 12.5, '{\"requestedModel\":\"gemini-4-argon\",\"status\":\"success\",\"attempts\":[{\"accountId\":\"acc-leg-1\"}]}');\
                    INSERT INTO account_usage VALUES ('acc-leg-1', 42, 3);\0";

        let rc = unsafe { (sqlite.exec)(db, ddl.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(rc, 0);
        unsafe { (sqlite.close)(db); }

        // Setup matching account in storage with 0 counters
        let acc = Account {
            id: "acc-leg-1".into(),
            name: "Legacy Account".into(),
            port: 4982,
            enabled: true,
            profile: "profiles/acc-leg-1".into(),
            request_count: 0,
            error_count: 0,
            extra: BTreeMap::new(),
        };
        store::save_in(&root, &[acc]).unwrap();

        // 1. First import
        let (hist_count, acct_count) = import_legacy_runtime(&root).expect("import legacy");
        assert_eq!(hist_count, 1);
        assert_eq!(acct_count, 1);

        let accounts = store::list_in(&root).unwrap();
        assert_eq!(accounts[0].request_count, 42);
        assert_eq!(accounts[0].error_count, 3);

        let history = store::recent_history_in(&root, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, "req-legacy-1");
        assert_eq!(history[0].account_id, "acc-leg-1");
        assert_eq!(history[0].requested_model, "gemini-4-argon");

        // 2. Second import must be idempotent (0 new items)
        let (hist_count_2, acct_count_2) = import_legacy_runtime(&root).expect("import legacy again");
        assert_eq!(hist_count_2, 0);
        assert_eq!(acct_count_2, 0);

        // Database still exists
        assert!(db_path.is_file());

        let _ = std::fs::remove_dir_all(&root);
    }
}
