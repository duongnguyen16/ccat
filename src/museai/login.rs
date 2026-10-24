//! Dedicated, short-lived Chrome login. Never attaches to the user's existing browser.
use super::{native, Account, secure_directory, secure_file};
use serde_json::{json, Value};
use std::{fs, io::Write, path::{Path, PathBuf}, process::{Child, Command, Stdio}, sync::atomic::{AtomicBool, Ordering}, time::{Duration, Instant}};
use tungstenite::Message;

struct Browser {
    child: Child,
    profile: PathBuf,
    #[cfg(windows)] job: isize,
}
impl Drop for Browser {
    fn drop(&mut self) {
        #[cfg(windows)] unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job as _); }
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Only this invocation's random, private profile; never touch the user's Chrome data.
        let _ = fs::remove_dir_all(&self.profile);
    }
}
fn chrome() -> Option<PathBuf> {
    let mut paths = Vec::new();
    if let Some(p) = std::env::var_os("CHROME_PATH") { paths.push(PathBuf::from(p)); }
    #[cfg(windows)] {
        for root in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Some(p) = std::env::var_os(root) { paths.push(PathBuf::from(p).join("Google/Chrome/Application/chrome.exe")); }
        }
    }
    #[cfg(target_os="macos")] paths.push(PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"));
    #[cfg(unix)] {
        for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            for name in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"] { paths.push(dir.join(name)); }
        }
    }
    paths.into_iter().find(|p| p.is_file())
}
fn start(url: &str) -> Result<Browser, String> {
    let exe = chrome().ok_or("Google Chrome not found; install Chrome or set CHROME_PATH")?;
    let profile = super::ccat_data_dir().join(format!("muse-login-{}", crate::antigravity::random_hex(12)));
    fs::create_dir_all(profile.parent().ok_or("Invalid profile path")?).map_err(|e| e.to_string())?;
    fs::create_dir(&profile).map_err(|e| e.to_string())?;
    if let Err(e) = secure_directory(&profile) { let _ = fs::remove_dir(&profile); return Err(e); }
    let mut command = Command::new(exe);
    command.args(["--remote-debugging-port=0", "--remote-debugging-address=127.0.0.1", "--no-first-run", "--no-default-browser-check"])
        .arg(format!("--user-data-dir={}", profile.display())).arg(url)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = match command.spawn() { Ok(c) => c, Err(e) => { let _ = fs::remove_dir_all(&profile); return Err(e.to_string()); } };
    let mut browser = Browser { child, profile, #[cfg(windows)] job: 0 };
    #[cfg(windows)] unsafe {
        use std::{os::windows::io::AsRawHandle, mem::size_of};
        use windows_sys::Win32::System::JobObjects::*;
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() { return Err("Could not create Chrome process job".into()); }
        browser.job = job as isize;
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as _, size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) == 0
            || AssignProcessToJobObject(job, browser.child.as_raw_handle() as _) == 0 {
            return Err("Could not bind Chrome to its cleanup job".into());
        }
    }
    Ok(browser)
}

fn devtools_endpoint(profile: &Path) -> Result<(String, u16), String> {
    let text = fs::read_to_string(profile.join("DevToolsActivePort")).map_err(|_| "Chrome debugging endpoint is not ready")?;
    let mut lines = text.lines();
    let port: u16 = lines.next().unwrap_or("").parse().map_err(|_| "Invalid Chrome debugging port")?;
    let path = lines.next().unwrap_or("");
    if port == 0 || !path.starts_with("/devtools/browser/") || path.contains(['\r', '\n', '?', '#']) { return Err("Invalid Chrome debugging endpoint".into()); }
    Ok((format!("ws://127.0.0.1:{port}{path}"), port))
}

pub(super) fn finish(url: &str, cancel: &AtomicBool, dir: &Path) -> Result<Account, String> {
    if cancel.load(Ordering::Relaxed) { return Err("Sign-in cancelled".into()); }
    let mut browser = start(url)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let (endpoint, port) = loop {
        check(&mut browser, cancel, deadline)?;
        if let Ok(e) = devtools_endpoint(&browser.profile) { break e; }
        std::thread::sleep(Duration::from_millis(200));
    };
    let mut ws = native::socket(&endpoint, "127.0.0.1", port, 1024 * 1024).map_err(|e| e.message)?;
    let mut id = 0;
    let mut last_error = None;
    while Instant::now() < deadline {
        check(&mut browser, cancel, deadline)?;
        id += 1;
        // Storage.getCookies is a browser-target command; Network.getCookies needs a page session.
        ws.send(Message::Text(json!({"id":id,"method":"Storage.getCookies"}).to_string().into())).map_err(|_| "Chrome sign-in connection closed")?;
        let reply = loop {
            check(&mut browser, cancel, deadline)?;
            match ws.read().map_err(|_| "Chrome sign-in connection closed or timed out")? {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text).map_err(|_| "Invalid Chrome response")?;
                    if value["id"].as_u64() == Some(id) { break value; }
                },
                Message::Ping(_) => { ws.flush().map_err(|_| "Chrome connection closed")?; },
                Message::Close(_) => return Err("Chrome window closed before sign-in".into()),
                _ => {},
            }
        };
        if reply.get("error").is_some() { return Err("Chrome could not export this login's cookies".into()); }
        let jar = cookie_document(&reply["result"]["cookies"])?;
        if let Ok(header) = native::cookies(&jar.to_string()) {
            match native::identity(&header) {
                Ok(identity) => {
                    check(&mut browser, cancel, deadline)?;
                    let mut document = jar;
                    document["viewer_id"] = json!(identity.viewer_id);
                    document["vm_id"] = json!(identity.vm_id);
                    save(dir, &document)?;
                    return Ok(Account { viewer_id: identity.viewer_id, vm_id: identity.vm_id, tier: "Free plan".into(), disabled: false });
                },
                Err(e) if matches!(e.status, 401 | 403) => { last_error = Some(e.message); },
                Err(e) => return Err(e.message),
            }
        }
        std::thread::sleep(Duration::from_millis(800));
    }
    Err(last_error.unwrap_or_else(|| "MuseAI sign-in timed out".into()))
}
fn check(browser: &mut Browser, cancel: &AtomicBool, deadline: Instant) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) { return Err("Sign-in cancelled".into()); }
    if Instant::now() >= deadline { return Err("MuseAI sign-in timed out".into()); }
    if browser.child.try_wait().map_err(|e| e.to_string())?.is_some() { return Err("Chrome window closed before sign-in".into()); }
    Ok(())
}
fn cookie_document(cookies: &Value) -> Result<Value, String> {
    let mut values = serde_json::Map::new();
    let mut expires = serde_json::Map::new();
    for c in cookies.as_array().ok_or("Invalid Chrome cookie response")? {
        let domain = c["domain"].as_str().unwrap_or("");
        if !native::muse_domain(domain) { continue; }
        let Some(name) = c["name"].as_str() else { continue };
        let Some(value) = c["value"].as_str() else { continue };
        values.insert(name.into(), json!(value));
        if let Some(expiry) = c["expires"].as_f64().filter(|v| *v > 0.0) { expires.insert(name.into(), json!(expiry as u64)); }
    }
    Ok(json!({"cookies":values,"expires":expires}))
}
fn save(dir: &Path, document: &Value) -> Result<(), String> {
    // A single credential document holds identity + cookies. Never rewrite global ccat config,
    // muse-cli config.json or usage.json. Validation precedes any durable change.
    native::cookies(&document.to_string()).map_err(|e| e.message)?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    secure_directory(dir)?;
    let target = dir.join("cookies.txt");
    let temp = dir.join(format!("cookies-{}.tmp", crate::antigravity::random_hex(12)));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let mut file = options.open(&temp).map_err(|e| e.to_string())?;
        secure_file(&temp)?;
        file.write_all(document.to_string().as_bytes()).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        if target.is_file() {
            let backup = dir.join(format!("cookies-{}.bak", crate::antigravity::random_hex(12)));
            fs::copy(&target, &backup).map_err(|e| e.to_string())?;
            secure_file(&backup)?;
        }
        replace(&temp, &target)
    })();
    if result.is_err() { let _ = fs::remove_file(&temp); }
    result
}
fn replace(source: &Path, target: &Path) -> Result<(), String> {
    #[cfg(windows)] {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
        let from: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 { return Err(std::io::Error::last_os_error().to_string()); }
        Ok(())
    }
    #[cfg(not(windows))] { fs::rename(source, target).map_err(|e| e.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cdp_cookie_export_cannot_include_other_sites() {
        let doc = cookie_document(&json!([
            {"domain":".muse.ai","name":"hatch_sess","value":"test"},
            {"domain":"evilmuse.ai","name":"private","value":"secret"},
            {"domain":"muse.ai.evil.test","name":"other","value":"secret"}
        ])).unwrap();
        assert_eq!(doc["cookies"].as_object().unwrap().len(), 1);
        assert_eq!(native::cookies(&doc.to_string()).unwrap(), "hatch_sess=test");
    }
    #[test]
    fn credential_save_preserves_unrelated_files_and_previous_login() {
        let dir = std::env::temp_dir().join(format!("ccat-native-login-{}", crate::antigravity::random_hex(12)));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("config.json"), "unrelated config").unwrap();
        fs::write(dir.join("cookies.txt"), "hatch_sess=old").unwrap();
        save(&dir, &json!({"cookies":{"hatch_sess":"new"},"viewer_id":"viewer-new"})).unwrap();
        assert_eq!(fs::read_to_string(dir.join("config.json")).unwrap(), "unrelated config");
        assert_eq!(super::super::active_account_in(&dir).unwrap().viewer_id, "viewer-new");
        let before = fs::read(dir.join("cookies.txt")).unwrap();
        assert!(save(&dir, &json!({"cookies":{"not_a_session":"bad"}})).is_err());
        assert_eq!(fs::read(dir.join("cookies.txt")).unwrap(), before);
        assert!(fs::read_dir(&dir).unwrap().filter_map(Result::ok).any(|e| e.path().extension().is_some_and(|x| x == "bak") && fs::read_to_string(e.path()).unwrap() == "hatch_sess=old"));
        fs::remove_dir_all(dir).unwrap();
    }
}
