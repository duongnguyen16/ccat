//! Gemini Web provider: native Google Gemini Web session manager and bridge.
//! Fully migrated into ccat: stores accounts and DPAPI-encrypted credentials in `%APPDATA%\ccat\gemini_web`,
//! and manages the local `gemini-api` bridge speaking the Anthropic Messages API (`/claude/v1`).

pub mod browser;
pub(crate) mod legacy;
pub(crate) mod media;
pub mod messages;
pub(crate) mod pool;
pub mod store;
pub(crate) mod wire;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 4982;
pub const DEFAULT_URL: &str = "http://127.0.0.1:4982";

/// Default models provided by Gemini Web.
pub fn default_models() -> Vec<String> {
    vec![
        "gemini-4-argon".into(),
        "gemini-3.1-pro".into(),
        "gemini-3.8-flash".into(),
        "gemini-3.5-flash-lite".into(),
    ]
}

/// Root directory for Gemini Web data: `%APPDATA%\ccat\gemini_web`.
pub fn dir() -> PathBuf {
    crate::config_path().with_file_name("gemini_web")
}

pub fn bin_dir() -> PathBuf {
    dir().join("bin")
}

pub fn accounts_dir() -> PathBuf {
    dir().join("accounts")
}

pub fn profiles_dir() -> PathBuf {
    dir().join("profiles")
}

pub fn accounts_file() -> PathBuf {
    accounts_dir().join("accounts.json")
}

/// Ensures the gemini_web directory structure exists for native storage.
pub fn ensure_installed() {
    let _ = std::fs::create_dir_all(accounts_dir());
    let _ = std::fs::create_dir_all(profiles_dir());
}

// ---------------------------------------------------------------- DPAPI (Windows)

#[cfg(windows)]
#[repr(C)]
struct DataBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

#[cfg(windows)]
#[link(name = "crypt32")]
extern "system" {
    fn CryptUnprotectData(
        p_data_in: *const DataBlob,
        ppsz_data_descr: *mut *mut u16,
        p_optional_entropy: *const DataBlob,
        pv_reserved: *mut std::ffi::c_void,
        p_prompt_struct: *mut std::ffi::c_void,
        dw_flags: u32,
        p_data_out: *mut DataBlob,
    ) -> i32;

    #[allow(dead_code)]
    fn CryptProtectData(
        p_data_in: *const DataBlob,
        sz_data_descr: *const u16,
        p_optional_entropy: *const DataBlob,
        pv_reserved: *mut std::ffi::c_void,
        p_prompt_struct: *mut std::ffi::c_void,
        dw_flags: u32,
        p_data_out: *mut DataBlob,
    ) -> i32;
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(h_mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

#[cfg(windows)]
pub fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let in_blob = DataBlob {
        cb_data: data.len() as u32,
        pb_data: data.as_ptr() as *mut u8,
    };
    let mut out_blob = DataBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &in_blob,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0x1, // CRYPTPROTECT_UI_FORBIDDEN
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err(format!("DPAPI unprotect failed (code: {})", std::io::Error::last_os_error()));
    }
    let res = unsafe {
        let slice = std::slice::from_raw_parts(out_blob.pb_data, out_blob.cb_data as usize);
        let vec = slice.to_vec();
        LocalFree(out_blob.pb_data as *mut std::ffi::c_void);
        vec
    };
    Ok(res)
}

#[allow(dead_code)]
#[cfg(windows)]
pub fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let in_blob = DataBlob {
        cb_data: data.len() as u32,
        pb_data: data.as_ptr() as *mut u8,
    };
    let mut out_blob = DataBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let descr: Vec<u16> = "Argon Gemini account".encode_utf16().chain(Some(0)).collect();
    let ok = unsafe {
        CryptProtectData(
            &in_blob,
            descr.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0x1, // CRYPTPROTECT_UI_FORBIDDEN
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err(format!("DPAPI protect failed (code: {})", std::io::Error::last_os_error()));
    }
    let res = unsafe {
        let slice = std::slice::from_raw_parts(out_blob.pb_data, out_blob.cb_data as usize);
        let vec = slice.to_vec();
        LocalFree(out_blob.pb_data as *mut std::ffi::c_void);
        vec
    };
    Ok(res)
}

#[cfg(not(windows))]
pub fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    Ok(data.to_vec())
}

#[allow(dead_code)]
#[cfg(not(windows))]
pub fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>, String> {
    Ok(data.to_vec())
}

// ---------------------------------------------------------------- Accounts

pub type GeminiAccount = store::Account;

/// Reads the list of Gemini Web accounts from storage.
pub fn list_accounts() -> Vec<GeminiAccount> {
    store::list().unwrap_or_default()
}

/// Reads and decrypts the cookie string for an account.
pub fn get_account_cookie(id: &str) -> Result<String, String> {
    store::cookie(id)
}

pub fn save_accounts(accounts: &[GeminiAccount]) -> Result<(), String> {
    store::save(accounts)
}

pub fn delete_account(id: &str) -> Result<(), String> {
    store::delete_account(id)
}

// ---------------------------------------------------------------- Native Account Operations

/// Launches the native Chrome/Edge browser sign-in for a named account in a background worker thread.
pub fn launch_native_login(account_name: &str) {
    let name = account_name.trim().to_string();
    if name.is_empty() { return; }
    std::thread::spawn(move || {
        let root = dir();
        let existing = store::list_in(&root).unwrap_or_default();
        let account = existing.into_iter().find(|a| a.name.eq_ignore_ascii_case(&name))
            .unwrap_or_else(|| store::Account {
                id: crate::common::uuid(),
                name: name.clone(),
                port: 0,
                enabled: true,
                profile: format!("profiles/{}", crate::common::uuid()),
                request_count: 0,
                error_count: 0,
                extra: Default::default(),
            });
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let _ = browser::login(&account, false, &cancel);
    });
}

pub fn claude_base(_url: &str) -> String {
    "".to_string()
}

pub fn extract_port(_url: &str) -> Option<u16> {
    None
}

pub fn ensure_bridge(_port: u16) -> Result<(), String> {
    Ok(())
}

/// Provides truthful usage limits / session status bars for Gemini Web in ccat usage view.
pub fn fetch_usage_bars(_cfg: &crate::Config) -> (Vec<crate::usage::Bar>, String) {
    let root = dir();
    let accounts = store::list_in(&root).unwrap_or_default();
    if accounts.is_empty() {
        return (Vec::new(), "No Gemini Web accounts added yet.".into());
    }
    let mut bars = Vec::new();

    for a in &accounts {
        let (used, part) = if !a.enabled {
            (0.0, "Disabled".to_string())
        } else {
            (0.0, format!("Requests: {} · Errors: {}", a.request_count, a.error_count))
        };

        bars.push(crate::usage::Bar {
            label: a.name.clone(),
            used,
            resets: 0,
            window: 0,
            part,
            ..Default::default()
        });
    }

    (bars, String::new())
}

// ---------------------------------------------------------------- Settings Page

pub(crate) fn page(
    _ed: &mut crate::settings::providers::ProvidersEditor,
    ui: &mut eframe::egui::Ui,
    cfg: &mut crate::Config,
    feed: &crate::usage::Feed,
) {
    use crate::settings::{
        group,
        providers::{count, head, name, note, row},
    };
    use eframe::egui::{self, Color32, RichText, TextEdit};
    let p = crate::pal(ui);
    let Some(idx) = cfg.providers.iter().position(|p| p.gemini_web) else { return };
    let pr = &mut cfg.providers[idx];
    let title = pr.name.clone();

    // Ensure models are 100% manual and default models always present
    if pr.models.is_empty() {
        pr.models = default_models();
    }

    head(ui, |ui| {
        let mut on = !pr.disabled;
        if crate::switch(ui, &mut on, "")
            .on_hover_text("Off: Gemini Web is not offered in the pickers and not used for routing.")
            .changed()
        {
            pr.disabled = !on;
        }
        name(ui, &title, on);
    });

    group(ui, |ui, g| {
        feed.analytics(ui, g, &title);
        g.item(ui, |ui| {
            feed.limits(ui, 4, "Google Gemini", &feed.usage.gemini_web.bars, feed.usage.gemini_web.fetched, &feed.usage.gemini_web.error);
        });
    });

    let n = crate::model_count(pr);
    row(ui, |ui, _| {
        count(ui, "Models (Manual 100%)", n);
    });

    let root = dir();
    let mut accounts = store::list_in(&root).unwrap_or_default();
    let mut settings = store::settings_in(&root).unwrap_or_default();
    let green = Color32::from_rgb(46, 160, 67);

    group(ui, |ui, g| {
        g.item(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Google Accounts (Native Pool)").color(p.fg).strong());
            });
            ui.add_space(4.0);

            if accounts.is_empty() {
                note(ui, "No Gemini Web accounts yet. Add your Google account below to start.", p.subtle);
            } else {
                let mut accounts_changed = false;
                let mut delete_id = None;
                let mut relogin_name = None;

                for a in &mut accounts {
                    let is_preferred = settings.preferred_account_id.as_deref() == Some(&a.id);
                    ui.horizontal(|ui| {
                        let dot_color = if a.enabled { green } else { p.muted };
                        let dot_char = if a.enabled { "●" } else { "○" };
                        ui.label(RichText::new(dot_char).color(dot_color).size(13.0));

                        let label = format!("{} (Req: {}, Err: {})", a.name, a.request_count, a.error_count);
                        let tag = if is_preferred { "Preferred" } else if a.enabled { "Active" } else { "Disabled" };
                        let resp = crate::pick_row(ui, is_preferred, &label, false, tag);
                        if resp.on_hover_text("Click to make preferred account").clicked() {
                            settings.preferred_account_id = Some(a.id.clone());
                            let _ = store::save_settings_in(&root, &settings);
                        }

                        if ui.button("Re-login").on_hover_text("Open Chrome/Edge to refresh session").clicked() {
                            relogin_name = Some(a.name.clone());
                        }

                        if crate::icon_btn(ui, crate::Icon::Delete).on_hover_text("Remove this account").clicked() {
                            delete_id = Some(a.id.clone());
                        }
                    });
                }

                if let Some(name) = relogin_name {
                    launch_native_login(&name);
                }
                if let Some(id) = delete_id {
                    let _ = store::delete_account_in(&root, &id);
                    accounts.retain(|a| a.id != id);
                    accounts_changed = true;
                }
                if accounts_changed {
                    let _ = store::save_in(&root, &accounts);
                }
            }
        });

        // Add Account Form
        g.item(ui, |ui| {
            ui.label(RichText::new("Add Google Account").color(p.fg).strong());
            ui.add_space(2.0);
            let state_id = ui.id().with("new-gemini-acct-name");
            let mut name_buf: String = ui.data_mut(|d| d.get_temp(state_id).unwrap_or_default());

            ui.horizontal(|ui| {
                ui.add(TextEdit::singleline(&mut name_buf).hint_text("Account name (e.g. Personal)"));
                if ui.button("Sign in with Google").on_hover_text("Opens Chrome/Edge to sign in natively").clicked() {
                    let trimmed = name_buf.trim();
                    if !trimmed.is_empty() {
                        launch_native_login(trimmed);
                        name_buf.clear();
                    }
                }
            });
            ui.data_mut(|d| d.insert_temp(state_id, name_buf));
            note(
                ui,
                "Uses an isolated browser profile to sign in to Gemini. Cookies are encrypted locally with Windows DPAPI.",
                p.subtle,
            );
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dpapi_roundtrip() {
        #[cfg(windows)]
        {
            let original = b"__Secure-1PSID=test_cookie_12345; __Secure-1PSIDTS=ts_98765";
            let protected = dpapi_protect(original).expect("protect");
            assert_ne!(&protected, original);
            let unprotected = dpapi_unprotect(&protected).expect("unprotect");
            assert_eq!(&unprotected, original);
        }
    }

    #[test]
    fn test_default_models() {
        let models = default_models();
        assert!(models.contains(&"gemini-4-argon".to_string()));
        assert!(models.contains(&"gemini-3.1-pro".to_string()));
        assert!(models.contains(&"gemini-3.8-flash".to_string()));
        assert!(models.contains(&"gemini-3.5-flash-lite".to_string()));
    }
}
