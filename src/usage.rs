//! Usage tray: while any ccat runs, one helper process (`ccat --ccat-tray`) shows Claude and Antigravity quota in the
//! system tray; clicking it opens the popup (`ccat --ccat-usage X`), a second small process, since winit allows one
//! event loop per process. Both share `usage.json` next to the config: the data and the fetch cooldowns.
//! Claude: Claude Code's own login (read only, never refreshed here) against `/api/oauth/usage`.
//! Antigravity: `retrieveUserQuotaSummary` per Google account.

use crate::{antigravity, chatgpt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, fs, path::PathBuf, sync::Mutex, time::Duration};

pub const TRAY_FLAG: &str = "--ccat-tray";
pub const POPUP_FLAG: &str = "--ccat-usage";
/// How often the tray refreshes: the reference monitor's cadence, gentle on `/api/oauth/usage`'s rate limit.
const POLL_SECS: u64 = 180;
/// The reload buttons can't go faster than this.
const RELOAD_SECS: u64 = 30;
const FIVE_HOURS: u64 = 5 * 3600;
const WEEK: u64 = 7 * 24 * 3600;

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Bar {
    pub label: String,
    /// Percent used, 0–100.
    pub used: f64,
    /// Unix time the window resets; 0 = not started.
    pub resets: u64,
    /// Window length in seconds (5 h or a week).
    pub window: u64,
}

impl Bar {
    /// How far through its window we are, 0–1; `None` before the window starts.
    pub fn elapsed(&self, now: u64) -> Option<f64> {
        (self.resets > 0 && self.window > 0).then(|| (1.0 - self.resets.saturating_sub(now) as f64 / self.window as f64).clamp(0.0, 1.0))
    }

    /// Used up as far as anyone can tell: it reads 100% (99.96% left nothing worth a request).
    pub fn out(&self) -> bool {
        self.used.round() >= 100.0
    }

    /// How worried to look: used up (red), past 90% or ahead of the time passed (orange), else fine (blue).
    pub fn level(&self, now: u64) -> Level {
        if self.out() {
            Level::Full
        } else if self.used > 90.0 || self.elapsed(now).is_some_and(|e| self.used > e * 100.0 + 0.5) {
            Level::Warn
        } else {
            Level::Ok
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Level {
    Ok,
    Warn,
    Full,
}

impl Bar {
}

/// One quota source: its bars, the last error, and the fetch bookkeeping both processes respect.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Source {
    pub bars: Vec<Bar>,
    pub error: String,
    pub fetched: u64,
    /// No request before this (a 429's Retry-After).
    pub not_before: u64,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Usage {
    /// "Team", "Max 5x"…; empty when there is no Claude login.
    pub plan: String,
    pub claude: Source,
    /// ChatGPT's allowance (Work/Codex), from a Codex login: set when one exists.
    pub chatgpt: Source,
    pub chatgpt_plan: String,
    pub chatgpt_on: bool,
    /// Google account email → its quota.
    pub antigravity: HashMap<String, Source>,
}

/// The config, read only: the tray runs unseen, so it must never move a file it can't parse aside as `load` does.
fn read_config() -> crate::Config {
    fs::read_to_string(crate::config_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn config_stamp() -> Option<std::time::SystemTime> {
    fs::metadata(crate::config_path()).and_then(|m| m.modified()).ok()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn usage_path() -> PathBuf {
    crate::config_path().with_file_name("usage.json")
}

pub fn load_usage() -> Usage {
    fs::read_to_string(usage_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Written whole and renamed into place: the tray and the popup both read it.
fn save_usage(u: &Usage) {
    let p = usage_path();
    let tmp = p.with_extension(format!("json.{}.tmp", std::process::id()));
    if fs::write(&tmp, serde_json::to_string_pretty(u).unwrap()).is_ok() {
        let _ = fs::rename(&tmp, &p);
    }
}

/// Modified time of `usage.json`, to notice the other process's writes.
pub fn usage_stamp() -> Option<std::time::SystemTime> {
    fs::metadata(usage_path()).and_then(|m| m.modified()).ok()
}

// ---------------------------------------------------------------- time

/// Unix seconds of an RFC 3339 time ("2026-10-19T12:38:11Z", "…T11:50:00.349366+00:00"); 0 when absent or unreadable.
pub fn parse_time(s: &str) -> u64 {
    let num = |a: usize, b: usize| s.get(a..b).and_then(|x| x.parse::<i64>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(se)) = (num(0, 4), num(5, 7), num(8, 10), num(11, 13), num(14, 16), num(17, 19)) else { return 0 };
    let rest = s[19..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match rest.as_bytes().first() {
        Some(b'+' | b'-') => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (rest.get(1..3).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0) * 3600 + rest.get(4..6).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0) * 60)
        }
        _ => 0,
    };
    u64::try_from(civil_days(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset).unwrap_or(0)
}

/// Days since 1970-01-01 of a date (Howard Hinnant's days_from_civil).
fn civil_days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// The date of a day number (civil_from_days).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The local calendar day of a unix time, as a day number.
fn local_day(t: u64) -> i64 {
    let (y, m, d) = local_date(t);
    civil_days(y, m, d)
}

#[cfg(windows)]
fn local_date(t: u64) -> (i64, i64, i64) {
    use windows_sys::Win32::{
        Foundation::{FILETIME, SYSTEMTIME},
        System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
    };
    let ticks = t * 10_000_000 + 116_444_736_000_000_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    // SAFETY: plain out-parameters on the stack.
    unsafe {
        let (mut utc, mut loc): (SYSTEMTIME, SYSTEMTIME) = (std::mem::zeroed(), std::mem::zeroed());
        if FileTimeToSystemTime(&ft, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut loc) == 0 {
            return civil_from_days((t / 86_400) as i64);
        }
        (loc.wYear as i64, loc.wMonth as i64, loc.wDay as i64)
    }
}

#[cfg(not(windows))]
fn local_date(t: u64) -> (i64, i64, i64) {
    civil_from_days((t / 86_400) as i64)
}

/// Local (weekday 0 = Sunday, hour, minute) of a unix time.
#[cfg(windows)]
fn local(t: u64) -> (u16, u16, u16) {
    use windows_sys::Win32::{
        Foundation::{FILETIME, SYSTEMTIME},
        System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
    };
    let ticks = t * 10_000_000 + 116_444_736_000_000_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    // SAFETY: plain out-parameters on the stack.
    unsafe {
        let (mut utc, mut loc): (SYSTEMTIME, SYSTEMTIME) = (std::mem::zeroed(), std::mem::zeroed());
        if FileTimeToSystemTime(&ft, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut loc) == 0 {
            return utc_parts(t);
        }
        (loc.wDayOfWeek, loc.wHour, loc.wMinute)
    }
}

#[cfg(not(windows))]
fn local(t: u64) -> (u16, u16, u16) {
    utc_parts(t)
}

fn utc_parts(t: u64) -> (u16, u16, u16) {
    (((t / 86_400 + 4) % 7) as u16, (t % 86_400 / 3600) as u16, (t % 3600 / 60) as u16)
}

/// "Resets in 3h 30m (22:00)" within a day, "Resets on Fri, 10:00" beyond.
pub fn reset_text(resets: u64, now: u64) -> String {
    if resets == 0 {
        return "Starts when used".into();
    }
    if resets <= now {
        return "Resetting…".into();
    }
    let left = resets - now;
    let (wd, h, m) = local(resets);
    if left < 86_400 {
        format!("Resets in {}h {}m ({h:02}:{m:02})", left / 3600, left % 3600 / 60)
    } else {
        let day = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][wd as usize % 7];
        format!("Resets on {day}, {h:02}:{m:02}")
    }
}

// ---------------------------------------------------------------- Claude

/// Claude Code's login as it left it: (access token, plan name). Read only: refreshing it here could race Claude
/// Code and sign it out, and while ccat runs Claude Code keeps it fresh itself.
fn claude_login() -> Option<Result<(String, String), String>> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| dirs_home().join(".claude"));
    let v: Value = serde_json::from_str(&fs::read_to_string(dir.join(".credentials.json")).ok()?).ok()?;
    let o = &v["claudeAiOauth"];
    let token = o["accessToken"].as_str().filter(|t| !t.is_empty())?.to_string();
    let plan = plan_name(o["subscriptionType"].as_str().unwrap_or(""), o["rateLimitTier"].as_str().unwrap_or(""));
    let expired = o["expiresAt"].as_u64().is_some_and(|ms| ms / 1000 <= now());
    Some(if expired { Err(format!("{plan}|Login expired: start Claude Code to refresh it.")) } else { Ok((token, plan)) })
}

fn dirs_home() -> PathBuf {
    PathBuf::from(std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).unwrap_or_default())
}

/// Claude Code is signed in to a claude.ai plan (an API-key setup has no plan quota to show).
pub fn claude_set_up() -> bool {
    claude_login().is_some()
}

/// "team" → "Team", "max" + "default_claude_max_20x" → "Max 20x".
fn plan_name(kind: &str, tier: &str) -> String {
    let mut name: String = kind.chars().take(1).flat_map(char::to_uppercase).chain(kind.chars().skip(1)).collect();
    if kind == "max" {
        if let Some(x) = tier.rsplit('_').next().filter(|x| x.ends_with('x')) {
            name = format!("{name} {x}");
        }
    }
    name
}

/// Bars from `/api/oauth/usage`: its `limits` list (session, weekly, per-model weekly), or the older top-level fields.
pub fn parse_claude(v: &Value) -> Vec<Bar> {
    let bar = |label: String, used: f64, resets: &Value, window| Bar { label, used, resets: resets.as_str().map_or(0, parse_time), window };
    let mut bars: Vec<Bar> = v["limits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let used = l["percent"].as_f64().unwrap_or(0.0);
            match (l["kind"].as_str()?, l["scope"]["model"]["display_name"].as_str()) {
                ("session", _) => Some(bar("Session (5hr)".into(), used, &l["resets_at"], FIVE_HOURS)),
                ("weekly_all", _) => Some(bar("Weekly (7 day)".into(), used, &l["resets_at"], WEEK)),
                ("weekly_scoped", Some(model)) => Some(bar(format!("Weekly {model}"), used, &l["resets_at"], WEEK)),
                _ => None,
            }
        })
        .collect();
    if bars.is_empty() {
        for (key, label, window) in [("five_hour", "Session (5hr)", FIVE_HOURS), ("seven_day", "Weekly (7 day)", WEEK)] {
            if let Some(used) = v[key]["utilization"].as_f64() {
                bars.push(bar(label.into(), used, &v[key]["resets_at"], window));
            }
        }
    }
    bars
}

/// `claude-code/<version>` of the installed CLI, asked once.
fn user_agent() -> &'static str {
    static UA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    UA.get_or_init(|| {
        let mut cmd = std::process::Command::new("claude");
        cmd.arg("--version");
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // CREATE_NO_WINDOW
        let out = cmd.output().ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let version = out.split_whitespace().next().filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit())).unwrap_or("2.1.303");
        format!("claude-code/{version}")
    })
}

fn fetch_claude(token: &str) -> Result<Vec<Bar>, (String, u64)> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).http_status_as_error(false).build().into();
    let mut res = agent
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", user_agent())
        .call()
        .map_err(|e| (format!("Can't reach Anthropic: {e}"), 0))?;
    let status = res.status().as_u16();
    let retry = res.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let body = res.body_mut().read_to_string().unwrap_or_default();
    match status {
        200 => serde_json::from_str(&body).map(|v| parse_claude(&v)).map_err(|e| (format!("Unreadable answer: {e}"), 0)),
        401 => Err(("Login expired: start Claude Code to refresh it.".into(), 0)),
        429 => {
            let wait = retry.unwrap_or(300).max(60);
            Err((format!("Rate limited, trying again in {} min.", wait.div_ceil(60)), wait))
        }
        s => Err((format!("Anthropic answered {s}."), 0)),
    }
}

// ---------------------------------------------------------------- ChatGPT

/// (plan, bars) from `/backend-api/wham/usage`: its primary (5-hour) and secondary (weekly) rate-limit windows.
pub fn parse_wham(v: &Value, now: u64) -> (String, Vec<Bar>) {
    let window = |w: &Value, label: &str, default: u64| {
        let used = w["used_percent"].as_f64()?;
        let resets = w["reset_at"].as_u64().filter(|r| *r > 0).or_else(|| w["reset_after_seconds"].as_u64().map(|s| now + s)).unwrap_or(0);
        Some(Bar { label: label.into(), used, resets, window: w["limit_window_seconds"].as_u64().filter(|s| *s > 0).unwrap_or(default) })
    };
    let limit = &v["rate_limit"];
    let bars = [window(&limit["primary_window"], "Session (5hr)", FIVE_HOURS), window(&limit["secondary_window"], "Weekly (7 day)", WEEK)].into_iter().flatten().collect();
    (plan_name(v["plan_type"].as_str().unwrap_or(""), ""), bars)
}

/// Whether there is a login to ask with: a ChatGPT account signed in to ccat (Codex CLI's own login is not used).
fn gpt_has_login(cfg: &crate::Config) -> bool {
    cfg.providers.iter().filter(|p| p.chatgpt).flat_map(|p| &p.chatgpt_accounts).any(|a| a.codex() && a.usable())
}

/// The usage call, with each login in turn (ccat's own Codex-route accounts first, then Codex CLI's, read only) until one
/// answers. Tokens and the account id are never logged.
fn fetch_chatgpt(cfg: &crate::Config) -> Result<(String, Vec<Bar>), (String, u64)> {
    let mut logins: Vec<(String, String, bool)> = Vec::new();
    for a in cfg.providers.iter().filter(|p| p.chatgpt).flat_map(|p| &p.chatgpt_accounts).filter(|a| a.codex() && a.usable()) {
        let login = if a.cli() { chatgpt::codex_cli_tokens().ok_or_else(String::new) } else { chatgpt::codex_token(&a.key) };
        if let Ok((t, id)) = login {
            logins.push((t, id, true));
        }
    }
    let mut last = ("ChatGPT sign-in expired: sign in again in Settings → Providers.".to_string(), 0);
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).http_status_as_error(false).build().into();
    for (token, id, _) in logins {
        let sent = agent
            .get(chatgpt::WHAM_USAGE)
            .header("Authorization", format!("Bearer {token}"))
            .header("ChatGPT-Account-Id", id)
            .header("User-Agent", "codex-cli")
            .header("Accept", "application/json")
            .call();
        let mut res = match sent {
            Ok(r) => r,
            Err(e) => {
                last = (format!("Can't reach ChatGPT: {e}"), 0);
                continue;
            }
        };
        let status = res.status().as_u16();
        let retry = res.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
        let body = res.body_mut().read_to_string().unwrap_or_default();
        match status {
            200 => match serde_json::from_str::<Value>(&body) {
                Ok(v) => return Ok(parse_wham(&v, now())),
                Err(e) => last = (format!("Unreadable answer: {e}"), 0),
            },
            401 | 403 => last = ("ChatGPT login expired: sign in again in ccat.".into(), 0),
            429 => {
                let wait = retry.unwrap_or(300).max(60);
                return Err((format!("Rate limited, trying again in {} min.", wait.div_ceil(60)), wait));
            }
            s => last = (format!("ChatGPT answered {s}."), 0),
        }
    }
    Err(last)
}

// ---------------------------------------------------------------- Antigravity

/// Bars from `retrieveUserQuotaSummary`: per group (Gemini, Claude & GPT), its 5-hour and weekly buckets.
pub fn parse_ag(v: &Value) -> Vec<Bar> {
    let mut bars = Vec::new();
    for b in v["groups"].as_array().into_iter().flatten().flat_map(|g| g["buckets"].as_array().into_iter().flatten()) {
        let id = b["bucketId"].as_str().unwrap_or("");
        // The second group is Claude, Sonnet and GPT-OSS; Claude is what anyone here uses it for.
        let group = if id.starts_with("gemini") { "Gemini" } else { "Claude" };
        let (window, label) = if b["window"] == "5h" || id.ends_with("5h") { (FIVE_HOURS, "5hr") } else { (WEEK, "Weekly") };
        let used = (1.0 - b["remainingFraction"].as_f64().unwrap_or(1.0)) * 100.0;
        bars.push(Bar { label: format!("{group} ({label})"), used: used.clamp(0.0, 100.0), resets: b["resetTime"].as_str().map_or(0, parse_time), window });
    }
    // Session before weekly within a group, as in the Claude section.
    bars.sort_by_key(|b| (!b.label.starts_with("Gemini"), b.window));
    bars
}

/// Google access tokens live an hour: kept per refresh token so a poll every few minutes doesn't refresh each time.
fn google_token(refresh: &str) -> Result<String, String> {
    static CACHE: Mutex<Option<HashMap<String, (String, std::time::Instant)>>> = Mutex::new(None);
    let mut g = CACHE.lock().unwrap();
    let cache = g.get_or_insert_with(HashMap::new);
    if let Some((t, _)) = cache.get(refresh).filter(|(_, exp)| *exp > std::time::Instant::now() + Duration::from_secs(60)) {
        return Ok(t.clone());
    }
    let (t, life) = antigravity::refresh(refresh)?;
    cache.insert(refresh.to_string(), (t.clone(), std::time::Instant::now() + life));
    Ok(t)
}

/// One model group (Gemini, Claude) across several accounts: how many can serve it right now, per plan, and which
/// account the bars show.
#[derive(Clone, Debug, PartialEq)]
pub struct Pool {
    pub group: String,
    /// "Free 0/4 · Plus 0/1 · Pro 1/2": accounts with quota left, of those that answered.
    pub ready: String,
    /// The account the group's bars are: "duongn (Pro)".
    pub shown: String,
}

/// "General": per group, the bars of the account with the most left right now (none ready: the one back soonest),
/// so every bar is one real account's 5-hour and weekly buckets. Averaging accounts mixed plans with different
/// windows and sizes (a free account has no 5-hour bucket) into numbers no account had.
pub fn pool(sources: &[(&antigravity::Account, &Source)]) -> (Vec<Bar>, Vec<Pool>) {
    if sources.len() < 2 {
        return (sources.first().map(|(_, s)| s.bars.clone()).unwrap_or_default(), Vec::new());
    }
    let group = |b: &Bar| b.label.split(" (").next().unwrap_or("").to_string();
    let mut groups: Vec<String> = Vec::new();
    for b in sources.iter().flat_map(|(_, s)| &s.bars) {
        if !groups.contains(&group(b)) {
            groups.push(group(b));
        }
    }
    let (mut bars, mut pools) = (Vec::new(), Vec::new());
    for g in groups {
        // (account, its bars of this group, highest use, back at: the latest reset among its used-up buckets)
        let each: Vec<_> = sources
            .iter()
            .map(|(a, s)| {
                let gb: Vec<Bar> = s.bars.iter().filter(|b| group(b) == g).cloned().collect();
                let used = gb.iter().map(|b| if b.out() { 100.0 } else { b.used }).fold(0.0, f64::max);
                let back = gb.iter().filter(|b| b.out()).map(|b| if b.resets == 0 { u64::MAX } else { b.resets }).max().unwrap_or(0);
                (*a, gb, used, back)
            })
            .filter(|(_, gb, ..)| !gb.is_empty())
            .collect();
        let best = each.iter().filter(|e| e.2 < 100.0).min_by(|x, y| x.2.total_cmp(&y.2)).or_else(|| each.iter().min_by_key(|e| e.3));
        let Some((a, gb, ..)) = best else { continue };
        bars.extend(gb.iter().cloned());
        // (plan rank, ready, accounts), lowest plan first: the order the proxy spends them in.
        let mut tiers: Vec<(u8, usize, usize)> = Vec::new();
        for (a, _, used, _) in &each {
            let (r, ok) = (antigravity::rank(&a.plan), (*used < 100.0) as usize);
            match tiers.iter_mut().find(|t| t.0 == r) {
                Some(t) => (t.1, t.2) = (t.1 + ok, t.2 + 1),
                None => tiers.push((r, ok, 1)),
            }
        }
        tiers.sort();
        let name = |r: u8| ["Free", "Unknown", "Plus", "Pro", "Ultra"][r as usize];
        let ready = tiers.iter().map(|(r, n, of)| format!("{} {n}/{of}", name(*r))).collect::<Vec<_>>().join(" · ");
        let alias = a.email.split('@').next().unwrap_or(&a.email);
        let plan = antigravity::plan_label(&a.plan);
        pools.push(Pool { group: g, ready, shown: if plan.is_empty() { alias.to_string() } else { format!("{alias} ({plan})") } });
    }
    bars.sort_by_key(|b| (!b.label.starts_with("Gemini"), b.window));
    (bars, pools)
}

/// The Antigravity bars and pools for `view` ("" = all accounts, else one account's email). Failed accounts are left
/// out unless `stale` (every one failed: their last answers, greyed out).
pub fn ag_pool(u: &Usage, accounts: &[antigravity::Account], view: &str, stale: bool) -> (Vec<Bar>, Vec<Pool>) {
    let mine: Vec<_> = accounts.iter().filter(|a| view.is_empty() || a.email == view).filter_map(|a| u.antigravity.get(&a.email).map(|s| (a, s))).filter(|(_, s)| stale || s.error.is_empty()).collect();
    pool(&mine)
}

/// The Antigravity bars for `view`: what the tray dial and tooltip show, the same as the popup.
pub fn ag_view(u: &Usage, accounts: &[antigravity::Account], view: &str) -> Vec<Bar> {
    ag_pool(u, accounts, view, false).0
}

// ---------------------------------------------------------------- refresh

fn due(s: &Source, force: bool, now: u64) -> bool {
    now >= s.not_before && now.saturating_sub(s.fetched) >= if force { RELOAD_SECS } else { POLL_SECS }
}

/// Fetches whatever is due (`force` = a reload click: due after `RELOAD_SECS` instead of `POLL_SECS`; a 429's wait
/// always holds), then saves. Errors stay with their own source.
pub fn refresh(force_claude: bool, force_ag: bool, force_gpt: bool) -> Usage {
    let cfg = read_config();
    let accounts: Vec<antigravity::Account> = cfg.providers.iter().filter(|p| p.antigravity).flat_map(|p| p.accounts.clone()).collect();
    let mut u = load_usage();
    let t = now();
    match claude_login() {
        None => (u.plan, u.claude) = (String::new(), Source::default()),
        Some(Err(e)) => {
            let (plan, msg) = e.split_once('|').unwrap_or(("", &e));
            u.plan = plan.into();
            u.claude.error = msg.into();
        }
        Some(Ok((token, plan))) if due(&u.claude, force_claude, t) => {
            u.plan = plan;
            u.claude.fetched = t;
            match fetch_claude(&token) {
                Ok(bars) => (u.claude.bars, u.claude.error) = (bars, String::new()),
                Err((e, wait)) => (u.claude.error, u.claude.not_before) = (e, t + wait),
            }
        }
        Some(Ok((_, plan))) => u.plan = plan,
    }
    u.chatgpt_on = gpt_has_login(&cfg);
    if !u.chatgpt_on {
        (u.chatgpt, u.chatgpt_plan) = (Source::default(), String::new());
    } else if due(&u.chatgpt, force_gpt, t) {
        u.chatgpt.fetched = t;
        match fetch_chatgpt(&cfg) {
            Ok((plan, bars)) => (u.chatgpt_plan, u.chatgpt.bars, u.chatgpt.error) = (plan, bars, String::new()),
            Err((e, wait)) => (u.chatgpt.error, u.chatgpt.not_before) = (e, t + wait),
        }
    }
    u.antigravity.retain(|email, _| accounts.iter().any(|a| a.email == *email));
    for a in &accounts {
        let s = u.antigravity.entry(a.email.clone()).or_default();
        if !due(s, force_ag, t) {
            continue;
        }
        s.fetched = t;
        match google_token(&a.refresh).and_then(|tok| antigravity::quota_summary(&tok, &a.project)) {
            Ok(v) => (s.bars, s.error) = (parse_ag(&v), String::new()),
            Err(e) => s.error = e,
        }
    }
    save_usage(&u);
    u
}

// ---------------------------------------------------------------- tray icon

/// One source on the tray icon: its 5-hour use (the number) and its weekly use (the arc).
#[derive(Clone, Debug, Default)]
pub struct Dial {
    pub five: Option<Bar>,
    pub week: Option<Bar>,
}

/// A usage source: what the popup's sections and the tray dial's parts are made of.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Src {
    Claude,
    ChatGpt,
    Antigravity,
}

impl Src {
    const ALL: [Src; 3] = [Src::Claude, Src::ChatGpt, Src::Antigravity];

    fn name(self) -> &'static str {
        match self {
            Src::Claude => "Claude",
            Src::ChatGpt => "ChatGPT",
            Src::Antigravity => "Antigravity",
        }
    }
}

/// The saved `source_order` as sources: known names in their saved order, then the rest in the default order,
/// so a missing, short or garbled list still gives all three.
fn ordered(saved: &[String]) -> Vec<Src> {
    let mut out: Vec<Src> = Vec::new();
    for n in saved {
        if let Some(s) = Src::ALL.into_iter().find(|s| s.name() == n) {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    let rest: Vec<Src> = Src::ALL.into_iter().filter(|s| !out.contains(s)).collect();
    out.extend(rest);
    out
}

/// Which sources the applied tiers route to, primary or fallback (endpoints have no usage source).
fn uses(cfg: &crate::Config) -> [bool; 3] {
    let mut on = [false; 3];
    for r in &cfg.tiers {
        for name in std::iter::once(&r.provider).chain(r.fallbacks.iter().map(|f| &f.provider)) {
            match cfg.providers.iter().find(|p| p.name == *name) {
                _ if name.is_empty() => on[0] = true,
                Some(p) if p.chatgpt => on[1] = true,
                Some(p) if p.antigravity => on[2] = true,
                _ => {}
            }
        }
    }
    on
}

/// The sources to show, in the saved order: those set up (`avail`, indexed like `Src::ALL`), narrowed to the
/// profile's when following it, unless that would leave nothing.
fn shown(cfg: &crate::Config, mode: crate::ViewMode, avail: [bool; 3]) -> Vec<Src> {
    let mine = uses(cfg);
    let pick = |follow: bool| -> Vec<Src> { ordered(&cfg.source_order).into_iter().filter(|s| avail[*s as usize] && (!follow || mine[*s as usize])).collect() };
    let v = pick(mode == crate::ViewMode::Follow);
    if v.is_empty() { pick(false) } else { v }
}

/// One dial part per shown source, in order. Antigravity takes the worse of its groups (Gemini, Claude & GPT)
/// in the chosen view.
pub fn icon_dials(u: &Usage, shown: &[Src], ag: &[Bar]) -> Vec<Dial> {
    let find = |bars: &[Bar], window: u64| bars.iter().filter(|b| b.window == window).max_by(|a, b| a.used.total_cmp(&b.used)).cloned();
    let dial = |s: &Source| {
        let bars = if s.error.is_empty() { s.bars.as_slice() } else { &[] };
        Dial { five: find(bars, FIVE_HOURS), week: find(bars, WEEK) }
    };
    shown
        .iter()
        .map(|s| match s {
            Src::Claude => dial(&u.claude),
            Src::ChatGpt => dial(&u.chatgpt),
            Src::Antigravity => Dial { five: find(ag, FIVE_HOURS), week: find(ag, WEEK) },
        })
        .collect()
}

/// An arc of the dial: centre-line radius, start angle and signed sweep (radians, y up; negative = clockwise),
/// half its thickness. Ends are rounded.
struct Arc {
    r: f32,
    start: f32,
    sweep: f32,
    half: f32,
}

impl Arc {
    /// How much of pixel (x, y) (offsets from the centre, y up) the first `f` of the arc covers, 0–1.
    fn cover(&self, x: f32, y: f32, f: f32) -> f32 {
        if f <= 0.0 {
            return 0.0;
        }
        let span = self.sweep.abs() * f.min(1.0);
        let tau = std::f32::consts::TAU;
        let along = ((y.atan2(x) - self.start) * self.sweep.signum()).rem_euclid(tau);
        let dist = if along <= span {
            ((x * x + y * y).sqrt() - self.r).abs()
        } else {
            let end = |a: f32| ((x - self.r * a.cos()).powi(2) + (y - self.r * a.sin()).powi(2)).sqrt();
            end(self.start).min(end(self.start + span * self.sweep.signum()))
        };
        (self.half - dist + 0.5).clamp(0.0, 1.0)
    }
}

/// RGBA pixels of a `size`² tray icon, one part per dial (1–3), each a pie of its 5-hour use inside a ring arc of
/// its weekly use. One: a full pie in a full ring, filling clockwise from the top. Two: a dial cut across the
/// middle, the first above, the second below, both filling from the left. Three: 120° sectors, the first centred
/// at 12 o'clock, then 4 and 8, each filling clockwise from its start edge. Blank bands separate the parts.
/// Light taskbar → black ink; orange when ahead of pace or past 90%, red when used up.
pub fn icon_pixels(size: u32, dials: &[Dial], light_taskbar: bool, now: u64) -> Vec<u8> {
    use std::f32::consts::{PI, TAU};
    let s = size as f32;
    let mut px = vec![0u8; (size * size * 4) as usize];
    let ink: [u8; 3] = if light_taskbar { [0, 0, 0] } else { [255, 255, 255] };
    let tint = |b: Option<&Bar>| match b.map(|b| b.level(now)) {
        Some(Level::Full) => [224, 80, 80],
        Some(Level::Warn) => [240, 150, 50],
        _ => ink,
    };
    let (c, outer) = (s / 2.0, s / 2.0 - 0.5);
    let w = (s * 0.13).max(2.0);
    let r = outer - w / 2.0;
    let inner = outer - w;
    let gap = (s * 0.06).max(1.0); // half a blank band
    let blank = Dial::default();
    let dials: Vec<&Dial> = if dials.is_empty() { vec![&blank] } else { dials.iter().take(3).collect() };
    let n = dials.len();
    let span = TAU / n as f32;
    // Each part's start angle and direction (-1 clockwise, +1 counter-clockwise; y up).
    let starts: Vec<(f32, f32)> = match n {
        1 => vec![(PI / 2.0, -1.0)],
        2 => vec![(PI, -1.0), (-PI, 1.0)],
        _ => (0..3).map(|k| (5.0 * PI / 6.0 - k as f32 * TAU / 3.0, -1.0)).collect(),
    };
    // Ends clear the bands, caps included.
    let t0 = ((gap + w / 2.0) / r).clamp(0.0, 1.0).asin();
    let arcs: Vec<Arc> = starts
        .iter()
        .map(|&(a, d)| if n == 1 { Arc { r, start: a, sweep: -TAU + 0.001, half: w / 2.0 } } else { Arc { r, start: a + d * t0, sweep: d * (span - 2.0 * t0), half: w / 2.0 } })
        .collect();
    // The pies sit inside the ring with a gap, clipped to their sector by the two bounding rays.
    let pie_r = inner - (s * 0.07).max(1.0);
    let pie = |x: f32, y: f32, (start, dir): (f32, f32), f: f32| -> f32 {
        let d = x.hypot(y);
        let mut cover = (pie_r - d + 0.5).clamp(0.0, 1.0);
        if n > 1 {
            let side = |a: f32| a.cos() * y - a.sin() * x; // which side of the ray at angle a
            cover *= (dir * side(start) - gap + 0.5).clamp(0.0, 1.0) * (-dir * side(start + dir * span) - gap + 0.5).clamp(0.0, 1.0);
        }
        if f < 1.0 {
            let along = ((y.atan2(x) - start) * dir).rem_euclid(TAU);
            cover *= (d * (span * f.max(0.0) - along) + 0.5).clamp(0.0, 1.0);
        }
        cover
    };
    for py in 0..size {
        for pxx in 0..size {
            let (x, y) = (pxx as f32 + 0.5 - c, c - (py as f32 + 0.5));
            let (mut rgb, mut a) = (ink, 0.0f32);
            for (i, dial) in dials.iter().enumerate() {
                let five = dial.five.as_ref();
                let track = pie(x, y, starts[i], 1.0) * 80.0 / 255.0;
                let fill = pie(x, y, starts[i], five.map_or(0.0, |b| b.used as f32 / 100.0));
                if track + fill > 0.0 {
                    rgb = if fill > 0.0 { tint(five) } else { ink };
                    a = fill + (1.0 - fill) * track;
                }
                let week = dial.week.as_ref();
                let track = arcs[i].cover(x, y, 1.0) * 80.0 / 255.0;
                let fill = arcs[i].cover(x, y, week.map_or(0.0, |b| b.used as f32 / 100.0));
                if track + fill > 0.0 {
                    rgb = if fill > 0.0 { tint(week) } else { ink };
                    a = fill + (1.0 - fill) * track;
                }
            }
            let i = ((py * size + pxx) * 4) as usize;
            px[i..i + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], (a * 255.0).round() as u8]);
        }
    }

    px
}

/// "Claude 5h 39% · 7d 35%" style lines for the tooltip, one per shown source (Windows cuts it at 127 characters).
pub fn tooltip(u: &Usage, shown: &[Src], ag: &[Bar]) -> String {
    let short = |bars: &[Bar]| {
        let pick = |w| bars.iter().filter(|b| b.window == w).map(|b| b.used).fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.max(x))));
        [("5h", pick(FIVE_HOURS)), ("7d", pick(WEEK))].iter().filter_map(|(n, v)| v.map(|v| format!("{n} {v:.0}%"))).collect::<Vec<_>>().join(" · ")
    };
    let mut lines = vec!["ccat usage".to_string()];
    for s in shown {
        let ag_src = Source { bars: ag.to_vec(), ..Default::default() };
        let src = match s {
            Src::Claude => &u.claude,
            Src::ChatGpt => &u.chatgpt,
            Src::Antigravity => &ag_src,
        };
        lines.push(if src.error.is_empty() { format!("{} {}", s.name(), short(&src.bars)) } else { format!("{}: see window", s.name()) });
    }
    lines.join("\n").chars().take(127).collect()
}

// ---------------------------------------------------------------- tray process (Windows)

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, RECT},
        System::{
            Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS},
            Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD},
            Threading::{CreateMutexW, OpenMutexW},
        },
        UI::{
            HiDpi::GetDpiForSystem,
            WindowsAndMessaging::{GetSystemMetrics, SystemParametersInfoW, SM_CXSMICON, SPI_GETWORKAREA},
        },
    };

    pub const MUTEX: &str = "Local\\ccat-usage-tray";

    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Takes the tray's single-instance mutex; `false` when another tray holds it. The handle lives until exit.
    pub fn claim() -> bool {
        // SAFETY: a named mutex; the handle is deliberately kept for the process lifetime.
        unsafe { !CreateMutexW(std::ptr::null(), 0, wide(MUTEX).as_ptr()).is_null() && GetLastError() != ERROR_ALREADY_EXISTS }
    }

    pub fn tray_running() -> bool {
        // SAFETY: opened only to test that it exists, then closed.
        unsafe {
            let h = OpenMutexW(0x0010_0000, 0, wide(MUTEX).as_ptr()); // SYNCHRONIZE
            if h.is_null() {
                return false;
            }
            CloseHandle(h);
            true
        }
    }

    /// Other running processes with this exe's file name, not counting `skip`.
    pub fn others(skip: &[u32]) -> usize {
        let me = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase())).unwrap_or_default();
        let mut n = 0;
        // SAFETY: a process snapshot walked with a correctly sized entry, then closed.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap.is_null() || snap as isize == -1 {
                return 1; // can't tell: assume someone still needs the tray
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e) != 0;
            while ok {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase();
                if name == me && e.th32ProcessID != std::process::id() && !skip.contains(&e.th32ProcessID) {
                    n += 1;
                }
                ok = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        n
    }

    /// Per-monitor DPI aware before anything measures the screen: tray clicks, the work area and the DPI then all
    /// come in physical pixels (eframe would set this too, but only once its window exists).
    pub fn dpi_aware() {
        use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
        // SAFETY: a process-wide setting, made before any window exists.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    }

    /// Turns off the window's open/close animation: the popup should just be there.
    pub fn no_transitions(title: &str) {
        use windows_sys::Win32::{Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED}, UI::WindowsAndMessaging::FindWindowW};
        let on: i32 = 1;
        // SAFETY: our own top-level window, found by its title (hidden windows are found too); a BOOL attribute.
        unsafe {
            let hwnd = FindWindowW(std::ptr::null(), wide(title).as_ptr());
            if !hwnd.is_null() {
                DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED as u32, &on as *const i32 as *const c_void, 4);
            }
        }
    }

    pub fn light_taskbar() -> bool {
        let mut v: u32 = 0;
        let mut size = 4u32;
        let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
        // SAFETY: a DWORD read into a 4-byte buffer.
        let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), wide("SystemUsesLightTheme").as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut v as *mut u32 as *mut c_void, &mut size) };
        r == 0 && v == 1
    }

    pub fn icon_size() -> u32 {
        // SAFETY: no arguments beyond the metric id.
        (unsafe { GetSystemMetrics(SM_CXSMICON) }).clamp(16, 64) as u32
    }

    /// Work area (physical pixels) and the display scale.
    pub fn work_area() -> (RECT, f32) {
        let mut r = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };
        // SAFETY: SPI_GETWORKAREA writes one RECT.
        unsafe {
            SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut RECT as *mut c_void, 0);
            (r, GetDpiForSystem() as f32 / 96.0)
        }
    }
}

/// Starts the tray helper unless one is running. Every ccat calls this at start and then now and then, so the tray
/// comes back if the terminal that started it was closed while other sessions go on.
pub fn ensure_tray() {
    #[cfg(windows)]
    if !win::tray_running() {
        use std::os::windows::process::CommandExt;
        if let Ok(exe) = std::env::current_exe() {
            // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no console, and not killed with this terminal.
            let _ = std::process::Command::new(exe)
                .arg(TRAY_FLAG)
                .creation_flags(0x0000_0008 | 0x0000_0200)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

/// The tray helper: one per user session (a named mutex), gone a little after the last ccat closes.
#[cfg(windows)]
pub fn run_tray() {
    use std::{
        process::{Child, Command, Stdio},
        sync::{Arc, Mutex as M},
        time::Instant,
    };
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    use windows_sys::Win32::{
        System::Threading::GetCurrentThreadId,
        UI::WindowsAndMessaging::{AllowSetForegroundWindow, DispatchMessageW, GetMessageW, PostThreadMessageW, SetTimer, TranslateMessage, MSG, WM_APP, WM_TIMER},
    };

    win::dpi_aware();
    if !win::claim() {
        return;
    }
    // SAFETY: plain Win32 calls on this thread; it owns the message queue the tray posts to.
    let tid = unsafe { GetCurrentThreadId() };
    // Clicks wake the loop at once (a timer would add up to its period of lag); only a left release counts.
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, position, .. } = e {
            // SAFETY: posting to our own thread's queue.
            unsafe { PostThreadMessageW(tid, WM_APP, position.x.max(0.0) as usize, position.y.max(0.0) as isize) };
        }
    }));

    // Fetching happens off the message loop, so the tray stays responsive; results arrive through usage.json.
    std::thread::spawn(|| loop {
        refresh(false, false, false);
        std::thread::sleep(Duration::from_secs(15));
    });

    let draw = |u: &Usage| {
        let cfg = read_config();
        let accounts: Vec<antigravity::Account> = cfg.providers.iter().filter(|p| p.antigravity).flat_map(|p| p.accounts.clone()).collect();
        let ag = ag_view(u, &accounts, &cfg.usage_view);
        let src = shown(&cfg, cfg.tray_mode, [claude_set_up(), u.chatgpt_on, !ag.is_empty()]);
        let size = win::icon_size();
        let icon = Icon::from_rgba(icon_pixels(size, &icon_dials(u, &src, &ag), win::light_taskbar(), now()), size, size).ok();
        (icon, tooltip(u, &src, &ag))
    };
    let (icon, tip) = draw(&load_usage());
    let Ok(tray) = TrayIconBuilder::new().with_icon(icon.unwrap_or_else(|| Icon::from_rgba(vec![0; 16 * 16 * 4], 16, 16).unwrap())).with_tooltip(tip).build() else { return };

    // SAFETY: a thread timer (no window): WM_TIMER lands in this thread's queue every 2 s.
    unsafe { SetTimer(std::ptr::null_mut(), 0, 2000, None) };
    let mut stamp = (usage_stamp(), config_stamp());
    let mut lonely_since: Option<Instant> = None;
    let mut last_draw = Instant::now();
    let popup: Arc<M<Option<Child>>> = Arc::new(M::new(None));
    let closed_at: Arc<M<Option<Instant>>> = Arc::new(M::new(None));
    // SAFETY: the standard message loop; MSG is plain data.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        if msg.hwnd.is_null() && msg.message == WM_APP {
            let mut p = popup.lock().unwrap();
            let open = p.as_mut().is_some_and(|c| c.try_wait().ok().flatten().is_none());
            // The press that reached the tray already took focus from an open popup, which closes itself on that:
            // this release must not open it again.
            let just_closed = closed_at.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_millis(500));
            if open {
                if let Some(c) = p.as_mut() {
                    let _ = c.kill();
                }
                *p = None;
            } else if !just_closed {
                use std::os::windows::process::CommandExt;
                // SAFETY: the tray just got the click, so it may hand the foreground on.
                unsafe { AllowSetForegroundWindow(u32::MAX) };
                let child = std::env::current_exe().and_then(|exe| {
                    Command::new(exe).args([POPUP_FLAG, &msg.wParam.to_string(), &msg.lParam.to_string()]).creation_flags(0x0000_0008 | 0x0000_0200).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
                });
                if let Ok(c) = child {
                    let (watch, at) = (popup.clone(), closed_at.clone());
                    *p = Some(c);
                    std::thread::spawn(move || loop {
                        std::thread::sleep(Duration::from_millis(50));
                        let mut g = watch.lock().unwrap();
                        match g.as_mut().map(|c| c.try_wait()) {
                            Some(Ok(None)) => continue,
                            Some(Ok(Some(_))) => {
                                *g = None;
                                *at.lock().unwrap() = Some(Instant::now());
                            }
                            _ => {}
                        }
                        break;
                    });
                }
            }
            continue;
        }
        if msg.hwnd.is_null() && msg.message == WM_TIMER {
            let popup_pid: Vec<u32> = popup.lock().unwrap().as_ref().map(|c| c.id()).into_iter().collect();
            if win::others(&popup_pid) == 0 {
                // Grace for a restart: a new ccat started within the window keeps the tray.
                if lonely_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(10) {
                    if let Some(c) = popup.lock().unwrap().as_mut() {
                        let _ = c.kill();
                    }
                    break;
                }
            } else {
                lonely_since = None;
            }
            // Redraw on new data or a changed config (Antigravity view, tray mode, order), and every minute anyway: the time markers move.
            let now_stamp = (usage_stamp(), config_stamp());
            if now_stamp != stamp || last_draw.elapsed() > Duration::from_secs(60) {
                stamp = now_stamp;
                last_draw = Instant::now();
                let (icon, tip) = draw(&load_usage());
                let _ = tray.set_icon(icon);
                let _ = tray.set_tooltip(Some(tip));
            }
            continue;
        }
        // SAFETY: standard dispatch for the tray's hidden window.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// ---------------------------------------------------------------- popup

const POPUP_W: f32 = 340.0;
const BLUE: egui::Color32 = egui::Color32::from_rgb(0x2a, 0x78, 0xd6);
const TRACK: egui::Color32 = egui::Color32::from_rgb(0x03, 0x20, 0x42);
const ORANGE: egui::Color32 = egui::Color32::from_rgb(0xe0, 0x8a, 0x2e);
const ORANGE_TRACK: egui::Color32 = egui::Color32::from_rgb(0x3d, 0x26, 0x0c);
const RED: egui::Color32 = egui::Color32::from_rgb(0xcf, 0x3b, 0x3b);
const RED_TRACK: egui::Color32 = egui::Color32::from_rgb(0x42, 0x10, 0x10);

// Every row has a fixed height, so the window knows its size before it opens (no growing into place).
const PAD: f32 = 14.0;
const HEADER_H: f32 = 30.0;
/// The header icon's height in points; its texture is 96px, so it stays sharp on a hi-dpi screen.
const ICON_H: f32 = 22.0;

/// `rgba` (w x h, straight alpha) averaged down to `to` x `to`: a plain GPU minify of the 1254px logo aliases.
fn shrink(rgba: &[u8], w: usize, h: usize, to: usize) -> egui::ColorImage {
    let mut pixels = Vec::with_capacity(to * to);
    for ty in 0..to {
        let (y0, y1) = (ty * h / to, ((ty + 1) * h / to).max(ty * h / to + 1));
        for tx in 0..to {
            let (x0, x1) = (tx * w / to, ((tx + 1) * w / to).max(tx * w / to + 1));
            let mut sum = [0.0f32; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let px = &rgba[(y * w + x) * 4..][..4];
                    let a = px[3] as f32 / 255.0;
                    // Premultiplied, so a transparent pixel's colour doesn't bleed into the edge.
                    sum[..3].iter_mut().zip(px).for_each(|(s, c)| *s += *c as f32 * a);
                    sum[3] += px[3] as f32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as f32;
            let c = |i: usize| (sum[i] / n).round().clamp(0.0, 255.0) as u8;
            pixels.push(egui::Color32::from_rgba_premultiplied(c(0), c(1), c(2), c(3)));
        }
    }
    egui::ColorImage::new([to, to], pixels)
}

fn icon_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let src = crate::app_icon();
    ctx.load_texture("ccat-icon", shrink(&src.rgba, src.width as usize, src.height as usize, 96), egui::TextureOptions::LINEAR)
}
const SECTION_H: f32 = 30.0;
const LABEL_H: f32 = 20.0;
const BAR_H: f32 = 7.0;
const RESET_H: f32 = 18.0;
const BAR_GAP: f32 = 4.0;
const BAR_BLOCK: f32 = 6.0 + LABEL_H + BAR_GAP + BAR_H + BAR_GAP + RESET_H;
const ERROR_H: f32 = 17.0;
const POOL_H: f32 = 6.0 + 18.0;
/// Between sections, with a 1px rule across its middle; the footer has one above it too.
const SECTION_GAP: f32 = 21.0;
const FOOTER_H: f32 = 12.0 + 18.0;

use eframe::egui;

/// Where the popup sits, in points: its left edge, the lowest its bottom may go, and the highest its top may go.
/// It opens next to the tray click (pixels) and stays inside the work area, so a taskbar on any side works.
fn anchor(click: (f64, f64)) -> (f32, f32, f32) {
    #[cfg(windows)]
    {
        win::dpi_aware();
        let (wa, scale) = win::work_area();
        let w = POPUP_W * scale;
        let x = (click.0 as f32 - w / 2.0).clamp(wa.left as f32 + 8.0, (wa.right as f32 - w - 8.0).max(wa.left as f32));
        let bottom = (click.1 as f32).min(wa.bottom as f32 - 8.0);
        (x / scale, bottom / scale, (wa.top as f32 + 8.0) / scale)
    }
    #[cfg(not(windows))]
    {
        let _ = click;
        (100.0, 700.0, 0.0)
    }
}

/// One block of the popup: Claude, ChatGPT, or Antigravity in the chosen view.
struct Section {
    src: Src,
    title: &'static str,
    /// 0 = Claude, 1 = Antigravity, 2 = ChatGPT: which reload it triggers.
    which: usize,
    bars: Vec<Bar>,
    errors: Vec<String>,
    /// The last fetch failed: the bars are the last answers, drawn greyed out.
    stale: bool,
    /// Antigravity's groups across several accounts, each drawn above its group's bars.
    pools: Vec<Pool>,
}

/// The Usage tab's blocks: the shown sources, in the saved order.
fn sections(cfg: &crate::Config, u: &Usage, claude: bool) -> Vec<Section> {
    let accounts = ag_accounts(cfg);
    let alias = |e: &str| e.split('@').next().unwrap_or(e).to_string();
    let mut out = Vec::new();
    if claude {
        out.push(Section { src: Src::Claude, title: "Claude", which: 0, bars: u.claude.bars.clone(), errors: [u.claude.error.clone()].into_iter().filter(|e| !e.is_empty()).collect(), stale: !u.claude.error.is_empty(), pools: Vec::new() });
    }
    if u.chatgpt_on && (!u.chatgpt.bars.is_empty() || !u.chatgpt.error.is_empty()) {
        out.push(Section { src: Src::ChatGpt, title: "ChatGPT", which: 2, bars: u.chatgpt.bars.clone(), errors: [u.chatgpt.error.clone()].into_iter().filter(|e| !e.is_empty()).collect(), stale: !u.chatgpt.error.is_empty(), pools: Vec::new() });
    }
    if !accounts.is_empty() {
        let mine = accounts.iter().filter(|a| cfg.usage_view.is_empty() || a.email == cfg.usage_view);
        let errors = mine.clone().filter_map(|a| u.antigravity.get(&a.email).filter(|s| !s.error.is_empty()).map(|s| format!("{}: {}", alias(&a.email), s.error))).collect();
        let sources: Vec<&Source> = mine.filter_map(|a| u.antigravity.get(&a.email)).collect();
        // Every account failed: show what they last said, greyed out, rather than nothing.
        let stale = !sources.is_empty() && sources.iter().all(|s| !s.error.is_empty());
        let (bars, pools) = ag_pool(u, &accounts, &cfg.usage_view, stale);
        out.push(Section { src: Src::Antigravity, title: "Antigravity", which: 1, bars, errors, stale, pools });
    }
    let keep = shown(cfg, cfg.usage_view_mode, Src::ALL.map(|s| out.iter().any(|x| x.src == s)));
    keep.iter().filter_map(|s| out.iter().position(|x| x.src == *s).map(|i| out.swap_remove(i))).collect()
}

/// The window height the sections need, in points: the same sums the layout below adds up.
fn height(sections: &[Section]) -> f32 {
    let body: f32 = sections.iter().map(|s| SECTION_H + s.errors.len() as f32 * ERROR_H + s.bars.len() as f32 * BAR_BLOCK + s.pools.len() as f32 * POOL_H).sum();
    let gaps = sections.len().saturating_sub(1) as f32 * SECTION_GAP;
    let empty = if sections.is_empty() { ERROR_H * 2.0 } else { 0.0 };
    (PAD + HEADER_H + body + gaps + empty + FOOTER_H + PAD).ceil()
}

fn ag_accounts(cfg: &crate::Config) -> Vec<antigravity::Account> {
    cfg.providers.iter().filter(|p| p.antigravity).flat_map(|p| p.accounts.clone()).collect()
}

/// `h` points of space with a 1px rule across the middle.
fn rule(ui: &mut egui::Ui, h: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
    let y = rect.center().y.round() + 0.5;
    ui.painter().line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], egui::Stroke::new(1.0, crate::pal(ui).border));
}

/// "just now", "45s ago", "3m ago", "2h ago"; "never" before the first answer.
fn ago(t: u64, now: u64) -> String {
    match now.saturating_sub(t) {
        _ if t == 0 => "never".into(),
        0..10 => "just now".into(),
        d @ 10..60 => format!("{d}s ago"),
        d @ 60..3600 => format!("{}m ago", d / 60),
        d => format!("{}h ago", d / 3600),
    }
}

/// A full-width row of exactly `h` points.
fn row<R>(ui: &mut egui::Ui, h: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(egui::vec2(ui.available_width(), h));
        add(ui)
    })
    .inner
}

/// `who`: the account a merged group's bars are, beside the first one's label.
fn bar_ui(ui: &mut egui::Ui, b: &Bar, now: u64, stale: bool, who: Option<&str>) {
    let p = crate::pal(ui);
    let ink = if stale { p.muted } else { p.fg };
    ui.add_space(6.0);
    row(ui, LABEL_H, |ui| {
        ui.label(egui::RichText::new(&b.label).size(13.0).color(ink));
        if let Some(w) = who {
            ui.add(egui::Label::new(egui::RichText::new(format!("· {w}")).size(11.5).color(p.subtle)).truncate())
                .on_hover_text("These bars are this account's: the one with the most left now, or the first back when none has any.");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(egui::RichText::new(format!("{:.0}%", b.used)).size(13.0).color(ink)));
    });
    ui.add_space(BAR_GAP);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), BAR_H), egui::Sense::hover());
    let pt = ui.painter();
    // Each fill sits on a dark track of its own hue.
    let (fill_color, track) = match b.level(now) {
        _ if stale => (p.border_strong, p.sunken),
        Level::Full => (RED, RED_TRACK),
        Level::Warn => (ORANGE, ORANGE_TRACK),
        Level::Ok => (BLUE, TRACK),
    };
    pt.rect_filled(rect, 2.0, track);
    let fill = rect.width() * (b.used.clamp(0.0, 100.0) / 100.0) as f32;
    if fill > 0.0 {
        pt.rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(fill, rect.height())), 2.0, fill_color);
    }
    // A week reads in days: a notch between each.
    if b.window == WEEK {
        for d in 1..7 {
            let x = rect.left() + rect.width() * d as f32 / 7.0;
            pt.line_segment([egui::pos2(x, rect.top() + 2.0), egui::pos2(x, rect.bottom() - 2.0)], egui::Stroke::new(1.0, p.bg));
        }
    }
    if let Some(e) = b.elapsed(now) {
        let x = rect.left() + rect.width() * e as f32;
        pt.line_segment([egui::pos2(x, rect.top() - 1.0), egui::pos2(x, rect.bottom() + 1.0)], egui::Stroke::new(2.0, if stale { p.muted } else { egui::Color32::WHITE }));
    }
    ui.add_space(BAR_GAP);
    let text = reset_text(b.resets, now);
    row(ui, RESET_H, |ui| ui.label(egui::RichText::new(text).size(11.5).color(p.subtle)));
}

// ---------------------------------------------------------------- analytics

/// Tokens per local day over the last `DAYS`, by model or by provider, from the proxy's `tokens.jsonl`.
const DAYS: i64 = 30;

#[derive(Default)]
struct Stats {
    /// (name, tokens in, tokens out), most tokens first.
    keys: Vec<(String, u64, u64)>,
    /// The first day shown (a day number; see `civil_days`).
    first: i64,
    /// (day - first, key index) → tokens.
    cells: HashMap<(i64, usize), u64>,
}

fn tokens_path() -> PathBuf {
    crate::config_path().with_file_name("tokens.jsonl")
}

/// "claude-opus-5-5" → "Opus 5.5", "claude-haiku-4-5-20251001" → "Haiku 4.5"; anything else as it is.
pub fn pretty_model(m: &str) -> String {
    let Some(rest) = m.strip_prefix("claude-") else { return m.to_string() };
    let mut parts = rest.split('-');
    let family = parts.next().unwrap_or("");
    let name: String = family.chars().take(1).flat_map(char::to_uppercase).chain(family.chars().skip(1)).collect();
    let version: Vec<&str> = parts.take_while(|p| p.len() <= 2 && p.chars().all(|c| c.is_ascii_digit())).collect();
    if version.is_empty() { name } else { format!("{name} {}", version.join(".")) }
}

/// 950, 405.6k, 21.9M, 1.6B.
pub fn fmt_tokens(n: f64) -> String {
    let (v, unit) = match n {
        n if n >= 1e9 => (n / 1e9, "B"),
        n if n >= 1e6 => (n / 1e6, "M"),
        n if n >= 1e3 => (n / 1e3, "k"),
        n => return format!("{n:.0}"),
    };
    let s = format!("{v:.1}");
    format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
}

fn load_stats(text: &str, by_model: bool, today: i64) -> Stats {
    let first = today - DAYS + 1;
    let mut st = Stats { first, ..Default::default() };
    let mut totals: HashMap<String, (u64, u64)> = HashMap::new();
    let mut by_day: Vec<(i64, String, u64)> = Vec::new();
    for v in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        let day = local_day(v["t"].as_u64().unwrap_or(0));
        if day < first || day > today {
            continue;
        }
        let n = |k: &str| v[k].as_u64().unwrap_or(0);
        let (input, output) = (n("input") + n("cache_read") + n("cache_write"), n("output"));
        let key = if by_model { pretty_model(v["model"].as_str().unwrap_or("?")) } else { v["provider"].as_str().unwrap_or("?").to_string() };
        let t = totals.entry(key.clone()).or_default();
        (t.0, t.1) = (t.0 + input, t.1 + output);
        by_day.push((day - first, key, input + output));
    }
    st.keys = totals.into_iter().map(|(k, (i, o))| (k, i, o)).collect();
    st.keys.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)).then(a.0.cmp(&b.0)));
    for (d, key, n) in by_day {
        let i = st.keys.iter().position(|k| k.0 == key).unwrap_or(0);
        *st.cells.entry((d, i)).or_default() += n;
    }
    st
}

/// Blues from dark to light for the biggest keys; the rest share a grey.
const SHADES: [u32; 6] = [0x3c6fd6, 0x5c8be3, 0x7ca5ec, 0x9cbff3, 0xbdd5f8, 0xdbe8fb];

fn shade(i: usize, p: &crate::Pal) -> egui::Color32 {
    SHADES.get(i).map_or(p.subtle, |&v| crate::hex(v))
}

/// "Oct 4" for a day number.
fn day_label(day: i64) -> String {
    let (_, m, d) = civil_from_days(day);
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(m as usize).clamp(1, 12) - 1];
    format!("{month} {d}")
}

/// Stacked bars, one per day, with the day's breakdown on hover.
fn chart(ui: &mut egui::Ui, st: &Stats) {
    let p = crate::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 150.0), egui::Sense::hover());
    let plot = egui::Rect::from_min_max(rect.min + egui::vec2(34.0, 6.0), rect.max - egui::vec2(0.0, 18.0));
    let day_total = |d: i64| (0..st.keys.len()).map(|i| st.cells.get(&(d, i)).copied().unwrap_or(0)).sum::<u64>();
    let max = (0..DAYS).map(day_total).max().unwrap_or(0).max(1) as f64;
    // A round step: 1, 2 or 5 × 10^n, about four lines high.
    let raw = max / 4.0;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(raw);
    let top = (max / step).ceil() * step;
    let pt = ui.painter();
    let y_of = |v: f64| plot.bottom() - (v / top) as f32 * plot.height();
    let mut v = 0.0;
    while v <= top + 0.5 {
        let y = y_of(v);
        if v > 0.0 {
            pt.line_segment([egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)], egui::Stroke::new(1.0, p.border));
        }
        pt.text(egui::pos2(plot.left() - 8.0, y), egui::Align2::RIGHT_CENTER, fmt_tokens(v), egui::FontId::proportional(10.5), p.subtle);
        v += step;
    }
    let slot = plot.width() / DAYS as f32;
    let hover = resp.hover_pos().filter(|h| plot.x_range().contains(h.x)).map(|h| (((h.x - plot.left()) / slot) as i64).clamp(0, DAYS - 1));
    for d in 0..DAYS {
        let x = plot.left() + slot * (d as f32 + 0.19);
        let mut base = 0.0;
        for i in 0..st.keys.len() {
            let n = st.cells.get(&(d, i)).copied().unwrap_or(0) as f64;
            if n == 0.0 {
                continue;
            }
            let bar = egui::Rect::from_min_max(egui::pos2(x, y_of(base + n)), egui::pos2(x + slot * 0.62, y_of(base)));
            let c = shade(i, p);
            pt.rect_filled(bar, 1.5, if hover.is_some_and(|h| h != d) { c.gamma_multiply(0.55) } else { c });
            base += n;
        }
        if d % 7 == 2 {
            pt.text(egui::pos2(x + slot * 0.31, plot.bottom() + 4.0), egui::Align2::CENTER_TOP, day_label(st.first + d), egui::FontId::proportional(10.5), p.subtle);
        }
    }
    if let Some(d) = hover {
        let rows: Vec<(usize, u64)> = (0..st.keys.len()).filter_map(|i| st.cells.get(&(d, i)).map(|&n| (i, n))).filter(|(_, n)| *n > 0).collect();
        resp.on_hover_ui_at_pointer(|ui| {
            ui.label(egui::RichText::new(day_label(st.first + d)).strong().color(p.fg));
            if rows.is_empty() {
                ui.label(egui::RichText::new("Nothing").color(p.subtle));
            }
            for (i, n) in rows {
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                    ui.painter().circle_filled(dot.center(), 3.5, shade(i, p));
                    ui.label(format!("{}: {}", st.keys[i].0, fmt_tokens(n as f64)));
                });
            }
        });
    }
}

/// The legend: each key's share, tokens in and out; the biggest six unless `more`.
fn legend(ui: &mut egui::Ui, st: &Stats, more: &mut bool) {
    let p = crate::pal(ui);
    let total: u64 = st.keys.iter().map(|k| k.1 + k.2).sum::<u64>().max(1);
    let shown = if *more { st.keys.len() } else { st.keys.len().min(6) };
    for (i, (name, input, output)) in st.keys.iter().take(shown).enumerate() {
        row(ui, 22.0, |ui| {
            let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().rect_filled(dot, 2.0, shade(i, p));
            // Numbers first, from the right; the name takes what's left and is cut short if it must be.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let share = (input + output) as f64 / total as f64 * 100.0;
                ui.add_sized([42.0, 18.0], egui::Label::new(egui::RichText::new(format!("{share:.1}%")).size(12.0).color(p.fg)));
                ui.label(egui::RichText::new(format!("{} in · {} out", fmt_tokens(*input as f64), fmt_tokens(*output as f64))).size(11.5).color(p.muted));
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(name).size(12.5).color(p.fg)).truncate()).on_hover_text(name);
                });
            });
        });
    }
    let hidden = st.keys.len() - shown;
    if hidden > 0 || *more && st.keys.len() > 6 {
        let text = if *more { "Show less".to_string() } else { format!("Show {hidden} more") };
        if ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(p.subtle)).sense(egui::Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            *more = !*more;
        }
    }
}

// ---------------------------------------------------------------- popup window

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Usage,
    Analytics,
    Settings,
}

/// A row of options in a sunken track, the chosen one raised. Returns whether the choice changed.
fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, cur: &mut T, items: &[(T, &str)]) -> bool {
    let p = crate::pal(ui);
    let font = egui::FontId::proportional(11.5);
    let galleys: Vec<_> = items.iter().map(|(_, t)| ui.painter().layout_no_wrap(t.to_string(), font.clone(), p.fg)).collect();
    let pad = 9.0;
    let w: f32 = galleys.iter().map(|g| g.size().x + 2.0 * pad).sum::<f32>() + 4.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 24.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 6.0, p.sunken);
    let mut x = rect.left() + 2.0;
    let mut changed = false;
    for ((v, _), g) in items.iter().zip(galleys) {
        let seg = egui::Rect::from_min_size(egui::pos2(x, rect.top() + 2.0), egui::vec2(g.size().x + 2.0 * pad, rect.height() - 4.0));
        let resp = ui.interact(seg, ui.id().with(("seg", x as i32)), egui::Sense::click());
        let on = *cur == *v;
        if on {
            ui.painter().rect_filled(seg, 5.0, p.hover);
        }
        let color = if on || resp.hovered() { p.fg } else { p.muted };
        ui.painter().galley_with_override_text_color(seg.center() - g.size() / 2.0, g, color);
        if resp.clicked() && !on {
            *cur = *v;
            changed = true;
        }
        x = seg.right();
    }
    changed
}

/// Height of `display_block`: its gap, heading and three rows.
const DISPLAY_H: f32 = 14.0 + 20.0 + 3.0 * 30.0 + 4.0;

/// One reorder chip: the name between two chevrons; returns -1 / 1 when the left / right one is clicked.
fn chip(ui: &mut egui::Ui, name: &str, left: bool, right: bool) -> i32 {
    let p = crate::pal(ui);
    let g = ui.painter().layout_no_wrap(name.to_string(), egui::FontId::proportional(11.5), p.fg);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(g.size().x + 28.0, 24.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 6.0, p.sunken);
    ui.painter().galley_with_override_text_color(rect.center() - g.size() / 2.0, g, p.fg);
    let mut moved = 0;
    for (dir, on) in [(-1, left), (1, right)] {
        let zone = egui::Rect::from_min_size(egui::pos2(if dir < 0 { rect.left() } else { rect.right() - 13.0 }, rect.top()), egui::vec2(13.0, 24.0));
        let resp = ui.interact(zone, ui.id().with((name, dir)), egui::Sense::click());
        let color = if !on { p.subtle.gamma_multiply(0.4) } else if resp.hovered() { p.fg } else { p.muted };
        let (c, d) = (zone.center(), dir as f32 * 2.5);
        ui.painter().add(egui::Shape::line(vec![egui::pos2(c.x - d, c.y - 4.0), egui::pos2(c.x + d, c.y), egui::pos2(c.x - d, c.y + 4.0)], egui::Stroke::new(1.5, color)));
        if on && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            moved = dir;
        }
    }
    moved
}

/// A row's name in a fixed column, so the controls line up.
fn label(ui: &mut egui::Ui, text: &str) {
    let p = crate::pal(ui);
    ui.allocate_ui(egui::vec2(66.0, 24.0), |ui| {
        ui.set_min_width(66.0);
        ui.label(egui::RichText::new(text).size(11.5).color(p.muted));
    });
}

/// The bottom of Settings: which usage sources the Usage tab and the tray dial show, and in what order.
fn display_block(ui: &mut egui::Ui, cfg: &mut crate::Config) {
    use crate::ViewMode::{All, Follow};
    let p = crate::pal(ui);
    ui.spacing_mut().item_spacing.y = 0.0;
    ui.add_space(14.0);
    row(ui, 20.0, |ui| ui.label(egui::RichText::new("Display").family(crate::anthropic()).size(14.0).color(p.fg)));
    let modes = [(All, "All"), (Follow, "Follow profile")];
    for (name, mode) in [("Usage view", &mut cfg.usage_view_mode), ("Tray icon", &mut cfg.tray_mode)] {
        ui.push_id(name, |ui| {
            row(ui, 30.0, |ui| {
                label(ui, name);
                segmented(ui, mode, &modes);
            })
        });
    }
    row(ui, 30.0, |ui| {
        label(ui, "Order");
        ui.spacing_mut().item_spacing.x = 4.0;
        let mut list = ordered(&cfg.source_order);
        let last = list.len() - 1;
        let mut swap = None;
        for (i, s) in list.iter().enumerate() {
            let m = chip(ui, s.name(), i > 0, i < last);
            if m != 0 {
                swap = Some((i, (i as i32 + m) as usize));
            }
        }
        if let Some((a, b)) = swap {
            list.swap(a, b);
            cfg.source_order = list.iter().map(|s| s.name().to_string()).collect();
        }
    });
}

/// Settings has two views: the tier pickers, and the providers behind them.
#[derive(Clone, Copy, PartialEq)]
enum Sub {
    Models,
    Providers,
}

struct Popup {
    cfg: crate::Config,
    loaded: Value,
    usage: Usage,
    stamp: Option<std::time::SystemTime>,
    claude: bool,
    busy: [std::sync::Arc<std::sync::atomic::AtomicBool>; 3],
    anchor: (f32, f32, f32),
    focused_once: bool,
    /// The window's fade-in is off (done on the first frame, before eframe shows it).
    styled: bool,
    tab: Tab,
    sub: Sub,
    editor: crate::TierEditor,
    providers: crate::providers_ui::ProvidersEditor,
    /// Analytics: by model (else by provider), the shown data and the file stamp it was read at, "show more".
    by_model: bool,
    stats: Option<(Stats, bool, Option<std::time::SystemTime>)>,
    more: bool,
    /// Content height measured last frame, for the tabs whose height isn't known up front.
    measured: f32,
    /// The header's icon, made on the first frame.
    icon: Option<egui::TextureHandle>,
}

impl Popup {
    /// A refresh in the background; `which` 0 = Claude, 1 = Antigravity, 2 = ChatGPT, 3 = whatever is due (on opening).
    fn reload(&self, which: usize, ctx: &egui::Context) {
        let flag = (which < 3).then(|| self.busy[which].clone());
        if flag.as_ref().is_some_and(|f| f.swap(true, std::sync::atomic::Ordering::SeqCst)) {
            return;
        }
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            refresh(which == 0, which == 1, which == 2);
            if let Some(f) = flag {
                f.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            ctx.request_repaint();
        });
    }

    /// Re-read `tokens.jsonl` when it changed or the grouping did.
    fn stats(&mut self) -> &Stats {
        let stamp = fs::metadata(tokens_path()).and_then(|m| m.modified()).ok();
        if self.stats.as_ref().is_none_or(|(_, b, s)| *b != self.by_model || *s != stamp) {
            let text = fs::read_to_string(tokens_path()).unwrap_or_default();
            self.stats = Some((load_stats(&text, self.by_model, local_day(now())), self.by_model, stamp));
        }
        &self.stats.as_ref().unwrap().0
    }

    fn usage_tab(&mut self, ui: &mut egui::Ui, blocks: &[Section], reload: &mut Option<usize>) {
        let p = crate::pal(ui);
        let now = now();
        let accounts = ag_accounts(&self.cfg);
        let alias = |e: &str| e.split('@').next().unwrap_or(e).to_string();
        for (n, s) in blocks.iter().enumerate() {
            if n > 0 {
                rule(ui, SECTION_GAP);
            }
            row(ui, SECTION_H, |ui| {
                ui.label(egui::RichText::new(s.title).family(crate::anthropic()).size(15.0).color(p.fg));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.busy[s.which].load(std::sync::atomic::Ordering::SeqCst) {
                        ui.add_sized([28.0, 28.0], egui::Spinner::new().size(14.0));
                    } else if crate::icon_btn(ui, crate::Icon::Reload).on_hover_text("Reload now").clicked() {
                        *reload = Some(s.which);
                    }
                    if s.which != 1 {
                        let plan = if s.which == 0 { &self.usage.plan } else { &self.usage.chatgpt_plan };
                        ui.label(egui::RichText::new(plan).monospace().size(11.5).color(p.muted));
                    } else {
                        let before = self.cfg.usage_view.clone();
                        let view = &mut self.cfg.usage_view;
                        let shown = if view.is_empty() { "General".to_string() } else { alias(view) };
                        // Plain mono text like the Claude plan beside it; a click lists the accounts with their plans.
                        let at = ui.add(egui::Label::new(egui::RichText::new(shown).monospace().size(11.5).color(p.muted)).sense(egui::Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand);
                        egui::Popup::menu(&at).show(|ui| {
                            ui.set_min_width(210.0);
                            ui.spacing_mut().item_spacing.y = 1.0;
                            let mut picked = None;
                            if crate::pick_row(ui, view.is_empty(), "General", false, "").clicked() {
                                picked = Some(String::new());
                            }
                            for a in &accounts {
                                if crate::pick_row(ui, *view == a.email, &alias(&a.email), false, antigravity::plan_label(&a.plan)).on_hover_text(&a.email).clicked() {
                                    picked = Some(a.email.clone());
                                }
                            }
                            if let Some(v) = picked {
                                *view = v;
                                ui.close();
                            }
                        });
                        // Saved now, not on close: the tray closes the popup by ending its process.
                        if self.cfg.usage_view != before {
                            self.save();
                        }
                    }
                });
            });
            for e in &s.errors {
                // One line, cut to the window with the whole text on hover: a long message must not widen the window.
                row(ui, ERROR_H, |ui| ui.add(egui::Label::new(egui::RichText::new(e.replace(['\n', '\r'], " ")).size(12.0).color(p.danger)).truncate()).on_hover_text(e));
            }
            for (i, b) in s.bars.iter().enumerate() {
                // A group's pool line goes above its first bar.
                let first = |g: &str| s.bars.iter().position(|x| x.label.starts_with(&format!("{g} ("))) == Some(i);
                let pl = s.pools.iter().find(|pl| first(&pl.group));
                if let Some(pl) = pl {
                    ui.add_space(6.0);
                    row(ui, 18.0, |ui| ui.label(egui::RichText::new(format!("{} ready: {}", pl.group, pl.ready)).size(11.5).color(p.subtle)));
                }
                bar_ui(ui, b, now, s.stale, pl.map(|pl| pl.shown.as_str()));
            }
        }
        if blocks.is_empty() {
            ui.add(egui::Label::new(egui::RichText::new("Nothing to show: sign in to Claude Code, or add a Google account in ccat → Config.").size(11.5).color(p.subtle)).wrap());
        }
        // When each source last answered.
        rule(ui, 12.0);
        let fetched = |s: &Section| match s.which {
            0 => self.usage.claude.fetched,
            2 => self.usage.chatgpt.fetched,
            _ => accounts.iter().filter_map(|a| self.usage.antigravity.get(&a.email)).map(|x| x.fetched).max().unwrap_or(0),
        };
        let text = blocks.iter().map(|s| format!("{}: updated {}", s.title, ago(fetched(s), now))).collect::<Vec<_>>().join("   ·   ");
        row(ui, 18.0, |ui| ui.label(egui::RichText::new(text).size(11.0).color(p.subtle)));
    }

    fn analytics_tab(&mut self, ui: &mut egui::Ui) {
        let p = crate::pal(ui);
        ui.add_space(8.0);
        row(ui, 26.0, |ui| {
            segmented(ui, &mut self.by_model, &[(false, "Overview"), (true, "Models")]);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(egui::RichText::new(format!("Last {DAYS} days")).size(11.5).color(p.subtle)));
        });
        ui.add_space(8.0);
        let mut more = self.more;
        let st = self.stats();
        if st.keys.is_empty() {
            ui.add(egui::Label::new(egui::RichText::new("No tokens recorded yet. ccat counts the requests that go through its proxy: launches with a tier on another provider, or with a fallback.").size(11.5).color(p.subtle)).wrap());
            return;
        }
        chart(ui, st);
        ui.add_space(8.0);
        legend(ui, st, &mut more);
        self.more = more;
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui, win: &eframe::Frame) {
        let p = crate::pal(ui);
        ui.add_space(10.0);
        if self.sub == Sub::Providers {
            self.editor.pick = None; // a picker left open would keep Esc from closing the window
            row(ui, 26.0, |ui| {
                segmented(ui, &mut self.sub, &[(Sub::Models, "Models"), (Sub::Providers, "Providers")]);
            });
            ui.add_space(8.0);
            // The window grows with the list up to the screen, then the list scrolls.
            let room = self.anchor.1 - self.anchor.2 - 2.0 * PAD - HEADER_H - 26.0 - 8.0 - 10.0;
            self.providers.show(ui, &mut self.cfg, (room - DISPLAY_H).max(120.0));
            display_block(ui, &mut self.cfg);
            return;
        }
        let sub = &mut self.sub;
        self.editor.bar.show(ui, &mut self.cfg, win, |ui| {
            segmented(ui, sub, &[(Sub::Models, "Models"), (Sub::Providers, "Providers")]);
        });
        ui.add_space(6.0);
        ui.spacing_mut().item_spacing.y = 4.0;
        // With every tier and fallback shown the list can outgrow the screen: it scrolls, Display stays below it.
        let room = ui.ctx().content_rect().top() + (self.anchor.1 - self.anchor.2) - PAD - ui.cursor().top() - DISPLAY_H;
        egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(room.max(120.0)).show(ui, |ui| {
            self.editor.rows(ui, &mut self.cfg);
            ui.add_space(6.0);
            ui.add(egui::Label::new(egui::RichText::new("Changes reach running ccat sessions within seconds, and every next launch. Claude names and context windows stay as each session started. A session launched with every tier on Claude and no fallback runs plain claude: relaunch it to switch.").size(11.0).color(p.subtle)).wrap());
        });
        display_block(ui, &mut self.cfg);
    }

    fn save(&mut self) {
        crate::profiles::sync(&crate::profiles::dir(), &self.cfg);
        let _ = crate::save(&self.cfg, &self.loaded);
        // What's on disk now: the next save only writes what changes after this.
        self.loaded = serde_json::to_value(&self.cfg).unwrap_or_default();
    }
}

impl eframe::App for Popup {
    fn ui(&mut self, ui: &mut egui::Ui, win: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let p = crate::pal(ui);
        // The other process (or our reload) wrote new data.
        let stamp = usage_stamp();
        if stamp != self.stamp {
            (self.stamp, self.usage) = (stamp, load_usage());
        }
        ctx.request_repaint_after(Duration::from_secs(1));
        #[cfg(windows)]
        if !self.styled {
            self.styled = true;
            win::no_transitions("ccat usages");
        }
        let focused = ui.input(|i| i.viewport().focused);
        self.focused_once |= focused == Some(true);
        let esc = !self.editor.active() && !self.providers.active() && ui.input(|i| i.key_pressed(egui::Key::Escape));
        // Our own file dialog (Import) takes focus, and gives it back a moment after it closes; so does the browser
        // while a Google account is being added.
        if esc || (self.focused_once && focused == Some(false) && !self.editor.bar.recent_dialog() && !self.providers.signing_in()) {
            ui.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        let blocks = sections(&self.cfg, &self.usage, self.claude);
        // Usage knows its height up front; the other tabs take last frame's. The bottom edge stays put.
        let h = match self.tab {
            Tab::Usage => height(&blocks),
            Tab::Settings if self.editor.active() => self.measured.max(470.0),
            _ => self.measured,
        };
        let h = h.min(self.anchor.1 - self.anchor.2).max(120.0);
        if let Some(r) = ui.input(|i| i.viewport().inner_rect) {
            if (r.height() - h).abs() > 0.5 {
                ui.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(POPUP_W, h)));
            }
            let want = egui::pos2(self.anchor.0, (self.anchor.1 - h).max(self.anchor.2));
            if ui.input(|i| i.viewport().outer_rect).is_some_and(|o| (o.min - want).length() > 1.0) {
                ui.send_viewport_cmd(egui::ViewportCommand::OuterPosition(want));
            }
        }

        let mut reload = None;
        let snapshot = |c: &crate::Config| (c.tiers.clone(), c.profile.clone(), c.claude_off, serde_json::to_value(&c.providers).unwrap_or_default(), (c.usage_view_mode, c.tray_mode, c.source_order.clone()));
        let before = snapshot(&self.cfg);
        self.providers.poll(&mut self.cfg);
        let icon = self.icon.get_or_insert_with(|| icon_texture(&ctx)).clone();
        let frame = egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::same(PAD as i8));
        let measured = egui::CentralPanel::default()
            .frame(frame)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                // No close button: a click anywhere else (or Esc) closes it.
                row(ui, HEADER_H, |ui| {
                    ui.image((icon.id(), egui::vec2(ICON_H, ICON_H)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        segmented(ui, &mut self.tab, &[(Tab::Settings, "Settings"), (Tab::Analytics, "Analytics"), (Tab::Usage, "Usage")]);
                    });
                });
                let body = ui.vertical(|ui| match self.tab {
                    Tab::Usage => self.usage_tab(ui, &blocks, &mut reload),
                    Tab::Analytics => self.analytics_tab(ui),
                    Tab::Settings => self.settings_tab(ui, win),
                });
                HEADER_H + body.response.rect.height()
            })
            .inner;
        let measured = (measured + 2.0 * PAD).ceil();
        if (measured - self.measured).abs() > 0.5 {
            self.measured = measured;
            ctx.request_repaint();
        }
        if snapshot(&self.cfg) != before {
            self.save(); // running sessions follow the config file
        }
        ui.ctx()
            .layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("outline")))
            .rect_stroke(ui.ctx().content_rect(), 0.0, egui::Stroke::new(1.0, p.border_strong), egui::StrokeKind::Inside);
        if let Some(w) = reload {
            self.reload(w, &ctx);
        }
    }
}

/// The usage window: opened by the tray, closes when it loses focus.
pub fn run_popup(click: (f64, f64)) {
    let (mut cfg, mut loaded) = crate::load();
    // First open after the update: the current tiers become the `Default` configuration, saved right away.
    if crate::profiles::ensure(&crate::profiles::dir(), &mut cfg) {
        let _ = crate::save(&cfg, &loaded);
        loaded = serde_json::to_value(&cfg).unwrap_or_default();
    }
    let (usage, claude) = (load_usage(), claude_set_up());
    let anchor = anchor(click);
    // Its real size from the start: the window opens where it ends up.
    let h = height(&sections(&cfg, &usage, claude));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ccat usages")
            .with_icon(crate::app_icon())
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top()
            .with_taskbar(false)
            .with_active(true)
            .with_inner_size([POPUP_W, h])
            .with_position(egui::pos2(anchor.0, (anchor.1 - h).max(anchor.2))),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "ccat usages",
        options,
        Box::new(move |cc| {
            crate::setup(&cc.egui_ctx);
            let app = Popup {
                cfg,
                loaded,
                usage,
                stamp: usage_stamp(),
                claude,
                busy: Default::default(),
                anchor,
                focused_once: false,
                styled: false,
                tab: Tab::Usage,
                sub: Sub::Models,
                editor: Default::default(),
                providers: Default::default(),
                by_model: true,
                stats: None,
                more: false,
                measured: h,
                icon: None,
            };
            app.reload(3, &cc.egui_ctx);
            Ok(Box::new(app))
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn icon_shrinks_by_averaging_and_keeps_transparency() {
        // 4x4: opaque white on the left two pixels, transparent (with junk colour) on the right two.
        let px = |c: [u8; 4]| c;
        let rgba: Vec<u8> = [px([255, 255, 255, 255]), px([255, 255, 255, 255]), px([9, 9, 9, 0]), px([9, 9, 9, 0])].repeat(4).concat();
        let img = shrink(&rgba, 4, 4, 2);
        assert_eq!(img.pixels, [egui::Color32::WHITE, egui::Color32::TRANSPARENT].repeat(2), "no junk bleeds from the transparent side");
        let half = shrink(&[255, 255, 255, 255, 0, 0, 0, 0], 2, 1, 1);
        assert_eq!(half.pixels[0], egui::Color32::from_rgba_premultiplied(128, 128, 128, 128));
    }

    #[test]
    fn times_parse_with_offsets_and_fractions() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), 0);
        assert_eq!(parse_time("2026-10-19T12:38:11Z"), 1_792_413_491);
        assert_eq!(parse_time("2026-10-19T11:50:00.349366+00:00"), 1_792_410_600);
        assert_eq!(parse_time("2026-10-19T18:50:00+07:00"), 1_792_410_600, "an offset is taken back out");
        assert_eq!(parse_time(""), 0);
        assert_eq!(parse_time("garbage"), 0);
    }

    #[test]
    fn reset_text_switches_to_a_weekday_after_a_day() {
        let now = 1_792_400_000;
        assert!(reset_text(now + 3 * 3600 + 30 * 60, now).starts_with("Resets in 3h 30m ("));
        assert!(reset_text(now + 3 * 86_400, now).starts_with("Resets on "));
        assert_eq!(reset_text(0, now), "Starts when used");
        assert_eq!(utc_parts(0), (4, 0, 0), "1970-01-01 was a Thursday");
    }

    #[test]
    fn claude_limits_become_bars() {
        // The Team account's real answer (Oct 2026): session, weekly, and a weekly limit scoped to Fable.
        let v = json!({ "five_hour": { "utilization": 39.0, "resets_at": "2026-10-19T11:50:00.349366+00:00" }, "limits": [
            { "kind": "session", "group": "session", "percent": 39, "resets_at": "2026-10-19T11:50:00.349366+00:00", "scope": null },
            { "kind": "weekly_all", "group": "weekly", "percent": 35, "resets_at": "2026-10-24T21:00:00.349441+00:00", "scope": null },
            { "kind": "weekly_scoped", "group": "weekly", "percent": 0, "resets_at": "2026-10-24T21:00:00.349681+00:00",
              "scope": { "model": { "id": null, "display_name": "Fable" }, "surface": null } } ] });
        let bars = parse_claude(&v);
        let labels: Vec<&str> = bars.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["Session (5hr)", "Weekly (7 day)", "Weekly Fable"]);
        assert_eq!((bars[0].used, bars[0].window, bars[0].resets), (39.0, FIVE_HOURS, 1_792_410_600));
        let old = parse_claude(&json!({ "five_hour": { "utilization": 48.0, "resets_at": null }, "seven_day": { "utilization": 64.0 } }));
        assert_eq!(old.iter().map(|b| b.used).collect::<Vec<_>>(), [48.0, 64.0], "older answers without `limits`");
        assert_eq!(plan_name("team", "default_claude_max_5x"), "Team");
        assert_eq!(plan_name("max", "default_claude_max_20x"), "Max 20x");
    }

    fn ag_summary(gemini_5h: f64, gemini_week: f64) -> Value {
        json!({ "groups": [
            { "displayName": "Gemini Models", "buckets": [
                { "bucketId": "gemini-weekly", "remainingFraction": gemini_week, "resetTime": "2026-10-24T10:51:27Z", "window": "weekly" },
                { "bucketId": "gemini-5h", "remainingFraction": gemini_5h, "resetTime": "2026-10-19T12:38:11Z", "window": "5h" } ] },
            { "displayName": "Claude and GPT models", "buckets": [
                { "bucketId": "3p-weekly", "remainingFraction": 1, "resetTime": "2026-10-26T09:07:02Z", "window": "weekly" },
                { "bucketId": "3p-5h", "remainingFraction": 1, "resetTime": "2026-10-19T14:07:02Z", "window": "5h" } ] } ] })
    }

    #[test]
    fn wham_usage_becomes_session_and_weekly_bars() {
        let v: Value = serde_json::from_str(r#"{ "plan_type": "plus", "rate_limit": { "allowed": true, "limit_reached": false,
            "primary_window": { "used_percent": 12, "limit_window_seconds": 18000, "reset_after_seconds": 3000, "reset_at": 1792303000 },
            "secondary_window": { "used_percent": 40.5, "limit_window_seconds": 604800, "reset_after_seconds": 90000 } }, "credits": { "has_credits": false } }"#).unwrap();
        let (plan, bars) = parse_wham(&v, 1_792_300_000);
        assert_eq!(plan, "Plus");
        assert_eq!(bars, [
            Bar { label: "Session (5hr)".into(), used: 12.0, resets: 1_792_303_000, window: 18_000 },
            Bar { label: "Weekly (7 day)".into(), used: 40.5, resets: 1_792_390_000, window: 604_800 },
        ], "a missing reset_at falls back to now + reset_after_seconds");
        assert!(parse_wham(&serde_json::json!({}), 0).1.is_empty());

        let cfg = crate::Config::default();
        let mut u = Usage { chatgpt_on: true, chatgpt_plan: plan, chatgpt: Source { bars, ..Default::default() }, ..Default::default() };
        let s = sections(&cfg, &u, true);
        assert_eq!(s.iter().map(|s| s.title).collect::<Vec<_>>(), ["Claude", "ChatGPT"], "between Claude and Antigravity");
        u.chatgpt_on = false;
        assert!(sections(&cfg, &u, false).is_empty(), "no Codex login: no section");
    }

    #[test]
    fn antigravity_groups_become_bars() {
        let a = parse_ag(&ag_summary(0.98, 0.73));
        let labels: Vec<&str> = a.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["Gemini (5hr)", "Gemini (Weekly)", "Claude (5hr)", "Claude (Weekly)"]);
        assert!((a[1].used - 27.0).abs() < 0.01);

    }

    #[test]
    fn failed_fetches_keep_their_last_bars_greyed_out() {
        let acct = |e: &str| antigravity::Account { email: e.into(), ..Default::default() };
        let ag = crate::Provider { name: "Antigravity".into(), antigravity: true, accounts: vec![acct("a@x"), acct("b@x")], ..Default::default() };
        let cfg = crate::Config { providers: vec![ag], ..Default::default() };
        let bars = parse_ag(&ag_summary(0.5, 0.5));
        let src = |err: &str| Source { bars: bars.clone(), error: err.into(), ..Default::default() };
        let mut u = Usage::default();
        u.antigravity.insert("a@x".into(), src(""));
        u.antigravity.insert("b@x".into(), src("boom"));
        let s = &sections(&cfg, &u, false)[0];
        assert!(!s.stale && s.pools.is_empty() && !s.bars.is_empty(), "one account answered: its bars, normal, no pool line");

        u.antigravity.insert("a@x".into(), src(""));
        u.antigravity.insert("b@x".into(), src(""));
        assert_eq!(sections(&cfg, &u, false)[0].pools.len(), 2, "two accounts: a pool line per group");

        u.antigravity.insert("a@x".into(), src("down"));
        u.antigravity.insert("b@x".into(), src("down"));
        let s = &sections(&cfg, &u, false)[0];
        assert!(s.stale && !s.bars.is_empty(), "every account failed: the last answers stay, greyed out");

        u.claude = src("nope");
        let s = &sections(&cfg, &u, true)[0];
        assert!(s.stale && !s.bars.is_empty());
    }

    #[test]
    fn dial_puts_claude_on_top_and_takes_the_whole_icon_alone() {
        let u = Usage { claude: Source { bars: parse_claude(&json!({ "limits": [
            { "kind": "session", "percent": 39, "resets_at": null }, { "kind": "weekly_all", "percent": 35, "resets_at": null } ] })), ..Default::default() }, ..Default::default() };
        let ag = parse_ag(&ag_summary(0.5, 0.9));
        let five = |d: &Option<Dial>| d.as_ref().and_then(|d| d.five.as_ref()).map(|b| b.used.round());
        let week = |d: &Option<Dial>| d.as_ref().and_then(|d| d.week.as_ref()).map(|b| b.used.round());
        let d = icon_dials(&u, &[Src::Claude, Src::Antigravity], &ag);
        let (top, bottom) = (d.first().cloned(), d.get(1).cloned());
        assert_eq!((five(&top), week(&top), five(&bottom), week(&bottom)), (Some(39.0), Some(35.0), Some(50.0), Some(10.0)), "Claude above, Antigravity below");
        let d = icon_dials(&Usage::default(), &[Src::Antigravity], &ag);
        assert_eq!((five(&d.first().cloned()), d.len()), (Some(50.0), 1), "Antigravity alone takes the whole dial");
    }

    #[test]
    fn general_shows_the_account_with_the_most_left_and_who_is_ready() {
        // The live mix: free accounts out for the week, a Pro out for the week despite an unused 5 hours, a Pro with a
        // quarter of its 5 hours left; Claude out everywhere.
        let bar = |label: &str, used, resets, window| Bar { label: label.into(), used, resets, window };
        let acct = |e: &str, plan: &str| antigravity::Account { email: e.into(), plan: plan.into(), ..Default::default() };
        let src = |bars: Vec<Bar>| Source { bars, ..Default::default() };
        let (h, d) = (3600, 24 * 3600);
        let free = src(vec![bar("Gemini (Weekly)", 100.0, 165 * h, WEEK), bar("Claude (Weekly)", 99.96, 167 * h, WEEK)]);
        let pro_a = src(vec![bar("Gemini (5hr)", 2.7, 2 * h, FIVE_HOURS), bar("Gemini (Weekly)", 100.0, 74 * h, WEEK), bar("Claude (5hr)", 100.0, 4 * h, FIVE_HOURS), bar("Claude (Weekly)", 99.96, 167 * h, WEEK)]);
        let pro_b = src(vec![bar("Gemini (5hr)", 75.5, 4 * h, FIVE_HOURS), bar("Gemini (Weekly)", 72.7, 6 * d, WEEK), bar("Claude (5hr)", 100.0, 4 * h, FIVE_HOURS), bar("Claude (Weekly)", 100.0, 154 * h, WEEK)]);
        let (f1, f2, a, b) = (acct("f1@x", "free-tier"), acct("f2@x", "free-tier"), acct("pa@x", "g1-pro-tier"), acct("pb@x", "g1-pro-tier"));
        let (bars, pools) = pool(&[(&f1, &free), (&f2, &free), (&a, &pro_a), (&b, &pro_b)]);
        let used: Vec<(&str, f64)> = bars.iter().map(|b| (b.label.as_str(), b.used)).collect();
        assert_eq!(used, [("Gemini (5hr)", 75.5), ("Gemini (Weekly)", 72.7), ("Claude (5hr)", 100.0), ("Claude (Weekly)", 100.0)], "Gemini: pb's own buckets, not an average");
        assert_eq!(pools[0], Pool { group: "Gemini".into(), ready: "Free 0/2 · Pro 1/2".into(), shown: "pb (Pro)".into() }, "pa's spare 5 hours don't count: its week is used up");
        assert_eq!(pools[1].ready, "Free 0/2 · Pro 0/2");
        assert_eq!((pools[1].shown.as_str(), bars[3].resets), ("pb (Pro)", 154 * h), "none ready: the one back soonest, by its weekly reset, not the 5-hour one");
        let (one, none) = pool(&[(&f1, &free)]);
        assert!(one.iter().all(|b| b.window == WEEK) && none.is_empty(), "one account: its own buckets, no pool line");
    }

    #[test]
    fn colors_follow_pace_and_limits() {
        let now = 1_000_000;
        let bar = |used, left: u64| Bar { label: "x".into(), used, resets: now + left, window: FIVE_HOURS };
        assert_eq!(bar(30.0, FIVE_HOURS / 2).level(now), Level::Ok, "30% used, half the time gone");
        assert_eq!(bar(60.0, FIVE_HOURS / 2).level(now), Level::Warn, "ahead of the time passed");
        assert_eq!(bar(92.0, 60).level(now), Level::Warn, "past 90% even though on pace");
        assert_eq!(bar(100.0, 60).level(now), Level::Full, "red only when used up");
        assert_eq!((ago(0, now), ago(now - 5, now), ago(now - 125, now)), ("never".into(), "just now".into(), "2m ago".into()));
    }

    #[test]
    fn dial_arcs_fill_from_the_left() {
        let bar = |used, window| Some(Bar { label: "x".into(), used, resets: 0, window });
        let dial = |five, week| Dial { five: bar(five, FIVE_HOURS), week: bar(week, WEEK) };
        let size = 32;
        let px = icon_pixels(size, &[dial(30.0, 50.0), dial(20.0, 100.0)], false, 0);
        let alpha = |x: u32, y: u32| px[((y * size + x) * 4 + 3) as usize];
        // On each arc's centre line: just past its left end, the top (12 o'clock), just short of its right end.
        let r = 15.5 - (32.0 * 0.13) / 2.0;
        let at = |deg: f32| {
            let a = deg.to_radians();
            ((16.0 + r * a.cos()) as u32, (16.0 - r * a.sin()) as u32)
        };
        let (l, top, rt) = (at(150.0), at(90.0), at(30.0));
        assert_eq!(alpha(l.0, l.1), 255, "50% weekly: the left end is filled");
        assert_eq!(alpha(rt.0, rt.1), 80, "…the right end is still track");
        assert!(alpha(top.0, top.1) >= 80);
        let b = at(-30.0);
        assert_eq!(alpha(b.0, b.1), 255, "100% weekly: the bottom arc is full");
        assert_eq!((alpha(0, 16), alpha(31, 16), alpha(0, 15), alpha(31, 15)), (0, 0, 0, 0), "a blank band across the middle");
        // The pies, halfway out: 30% of the top half from the left, 20% of the bottom one.
        let pie_at = |deg: f32| {
            let a = deg.to_radians();
            ((16.0 + 6.0 * a.cos()) as u32, (16.0 - 6.0 * a.sin()) as u32)
        };
        let (a, b) = (pie_at(160.0), pie_at(60.0));
        assert_eq!((alpha(a.0, a.1), alpha(b.0, b.1)), (255, 80), "top pie: filled near the left, track further round");
        let (a, b) = (pie_at(-155.0), pie_at(-90.0));
        assert_eq!((alpha(a.0, a.1), alpha(b.0, b.1)), (255, 80), "bottom pie the same way");
        assert_eq!(tooltip(&Usage::default(), &[], &[]), "ccat usage");
    }

    #[test]
    fn three_sources_split_the_dial_into_sectors() {
        let bar = |used, window| Some(Bar { label: "x".into(), used, resets: 0, window });
        let dial = |five, week| Dial { five: bar(five, FIVE_HOURS), week: bar(week, WEEK) };
        let size = 32;
        let px = icon_pixels(size, &[dial(50.0, 50.0), dial(0.0, 0.0), dial(100.0, 100.0)], false, 0);
        let alpha = |x: u32, y: u32| px[((y * size + x) * 4 + 3) as usize];
        let (r, pr) = (15.5 - 32.0 * 0.13 / 2.0, 6.0);
        let at = |rad: f32, deg: f32| {
            let a = deg.to_radians();
            ((16.0 + rad * a.cos()) as u32, (16.0 - rad * a.sin()) as u32)
        };
        // Sector 0 runs clockwise from 150° (50%: filled to 90°, track after); sector 1 from 30°, empty; sector 2 from -90°, full.
        let near = |rad, deg| alpha(at(rad, deg).0, at(rad, deg).1);
        assert_eq!((near(r, 120.0), near(r, 60.0)), (255, 80), "weekly arc of the first sector: filled near its start, track near its end");
        assert_eq!((near(pr, 110.0), near(pr, 70.0)), (255, 80), "…and its pie");
        assert_eq!((near(r, -30.0), near(pr, -30.0), near(r, 0.0)), (80, 80, 80), "an empty source is all track");
        assert_eq!((near(r, -120.0), near(pr, -170.0), near(pr, 195.0)), (255, 255, 255), "a full one is solid to its end edge");
        for deg in [150.0, 30.0, -90.0] {
            assert_eq!((near(r, deg), near(pr, deg)), (0, 0), "blank band at {deg}°");
        }
    }

    #[test]
    fn sources_follow_saved_order_and_the_profile() {
        let names = |v: Vec<Src>| v.iter().map(|s| s.name()).collect::<Vec<_>>();
        assert_eq!(names(ordered(&["Antigravity".into(), "bogus".into(), "Claude".into()])), ["Antigravity", "Claude", "ChatGPT"]);
        assert_eq!(names(ordered(&[])), ["Claude", "ChatGPT", "Antigravity"]);
        let gpt = crate::Provider { name: "ChatGPT".into(), chatgpt: true, ..Default::default() };
        let ag = crate::Provider { name: "Antigravity".into(), antigravity: true, ..Default::default() };
        let mut cfg = crate::Config { providers: vec![gpt, ag], ..Default::default() };
        cfg.tiers[0].provider = "ChatGPT".into();
        cfg.tiers[1].provider = "some-endpoint".into();
        cfg.tiers[1].fallbacks = vec![crate::Fallback { provider: "Antigravity".into(), model: String::new() }];
        cfg.tiers[2].provider = "some-endpoint".into();
        cfg.tiers[3].provider = "some-endpoint".into();
        assert_eq!(uses(&cfg), [false, true, true], "primaries and fallbacks count; endpoints and unused official don't");
        let all = [true; 3];
        assert_eq!(names(shown(&cfg, crate::ViewMode::All, all)), ["Claude", "ChatGPT", "Antigravity"]);
        assert_eq!(names(shown(&cfg, crate::ViewMode::Follow, all)), ["ChatGPT", "Antigravity"]);
        assert_eq!(names(shown(&cfg, crate::ViewMode::Follow, [true, false, false])), ["Claude"], "nothing left: back to all");
        cfg.tiers[3].provider = String::new();
        assert_eq!(names(shown(&cfg, crate::ViewMode::Follow, all)), ["Claude", "ChatGPT", "Antigravity"], "official Claude counts");
    }

    #[test]
    fn analytics_counts_per_day_and_formats_like_the_console() {
        assert_eq!((fmt_tokens(950.0), fmt_tokens(405_600.0), fmt_tokens(21_900_000.0), fmt_tokens(1.6e9), fmt_tokens(4e9)), ("950".into(), "405.6k".into(), "21.9M".into(), "1.6B".into(), "4B".into()));
        assert_eq!((pretty_model("claude-opus-5-5"), pretty_model("claude-haiku-4-5-20251001"), pretty_model("gemini-3.8-flash-tiered")), ("Opus 5.5".into(), "Haiku 4.5".into(), "gemini-3.8-flash-tiered".into()));
        assert_eq!(civil_from_days(civil_days(2026, 10, 4)), (2026, 10, 4));
        assert_eq!(day_label(civil_days(2026, 10, 4)), "Oct 4");
        let t = 1_792_400_000u64;
        let today = local_day(t);
        let line = |dt: u64, model: &str, provider: &str, i: u64, o: u64| json!({ "t": t - dt, "tier": "opus", "provider": provider, "model": model, "input": i, "output": o, "cache_read": 1000, "cache_write": 0 }).to_string();
        let text = [line(0, "claude-opus-5-5", "Claude", 10, 5), line(0, "gemini-x", "Antigravity", 1, 1), line(86_400, "claude-opus-5-5", "Claude", 0, 100), line(40 * 86_400, "old", "Claude", 9, 9), "garbage".into()].join("\n");
        let st = load_stats(&text, true, today);
        assert_eq!(st.keys.iter().map(|k| (k.0.as_str(), k.1, k.2)).collect::<Vec<_>>(), [("Opus 5.5", 2010, 105), ("gemini-x", 1001, 1)], "cache reads count as input; a 40-day-old line is out");
        assert_eq!(st.cells[&(DAYS - 1, 0)], 1015, "today's Opus: in + out");
        let by_provider = load_stats(&text, false, today);
        assert_eq!(by_provider.keys[0].0, "Claude");
    }


}
