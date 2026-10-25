//! Official Claude: the model list (models.dev) and the plan usage (Claude Code's own login, read only).

use crate::now;
use crate::usage::{dirs_home, parse_time, plan_name, Bar, FIVE_HOURS, WEEK};
use serde_json::Value;
use std::{collections::HashMap, fs, path::PathBuf, time::Duration};

/// Extract all model costs from models.dev and cache to pricing.json for the Live cost tab.
fn save_pricing_cache(v: &serde_json::Value) {
    let mut map: HashMap<&str, [f64; 4]> = HashMap::new();
    if let Some(obj) = v.as_object() {
        for prov in obj.values() {
            if let Some(models) = prov.get("models").and_then(|m| m.as_object()) {
                for (id, m) in models {
                    if let Some(cost) = m.get("cost").and_then(|c| c.as_object()) {
                        let num = |k: &str| cost.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                        map.insert(id.as_str(), [num("input"), num("output"), num("cache_read"), num("cache_write")]);
                    }
                }
            }
        }
    }
    if !map.is_empty() {
        if let Ok(json) = serde_json::to_string(&map) {
            let _ = fs::write(crate::config_path().with_file_name("pricing.json"), json);
        }
    }
}

/// Current Claude model ids, newest first, from models.dev (public, no key).
pub(crate) fn fetch_claude_models() -> Result<Vec<String>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).build().into();
    let body = agent.get("https://models.dev/api.json").call().map_err(|e| e.to_string())?.body_mut().with_config().limit(64 << 20).read_to_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    save_pricing_cache(&v);
    let mut models: Vec<(&str, &str)> = v["anthropic"]["models"]
        .as_object()
        .ok_or("models.dev has no anthropic models")?
        .iter()
        .filter(|(id, _)| id.starts_with("claude-"))
        .map(|(id, m)| (id.as_str(), m["release_date"].as_str().unwrap_or("")))
        .collect();
    models.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    Ok(models.into_iter().map(|(id, _)| id.to_string()).collect())
}

/// Claude Code's login as it left it: (access token, plan name). Read only: refreshing it here could race Claude
/// Code and sign it out, and while ccat runs Claude Code keeps it fresh itself.
pub(crate) fn claude_login() -> Option<Result<(String, String), String>> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| dirs_home().join(".claude"));
    let v: Value = serde_json::from_str(&fs::read_to_string(dir.join(".credentials.json")).ok()?).ok()?;
    let o = &v["claudeAiOauth"];
    let token = o["accessToken"].as_str().filter(|t| !t.is_empty())?.to_string();
    let plan = plan_name(o["subscriptionType"].as_str().unwrap_or(""), o["rateLimitTier"].as_str().unwrap_or(""));
    let expired = o["expiresAt"].as_u64().is_some_and(|ms| ms / 1000 <= now());
    Some(if expired { Err(format!("{plan}|Login expired: start Claude Code to refresh it.")) } else { Ok((token, plan)) })
}

/// Claude Code is signed in to a claude.ai plan (an API-key setup has no plan quota to show).
pub fn claude_set_up() -> bool {
    claude_login().is_some()
}

/// Bars from `/api/oauth/usage`: its `limits` list (session, weekly, per-model weekly), or the older top-level fields.
pub fn parse_claude(v: &Value) -> Vec<Bar> {
    let bar = |label: String, used: f64, resets: &Value, window| Bar { label, used, resets: resets.as_str().map_or(0, parse_time), window, ..Default::default() };
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

pub(crate) fn fetch_claude(token: &str) -> Result<Vec<Bar>, (String, u64)> {
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

// ---------------------------------------------------------------- settings page

/// Settings → Claude: Claude Code's own login can't be turned off, only offered (or not) as a choice; under it the
/// plan's usage limits.
pub(crate) fn page(ui: &mut eframe::egui::Ui, cfg: &mut crate::Config, feed: &crate::usage::Feed) {
    use crate::settings::providers::{head, name, note};
    let p = crate::pal(ui);
    head(ui, |ui| {
        let mut on = !cfg.claude_off;
        if crate::switch(ui, &mut on, "")
            .on_hover_text("Off: Claude models leave the model pickers (Default stays) and are skipped as fallbacks. A tier already on Claude keeps working.")
            .changed()
        {
            cfg.claude_off = !on;
        }
        name(ui, "Claude Code (Original)", on);
    });
    note(ui, "Claude Code's own login. Off: Claude models leave the model pickers (Default stays) and are skipped as fallbacks. A tier already on Claude keeps working.", p.subtle);
    ui.add_space(10.0);
    let u = &feed.usage;
    crate::settings::group(ui, |ui, g| {
        feed.analytics(ui, g, "Claude");
        if !u.plan.is_empty() || !u.claude.bars.is_empty() || !u.claude.error.is_empty() {
            g.item(ui, |ui| feed.limits(ui, 0, &u.plan, &crate::usage::tidy(&u.claude.bars, cfg.show_fable), u.claude.fetched, &u.claude.error));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
