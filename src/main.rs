//! ccat — pick a provider/folder in a small native window, then run `claude` in this terminal.
//! Overrides live only in the child process env; no Claude config file is read or written.
//! An Antigravity provider is served by an in-process proxy (see `proxy`), so ccat stays alive while claude runs.

mod common;
mod live;
mod profiles;
mod providers;
mod providers_ui;
mod proxy;
mod usage;

use providers::{antigravity, chatgpt, museai, ollama, Provider, AG_NAME, GPT_NAME, MUSE_NAME, OLLAMA_NAME};
use providers::claude::fetch_claude_models;
use providers::endpoint::fetch_models;

use eframe::egui::{
    self, vec2, Align, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Key, Layout,
    RichText, Sense, Stroke, StrokeKind, TextEdit, TextStyle, Theme, ViewportCommand,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

/// (UI label, routing key, env var) for each model tier Claude Code knows.
pub const TIERS: [(&str, &str, &str); 4] = [
    ("Fable", "fable", "ANTHROPIC_DEFAULT_FABLE_MODEL"),
    ("Opus", "opus", "ANTHROPIC_DEFAULT_OPUS_MODEL"),
    ("Sonnet", "sonnet", "ANTHROPIC_DEFAULT_SONNET_MODEL"),
    ("Haiku", "haiku", "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
];
/// How the official provider (Claude Code's own login, `Route::provider` "") shows in the UI.
const OFFICIAL: &str = "Claude";

/// Where one tier runs: a provider by name ("" = official Claude), its model ("" = Claude Code's own pick, official
/// only), and the Claude name Claude Code is told instead ("" = the real id). A `modelOverrides` entry sends the
/// real id, so Claude Code's catalog (context window, thinking…) applies to any model.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
struct Route {
    provider: String,
    model: String,
    claude_as: String,
    /// Where the tier's requests go, in order, when its own provider fails (down, out of quota) before anything
    /// has streamed.
    fallbacks: Vec<Fallback>,
    /// The single fallback of configs from before the chain; moved into `fallbacks` on load.
    #[serde(skip_serializing)]
    fallback: Option<Fallback>,
}

/// A tier's second choice: a provider by name ("" = official Claude) and its model.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
struct Fallback {
    provider: String,
    model: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Config {
    providers: Vec<Provider>,
    /// Fable, Opus, Sonnet, Haiku (TIERS order).
    tiers: [Route; 4],
    recents: Vec<String>,
    bypass: bool,
    /// Claude model ids to spoof as, newest first; refreshed from models.dev at most every `REFRESH_SECS`.
    claude_models: Vec<String>,
    /// Latest GitHub release tag, from the same refresh.
    latest: String,
    /// Unix time of that refresh.
    checked: u64,
    /// The usage window's Antigravity view: "" = all accounts combined, else one account's email.
    usage_view: String,
    /// The applied configuration (see `profiles`): the file `tiers` came from and is kept in step with.
    profile: String,
    /// Official Claude models are not offered in the pickers (but Default is) and are skipped as fallbacks.
    claude_off: bool,
    /// The "you're using your ChatGPT plan" note: 0 before the first ChatGPT sign-in, 1 showing, 2 dismissed.
    chatgpt_note: u8,
    /// Which usage sources the popup's Usage tab shows, and which the tray dial draws.
    usage_view_mode: ViewMode,
    tray_mode: ViewMode,
    /// Usage sources top to bottom in the popup and first to last on the dial (names; see `usage::ordered`).
    source_order: Vec<String>,
    /// A routed request whose every route is at its usage limit fails at once, instead of waiting (open) until one
    /// can run. Off by default: an agent pauses at a limit rather than stopping.
    fail_on_limit: bool,
    /// Routed requests never fall back to a custom endpoint (a paid API key); a tier's own primary still runs.
    /// Off by default.
    no_paid_fallback: bool,
    /// Routed requests to third-party models get their tool results cleaned (ANSI, progress redraws, repeated
    /// lines); never Claude. Off by default.
    compress_tool_results: bool,
    /// Usage: the Antigravity model group shown ("" = all), and Claude's per-model weekly bars (Fable) shown.
    ag_group: String,
    show_fable: bool,
    /// Analytics: the view last open (0 Models, 1 Overview, 2 Live); Live's range (an index into `usage::RANGES`),
    /// calls instead of tokens, grouping (0 total, 1 per model, 2 per provider) and running totals.
    analytics_view: u8,
    live_range: u8,
    #[serde(default)]
    live_calls: bool,
    #[serde(default)]
    live_metric: u8,
    live_by: u8,
    live_cumulative: bool,
}

/// Show every set-up usage source, or only those the applied profile routes to.
#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum ViewMode {
    #[default]
    All,
    Follow,
}

/// Offline fallback for `Config::claude_models`.
const CLAUDE_MODELS: [&str; 6] = ["claude-sonnet-5-5", "claude-opus-5-5", "claude-fable-5-1", "claude-opus-4-6", "claude-sonnet-4-6", "claude-haiku-4-5-20251001"];
/// models.dev (a multi-MB download) and GitHub (60 unauthenticated calls an hour) are asked at most this often,
/// so a row of ccat windows opened together costs one check, not one each.
const REFRESH_SECS: u64 = 6 * 3600;

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

const REPO_URL: &str = "https://github.com/duongnguyen16/ccat";
const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// Tag of the latest GitHub release, e.g. "v0.2.0" (public API, no key).
fn fetch_latest_release() -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().into();
    let body = agent
        .get("https://api.github.com/repos/duongnguyen16/ccat/releases/latest")
        .header("User-Agent", "ccat")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    v["tag_name"].as_str().map(String::from).ok_or_else(|| "the latest release has no tag".into())
}

/// `v1.10.0` > `1.9.3`: major.minor.patch compared as numbers; a missing or non-numeric part counts as 0.
fn is_newer(latest: &str, current: &str) -> bool {
    let parts = |s: &str| {
        let mut v: Vec<u64> = s.trim_start_matches('v').split(['.', '-', '+']).take(3).map(|x| x.parse().unwrap_or(0)).collect();
        v.resize(3, 0);
        v
    };
    parts(latest) > parts(current)
}

/// Claude models for tier `t`: its own family for spoofing (Fable rows only Fable…), every one for an official tier.
fn claude_choices(models: &[String], t: Option<usize>) -> Vec<String> {
    let all: Vec<String> = if models.is_empty() { CLAUDE_MODELS.map(String::from).to_vec() } else { models.to_vec() };
    all.into_iter().filter(|m| t.is_none_or(|t| m.contains(TIERS[t].1))).collect()
}

struct Launch {
    tiers: [Route; 4],
    providers: Vec<Provider>,
    claude_models: Vec<String>,
    claude_off: bool,
    folder: String,
    bypass: bool,
    fail_on_limit: bool,
    no_paid_fallback: bool,
    compress_tool_results: bool,
}

/// Whether a launch goes through ccat's local proxy: anything but every tier on Claude with no fallback. Plain
/// `claude` talks to Anthropic directly; a routed session can switch models and fall back while it runs.
fn routed(tiers: &[Route; 4]) -> bool {
    tiers.iter().any(|r| !r.provider.is_empty() || !r.fallbacks.is_empty())
}

/// Per tier: the model name Claude Code is told (its env var; "" = unset), and `modelOverrides` (Claude name →
/// the id it sends instead). Routed, a non-Claude id carries its tier (`gemini-x@sonnet`), so every tier sends a name
/// of its own and can be switched to another model while the session runs; the proxy swaps in the real one.
/// A Claude name already standing for another id is skipped (first tier wins; the UI flags the clash).
fn tier_names(tiers: &[Route; 4], routed: bool) -> ([String; 4], serde_json::Map<String, serde_json::Value>) {
    let mut names: [String; 4] = Default::default();
    let mut overrides = serde_json::Map::new();
    for (t, r) in tiers.iter().enumerate() {
        let real = bare_id(&r.model);
        // Claude Code strips a `[1m]` hint from a model name, but sends an override target verbatim.
        let id = real.split('[').next().unwrap_or("").trim().to_string();
        let target = if routed { format!("{id}@{}", TIERS[t].1) } else { id.clone() };
        let name = r.claude_as.trim().to_string();
        let spoof = !r.provider.is_empty() && !name.is_empty() && !id.is_empty() && overrides.get(&name).is_none_or(|v| *v == target);
        names[t] = if spoof {
            overrides.insert(name.clone(), target.into());
            name
        } else if routed && !r.provider.is_empty() && !id.is_empty() {
            // Keep a `[1m]` hint where Claude Code reads it: at the end.
            format!("{target}{}", real.find('[').map_or("", |i| &real[i..]))
        } else {
            real
        };
    }
    (names, overrides)
}

// ---------------------------------------------------------------- storage / network

fn user_config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("ccat").join("config.json")
}

#[cfg(not(test))]
pub fn config_path() -> PathBuf { user_config_path() }

#[cfg(test)]
pub fn config_path() -> PathBuf {
    // App::drop also saves settings; test launchers must never reach live accounts.
    std::env::temp_dir().join(format!("ccat-tests-{}", std::process::id())).join("ccat/config.json")
}

/// The model list prefixes ids with the vendor ("anthropic/…"); endpoints want the bare id.
pub fn bare_id(m: &str) -> String {
    let m = m.trim();
    m.strip_prefix("anthropic/").unwrap_or(m).to_string()
}

/// The config, and the file as it was read: `save` writes back only what this window changed.
fn load() -> (Config, serde_json::Value) {
    let path = config_path();
    let text = fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let raw: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| {
        // Don't let the save-on-exit overwrite a file we couldn't read.
        let bad = path.with_extension("json.bad");
        let _ = fs::rename(&path, &bad);
        eprintln!("ccat: {} is invalid ({e}); moved to {}", path.display(), bad.display());
        serde_json::Value::Null
    });
    let mut cfg: Config = serde_json::from_value(raw.clone()).unwrap_or_default();
    if raw.get("live_metric").is_none() && raw.get("live_calls").and_then(|v| v.as_bool()).unwrap_or(false) {
        cfg.live_metric = 1;
    }
    if raw.get("tiers").is_none() {
        cfg.tiers = migrate_tiers(&raw);
    }
    // ponytail: one-off migration of ids saved before bare_id existed; cheap, so it just runs every load.
    for p in &mut cfg.providers {
        for m in p.models.iter_mut().chain(p.custom.iter_mut()) {
            *m = bare_id(m);
        }
        p.models.dedup();
    }
    for r in &mut cfg.tiers {
        r.model = bare_id(&r.model);
        r.fallbacks.extend(r.fallback.take());
    }
    builtin_antigravity(&mut cfg);
    builtin_chatgpt(&mut cfg);
    builtin_ollama(&mut cfg);
    builtin_muse(&mut cfg);
    // Codex CLI's login is the ChatGPT account ccat shares (see `chatgpt::CODEX_CLI`).
    if let Some(g) = cfg.providers.iter_mut().find(|p| p.chatgpt) {
        chatgpt::sync_codex_cli(&mut g.chatgpt_accounts);
    }
    (cfg, raw)
}

/// Before per-tier routing, tiers lived in each provider and the launcher picked one provider for all of them:
/// that provider's tiers (an empty one meant "same as Default") become the routes. The Default tier itself is
/// gone (Claude Code picks its own default among the four); its Claude name moves to the tier of that family when
/// that tier ran the Default model without a name of its own.
fn migrate_tiers(raw: &serde_json::Value) -> [Route; 4] {
    let mut tiers: [Route; 4] = Default::default();
    let chosen = raw["provider"].as_str().unwrap_or("");
    let Some(p) = raw["providers"].as_array().into_iter().flatten().find(|p| !chosen.is_empty() && p["name"] == chosen) else { return tiers };
    let s = |k: &str| p[k].as_str().unwrap_or("").trim().to_string();
    for (t, r) in tiers.iter_mut().enumerate() {
        let model = Some(s(TIERS[t].1)).filter(|m| !m.is_empty()).unwrap_or_else(|| s("default"));
        if !model.is_empty() {
            *r = Route { provider: chosen.into(), model, claude_as: p["claude_as"][t + 1].as_str().unwrap_or("").into(), ..Default::default() };
        }
    }
    let default_as = p["claude_as"][0].as_str().unwrap_or("").trim();
    if let Some(t) = TIERS.iter().position(|(_, key, _)| default_as.contains(key)).filter(|&t| s(TIERS[t].1).is_empty() && tiers[t].claude_as.is_empty()) {
        tiers[t].claude_as = default_as.into();
    }
    tiers
}

/// Points every tier on provider `from` at `to`.
fn rename_routes(tiers: &mut [Route; 4], from: &str, to: &str) {
    for r in tiers.iter_mut().filter(|_| !from.is_empty()) {
        if r.provider == from {
            r.provider = to.into();
        }
        for f in r.fallbacks.iter_mut().filter(|f| f.provider == from) {
            f.provider = to.into();
        }
    }
}

/// A deleted provider: its tiers go back to Claude Code's pick (keeping their fallbacks), and no fallback uses it.
fn reset_routes(tiers: &mut [Route; 4], gone: &str) {
    for r in tiers.iter_mut() {
        if r.provider == gone {
            *r = Route { fallbacks: std::mem::take(&mut r.fallbacks), ..Default::default() };
        }
        r.fallbacks.retain(|f| f.provider != gone);
    }
}

/// Renames provider `i`; the tiers (applied and in every profile file) follow it.
fn rename_provider(cfg: &mut Config, dir: &Path, i: usize, to: &str) {
    let from = std::mem::replace(&mut cfg.providers[i].name, to.into());
    rename_routes(&mut cfg.tiers, &from, to);
    profiles::edit_all(dir, |t| rename_routes(t, &from, to));
}

/// Deletes provider `i`; its tiers and fallbacks are reset in the applied set and in every profile file.
fn delete_provider(cfg: &mut Config, dir: &Path, i: usize) {
    let gone = cfg.providers.remove(i).name;
    reset_routes(&mut cfg.tiers, &gone);
    profiles::edit_all(dir, |t| reset_routes(t, &gone));
}

/// The new name for provider `i`, trimmed — or why it can't have it. An empty name would read as the official login.
fn check_rename(providers: &[Provider], i: usize, name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        Err("Give it a name.".into())
    } else if name.eq_ignore_ascii_case(AG_NAME) || name.eq_ignore_ascii_case(GPT_NAME) {
        Err("That is a built-in provider's name.".into())
    } else if providers.iter().enumerate().any(|(n, p)| n != i && p.name.eq_ignore_ascii_case(name)) {
        Err("Another provider already has that name.".into())
    } else {
        Ok(name.into())
    }
}

/// Models a provider offers: fetched, plus saved custom ids the list doesn't have.
fn model_count(p: &Provider) -> usize {
    p.models.len() + p.custom.iter().filter(|c| !p.models.contains(c)).count()
}

/// Emails of the accounts turned off, for the proxy's pool.
fn off_accounts(providers: &[Provider]) -> HashSet<String> {
    providers.iter().flat_map(|p| &p.accounts).filter(|a| a.disabled).map(|a| a.email.clone()).collect()
}

/// Antigravity is one built-in profile, always first and never deleted — not a type any profile can switch to
/// (one stray click on the old Type box turned an OpenRouter profile into an empty Antigravity one).
/// Older configs: an Antigravity profile with a URL but no accounts was really an endpoint and goes back to being
/// one; the first with accounts becomes the built-in; any other's accounts merge into it.
fn builtin_antigravity(cfg: &mut Config) {
    for p in cfg.providers.iter_mut().filter(|p| p.antigravity && p.accounts.is_empty() && !p.url.trim().is_empty()) {
        p.antigravity = false;
    }
    let main = cfg.providers.iter().position(|p| p.antigravity && !p.accounts.is_empty()).or_else(|| cfg.providers.iter().position(|p| p.antigravity));
    let mut ag = main.map_or_else(|| Provider { antigravity: true, ..Default::default() }, |i| cfg.providers.remove(i));
    let old = std::mem::replace(&mut ag.name, AG_NAME.into());
    rename_routes(&mut cfg.tiers, &old, AG_NAME);
    let mut rest = Vec::new();
    for mut p in std::mem::take(&mut cfg.providers) {
        if !p.antigravity {
            if p.name == AG_NAME {
                p.name = format!("{AG_NAME} endpoint");
            }
            rest.push(p);
            continue;
        }
        for a in p.accounts {
            if !ag.accounts.iter().any(|x| x.email == a.email) {
                ag.accounts.push(a);
            }
        }
        rename_routes(&mut cfg.tiers, &p.name, AG_NAME);
    }
    cfg.providers = std::iter::once(ag).chain(rest).collect();
}

/// ChatGPT is the other built-in provider: exactly one, second in the list. An endpoint a user had named "ChatGPT" is
/// renamed (its tiers follow); any extra built-in's accounts merge into the one.
fn builtin_chatgpt(cfg: &mut Config) {
    // First, so the tiers still pointing at that endpoint aren't mistaken for the built-in's.
    for p in cfg.providers.iter_mut().filter(|p| !p.chatgpt && p.name == GPT_NAME) {
        let to = format!("{GPT_NAME} endpoint");
        rename_routes(&mut cfg.tiers, GPT_NAME, &to);
        p.name = to;
    }
    let main = cfg.providers.iter().position(|p| p.chatgpt);
    let mut gpt = main.map_or_else(|| Provider { chatgpt: true, ..Default::default() }, |i| cfg.providers.remove(i));
    let old = std::mem::replace(&mut gpt.name, GPT_NAME.into());
    rename_routes(&mut cfg.tiers, &old, GPT_NAME);
    let mut rest = Vec::new();
    for p in std::mem::take(&mut cfg.providers) {
        if p.chatgpt {
            for a in p.chatgpt_accounts {
                if !gpt.chatgpt_accounts.iter().any(|x| x.key == a.key) {
                    gpt.chatgpt_accounts.push(a);
                }
            }
            rename_routes(&mut cfg.tiers, &p.name, GPT_NAME);
            continue;
        }
        rest.push(p);
    }
    cfg.providers = rest;
    cfg.providers.insert(1.min(cfg.providers.len()), gpt);
}

/// Ollama is the local built-in provider: exactly one, third in the list. An endpoint a user had named "Ollama" is
/// renamed (its tiers follow).
fn builtin_ollama(cfg: &mut Config) {
    for p in cfg.providers.iter_mut().filter(|p| !p.ollama && p.name == OLLAMA_NAME) {
        let to = format!("{OLLAMA_NAME} endpoint");
        rename_routes(&mut cfg.tiers, OLLAMA_NAME, &to);
        p.name = to;
    }
    let main = cfg.providers.iter().position(|p| p.ollama);
    let mut ol = main.map_or_else(|| Provider {
        name: OLLAMA_NAME.into(),
        url: ollama::DEFAULT_URL.into(),
        ollama: true,
        ..Default::default()
    }, |i| cfg.providers.remove(i));
    if ol.url.trim().is_empty() {
        ol.url = ollama::DEFAULT_URL.into();
    }
    let old = std::mem::replace(&mut ol.name, OLLAMA_NAME.into());
    rename_routes(&mut cfg.tiers, &old, OLLAMA_NAME);
    let mut rest = Vec::new();
    for p in std::mem::take(&mut cfg.providers) {
        if p.ollama {
            rename_routes(&mut cfg.tiers, &p.name, OLLAMA_NAME);
            continue;
        }
        rest.push(p);
    }
    cfg.providers = rest;
    cfg.providers.insert(2.min(cfg.providers.len()), ol);
}

/// MuseAI is the Hatch Gateway VM built-in provider: exactly one, fourth in the list. An endpoint a user had named "MuseAI" is
/// renamed (its tiers follow).
fn builtin_muse(cfg: &mut Config) {
    for p in cfg.providers.iter_mut().filter(|p| !p.muse && p.name == MUSE_NAME) {
        let to = format!("{MUSE_NAME} endpoint");
        rename_routes(&mut cfg.tiers, MUSE_NAME, &to);
        p.name = to;
    }
    let main = cfg.providers.iter().position(|p| p.muse);
    let mut mu = main.map_or_else(|| Provider {
        name: MUSE_NAME.into(),
        url: museai::DEFAULT_URL.into(),
        muse: true,
        ..Default::default()
    }, |i| cfg.providers.remove(i));
    if mu.url.trim().is_empty() {
        mu.url = museai::DEFAULT_URL.into();
    }
    let old = std::mem::replace(&mut mu.name, MUSE_NAME.into());
    rename_routes(&mut cfg.tiers, &old, MUSE_NAME);
    let mut rest = Vec::new();
    for p in std::mem::take(&mut cfg.providers) {
        if p.muse {
            rename_routes(&mut cfg.tiers, &p.name, MUSE_NAME);
            continue;
        }
        rest.push(p);
    }
    cfg.providers = rest;
    cfg.providers.insert(3.min(cfg.providers.len()), mu);
}

/// Several ccat windows may be open at once, so the file is never written in place (a reader could catch it half
/// written and start empty) and only the top-level fields this window changed since `loaded` are written over
/// whatever another window saved meanwhile.
fn save(cfg: &Config, loaded: &serde_json::Value) -> Result<(), String> {
    let p = config_path();
    fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    let mine = serde_json::to_value(cfg).map_err(|e| e.to_string())?;
    let disk = fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str(&t).ok());
    let out = merged(disk, loaded, mine);
    let tmp = p.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(&out).unwrap()).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// `disk` (the file now; `None` if missing or unreadable) with the top-level fields of `mine` that differ from `loaded`.
fn merged(disk: Option<serde_json::Value>, loaded: &serde_json::Value, mine: serde_json::Value) -> serde_json::Value {
    let Some(mut out) = disk.filter(|v| v.is_object()) else { return mine };
    for (k, v) in mine.as_object().into_iter().flatten() {
        if loaded.get(k) != Some(v) {
            out[k] = v.clone();
        }
    }
    out
}

// ---------------------------------------------------------------- look: Claude Console palette, compact

struct Pal {
    bg: Color32,
    surface: Color32,
    hover: Color32,
    sunken: Color32,
    border: Color32,
    border_strong: Color32,
    fg: Color32,
    muted: Color32,
    subtle: Color32,
    accent: Color32,
    danger: Color32,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

const DARK: Pal = Pal {
    bg: hex(0x1a1a19), surface: hex(0x212120), hover: hex(0x2b2b29), sunken: hex(0x161615),
    border: hex(0x302f2d), border_strong: hex(0x454440), fg: hex(0xf0eee6), muted: hex(0xa8a59c),
    subtle: hex(0x8a8780), accent: hex(0xd97757), danger: hex(0xe5726a),
};
const LIGHT: Pal = Pal {
    bg: hex(0xffffff), surface: hex(0xffffff), hover: hex(0xedebe5), sunken: hex(0xfaf9f7),
    border: hex(0xe6e4de), border_strong: hex(0xd2cfc7), fg: hex(0x191817), muted: hex(0x63605a),
    subtle: hex(0x6e6b63), accent: hex(0xbd5d3c), danger: hex(0xc0392b),
};

fn pal(ui: &egui::Ui) -> &'static Pal {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

fn visuals(p: &Pal, dark: bool) -> egui::Visuals {
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.extreme_bg_color = p.sunken;
    v.faint_bg_color = p.hover;
    v.error_fg_color = p.danger;
    v.window_stroke = Stroke::new(1.0, p.border_strong);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.fg);
    let states = [
        (&mut v.widgets.noninteractive, p.surface, p.border),
        (&mut v.widgets.inactive, p.surface, p.border_strong),
        (&mut v.widgets.hovered, p.hover, p.border_strong),
        (&mut v.widgets.active, p.hover, p.muted),
        (&mut v.widgets.open, p.hover, p.border_strong),
    ];
    for (w, fill, stroke) in states {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, stroke);
        w.fg_stroke = Stroke::new(1.0, p.fg);
        w.corner_radius = CornerRadius::same(6);
        w.expansion = 0.0;
    }
    v
}

fn setup(ctx: &egui::Context) {
    // Bundled Console fonts (font/, latin subset) first; Windows fonts back them up for other scripts.
    let mut fonts = FontDefinitions::default();
    let dir = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| r"C:\Windows".into())).join("Fonts");
    let system = |file: &str| fs::read(dir.join(file)).ok();
    let faces: [(&str, FontFamily, Option<Vec<u8>>); 7] = [
        // Anthropic Sans isn't openly licensed, so it isn't bundled: used when it's in `<config dir>onts`, else Inter.
        ("anthropic", anthropic(), fs::read(config_path().with_file_name("fonts").join("anthropic-sans-medium.otf")).ok()),
        ("inter", FontFamily::Proportional, Some(include_bytes!("../font/inter.ttf").to_vec())),
        ("segoe", FontFamily::Proportional, system("segoeui.ttf")),
        ("jetbrains", FontFamily::Monospace, Some(include_bytes!("../font/jetbrains-mono.ttf").to_vec())),
        ("consolas", FontFamily::Monospace, system("consola.ttf")),
        ("newsreader", serif(), Some(include_bytes!("../font/newsreader.ttf").to_vec())),
        ("georgia", serif(), system("georgia.ttf")),
    ];
    let mut at = std::collections::HashMap::new();
    for (name, family, bytes) in faces {
        let Some(bytes) = bytes else { continue };
        fonts.font_data.insert(name.into(), Arc::new(FontData::from_owned(bytes)));
        let slot = at.entry(family.clone()).or_insert(0);
        fonts.families.entry(family).or_default().insert(*slot, name.into());
        *slot += 1;
    }
    let rest = fonts.families[&FontFamily::Proportional].clone();
    fonts.families.get_mut(&serif()).unwrap().extend(rest.clone());
    fonts.families.entry(anthropic()).or_default().extend(rest); // Inter when Anthropic Sans is absent
    ctx.set_fonts(fonts);

    ctx.set_visuals_of(Theme::Dark, visuals(&DARK, true));
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT, false));
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 4.0);
        s.spacing.interact_size.y = 26.0;
        s.text_styles = [
            (TextStyle::Heading, FontId::proportional(17.0)),
            (TextStyle::Body, FontId::proportional(13.0)),
            (TextStyle::Button, FontId::proportional(13.0)),
            (TextStyle::Small, FontId::proportional(11.5)),
            (TextStyle::Monospace, FontId::monospace(12.0)),
        ]
        .into();
    });
}

fn serif() -> FontFamily {
    FontFamily::Name("serif".into())
}

/// Anthropic Sans (medium), for the usage window's Claude and Antigravity headings.
fn anthropic() -> FontFamily {
    FontFamily::Name("anthropic".into())
}

fn hint(ui: &mut egui::Ui, text: &str) {
    let p = pal(ui);
    ui.label(RichText::new(text).size(11.5).color(p.subtle));
}

/// Full-width select-looking button: value on the left, chevron on the right.
fn picker_btn(ui: &mut egui::Ui, width: f32, text: &str, placeholder: bool) -> egui::Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 26.0), Sense::click());
    let w = ui.style().interact(&resp);
    ui.painter().rect(rect, 6.0, p.sunken, w.bg_stroke, StrokeKind::Inside);
    let (font, color) = if placeholder { (FontId::proportional(13.0), p.subtle) } else { (FontId::monospace(12.0), p.fg) };
    let text_rect = rect.shrink2(vec2(10.0, 0.0));
    ui.painter().with_clip_rect(text_rect.with_max_x(text_rect.max.x - 14.0)).text(text_rect.left_center(), Align2::LEFT_CENTER, text, font, color);
    ui.painter().text(text_rect.right_center(), Align2::RIGHT_CENTER, "⏷", FontId::proportional(10.0), p.subtle);
    resp
}

/// Frameless list row for the model picker.
fn pick_row(ui: &mut egui::Ui, checked: bool, text: &str, mono: bool, tag: &str) -> egui::Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
    if resp.hovered() || resp.has_focus() {
        ui.painter().rect_filled(rect, 5.0, p.hover);
    }
    let r = rect.shrink2(vec2(8.0, 0.0));
    if checked {
        ui.painter().text(r.left_center(), Align2::LEFT_CENTER, "✔", FontId::proportional(11.0), p.accent);
    }
    let font = if mono { FontId::monospace(12.0) } else { FontId::proportional(13.0) };
    ui.painter().with_clip_rect(r.with_max_x(r.max.x - 60.0)).text(r.left_center() + vec2(18.0, 0.0), Align2::LEFT_CENTER, text, font, p.fg);
    if !tag.is_empty() {
        ui.painter().text(r.right_center(), Align2::RIGHT_CENTER, tag, FontId::proportional(11.0), p.subtle);
    }
    resp
}

#[derive(Clone, Copy, PartialEq)]
enum Btn {
    Min,
    Close,
}

fn win_btn(ui: &mut egui::Ui, kind: Btn) -> egui::Response {
    if kind == Btn::Min {
        return icon_btn(ui, Icon::Minimize);
    }
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(40.0, 34.0), Sense::click());
    let (bg, fg) = match (resp.hovered(), kind) {
        (true, Btn::Close) => (p.danger, Color32::WHITE),
        (true, _) => (p.hover, p.fg),
        _ => (Color32::TRANSPARENT, p.muted),
    };
    ui.painter().rect_filled(rect, 0.0, bg);
    let (c, s, d) = (rect.center(), Stroke::new(1.0, fg), 4.5);
    match kind {
        Btn::Close => {
            ui.painter().line_segment([c + vec2(-d, -d), c + vec2(d, d)], s);
            ui.painter().line_segment([c + vec2(d, -d), c + vec2(-d, d)], s);
        }
        Btn::Min => {
            ui.painter().line_segment([c + vec2(-d, 0.0), c + vec2(d, 0.0)], s);
        }
    }
    resp
}

/// Chrome for a frameless window: thin bar with the window buttons, then the title as the body's heading, 1px outline, height fitted to the content.
/// `buttons` are laid out right-to-left. Returns the clicked titlebar button.
fn window(ui: &mut egui::Ui, title: &str, buttons: &[Btn], body: impl FnOnce(&mut egui::Ui)) -> Option<Btn> {
    let bar = if title.is_empty() { 0.0 } else { 32.0 };
    const PAD: i8 = 14;
    let p = pal(ui);
    let mut clicked = None;
    if !title.is_empty() {
    egui::Panel::top(egui::Id::new(("titlebar", title)))
        .exact_size(bar)
        .frame(egui::Frame::new().fill(p.bg))
        .show_separator_line(false)
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for &b in buttons {
                        if win_btn(ui, b).clicked() {
                            clicked = Some(b);
                        }
                    }
                    let rest = ui.available_rect_before_wrap();
                    if ui.interact(rest, ui.id().with("drag"), Sense::drag()).drag_started() {
                        ui.send_viewport_cmd(ViewportCommand::StartDrag);
                    }
                });
            });
        });
    }
    let mut used = 0.0;
    egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin { left: PAD, right: PAD, top: 4, bottom: PAD })).show(ui, |ui| {
        used = ui
            .scope(|ui| {
                if !title.is_empty() {
                    ui.label(RichText::new(title).family(serif()).size(20.0));
                    ui.add_space(4.0);
                }
                body(ui)
            })
            .response
            .rect
            .height();
    });

    // Not resizable, so the window simply follows its content — no dead space at the bottom.
    let screen = ui.ctx().content_rect();
    let want = (bar + 4.0 + PAD as f32 + used).ceil();
    if (screen.height() - want).abs() > 1.0 {
        ui.send_viewport_cmd(ViewportCommand::InnerSize(vec2(screen.width(), want)));
    }
    ui.ctx()
        .layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("outline")))
        .rect_stroke(screen, 0.0, Stroke::new(1.0, p.border_strong), StrokeKind::Inside);
    clicked
}

/// "Never fall back to a paid API key", shared by both Settings copies (launcher and tray popup).
fn no_paid_switch(ui: &mut egui::Ui, cfg: &mut Config) {
    switch(ui, &mut cfg.no_paid_fallback, "Never fall back to a paid API key")
        .on_hover_text("A tier's fallbacks that are custom Anthropic-compatible endpoints (a paid API key) are skipped; a tier's own first choice still runs. Subscription plans (Claude, Antigravity, ChatGPT, MuseAI) and Ollama stay eligible. Routed sessions only.");
}

/// "Compress tool output (third-party models)", shared by both Settings copies.
fn compress_switch(ui: &mut egui::Ui, cfg: &mut Config) {
    switch(ui, &mut cfg.compress_tool_results, "Compress tool output (third-party models)")
        .on_hover_text("Lossless cleanup of tool output before it goes to a third-party model: ANSI colours, progress-bar redraws, trailing spaces, runs of identical lines. Never on Claude, never on error results. Routed sessions only.");
}

/// Toggle switch + label; the whole row is the hit target.
fn switch(ui: &mut egui::Ui, on: &mut bool, text: &str) -> egui::Response {
    let p = pal(ui);
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(13.0), p.fg);
    let track = vec2(30.0, 16.0);
    // Without a label the track is the whole control, so it can sit flush right.
    let gap = if text.is_empty() { 0.0 } else { 8.0 };
    let (rect, mut resp) = ui.allocate_exact_size(vec2(track.x + gap + galley.size().x, 20.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let tr = egui::Rect::from_min_size(rect.left_center() - vec2(0.0, track.y / 2.0), track);
    let (fill, knob) = (if *on { p.fg } else { p.hover }, if *on { p.bg } else { p.muted });
    let edge = if resp.hovered() { p.muted } else { p.border_strong };
    ui.painter().rect(tr, 8.0, fill, Stroke::new(1.0, if *on { p.fg } else { edge }), StrokeKind::Inside);
    let x = egui::lerp((tr.left() + 8.0)..=(tr.right() - 8.0), t);
    ui.painter().circle_filled(egui::pos2(x, tr.center().y), 5.0, knob);
    ui.painter().galley(egui::pos2(tr.right() + 8.0, rect.center().y - galley.size().y / 2.0), galley, p.fg);
    resp
}

#[derive(Clone, Copy)]
enum Icon {
    New,
    Delete,
    GitHub,
    Reload,
    /// A pencil.
    Rename,
    /// A check, in the accent colour: it confirms the edit under way.
    Save,
    /// An arrow into a tray.
    Fetch,
    Settings,
    Minimize,
}

/// 28×28 icon button, drawn with strokes (no icon font): fill only on hover.
fn icon_btn(ui: &mut egui::Ui, icon: Icon) -> egui::Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    let hot = resp.hovered() || resp.has_focus();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), match icon {
        Icon::New => "New", Icon::Delete => "Delete", Icon::GitHub => "GitHub", Icon::Reload => "Reload",
        Icon::Rename => "Rename", Icon::Save => "Save", Icon::Fetch => "Fetch", Icon::Settings => "Settings", Icon::Minimize => "Minimize",
    }));
    let danger = matches!(icon, Icon::Delete);
    if hot {
        ui.painter().rect_filled(rect, 6.0, p.hover);
    }
    let color = match (hot, danger) {
        _ if matches!(icon, Icon::Save) => p.accent,
        (true, true) => p.danger,
        (true, false) => p.fg,
        _ => p.muted,
    };
    let (c, s) = (rect.center(), Stroke::new(1.3, color));
    let pt = ui.painter();
    match icon {
        Icon::Minimize => {
            pt.line_segment([c - vec2(5.0, 0.0), c + vec2(5.0, 0.0)], s);
        }
        Icon::Settings => {
            let teeth = (0..32).map(|i| {
                let a = (i as f32 - 0.5) * std::f32::consts::TAU / 32.0;
                let r = if i % 4 < 2 { 7.0 } else { 5.4 };
                c + vec2(a.cos(), a.sin()) * r
            }).collect();
            pt.add(egui::Shape::closed_line(teeth, s));
            pt.circle_stroke(c, 2.3, s);
        }
        Icon::New => {
            pt.line_segment([c - vec2(5.5, 0.0), c + vec2(5.5, 0.0)], s);
            pt.line_segment([c - vec2(0.0, 5.5), c + vec2(0.0, 5.5)], s);
        }
        Icon::Delete => {
            pt.line_segment([c + vec2(-5.5, -3.5), c + vec2(5.5, -3.5)], s);
            pt.line_segment([c + vec2(-2.0, -5.5), c + vec2(2.0, -5.5)], s);
            pt.rect_stroke(egui::Rect::from_min_max(c + vec2(-4.0, -3.5), c + vec2(4.0, 5.5)), 1.0, s, StrokeKind::Middle);
        }
        Icon::Rename => {
            pt.line_segment([c + vec2(-2.0, 2.0), c + vec2(4.0, -4.0)], Stroke::new(3.0, color));
            pt.add(egui::Shape::convex_polygon(vec![c + vec2(-5.5, 5.5), c + vec2(-0.9, 3.1), c + vec2(-3.1, 0.9)], color, Stroke::NONE));
        }
        Icon::Save => {
            pt.add(egui::Shape::line(vec![c + vec2(-5.0, 0.5), c + vec2(-1.8, 3.8), c + vec2(5.0, -3.5)], Stroke::new(1.8, color)));
        }
        Icon::Fetch => {
            pt.line_segment([c + vec2(0.0, -6.0), c + vec2(0.0, 2.5)], s);
            pt.add(egui::Shape::line(vec![c + vec2(-3.5, -1.0), c + vec2(0.0, 2.5), c + vec2(3.5, -1.0)], s));
            pt.line_segment([c + vec2(-5.5, 5.5), c + vec2(5.5, 5.5)], s);
        }
        Icon::Reload => {
            // A circular arrow: most of a circle, an arrowhead where it ends.
            let r = 5.0;
            let pts: Vec<egui::Pos2> = (0..=20).map(|i| {
                let a = (-50.0 + 280.0 * i as f32 / 20.0).to_radians();
                c + vec2(a.cos(), a.sin()) * r
            }).collect();
            let end = pts[0];
            pt.add(egui::Shape::line(pts, s));
            pt.add(egui::Shape::convex_polygon(vec![end + vec2(-3.2, -1.2), end + vec2(1.6, -3.0), end + vec2(1.2, 1.8)], color, Stroke::NONE));
        }
        Icon::GitHub => {
            let at = |x: f32, y: f32| c + vec2(x - 12.0, y - 12.0) * 0.7;
            let curve = |points: [(f32, f32); 4]| {
                pt.add(egui::epaint::CubicBezierShape::from_points_stroke(points.map(|(x, y)| at(x, y)), false, Color32::TRANSPARENT, s));
            };
            curve([(9.0, 18.0), (3.0, 18.0), (2.0, 14.0), (4.0, 9.0)]);
            pt.add(egui::Shape::line(vec![at(4.0, 9.0), at(4.0, 3.0), at(9.0, 5.0)], s));
            curve([(9.0, 5.0), (11.0, 4.3), (13.0, 4.3), (15.0, 5.0)]);
            pt.add(egui::Shape::line(vec![at(15.0, 5.0), at(20.0, 3.0), at(20.0, 9.0)], s));
            curve([(20.0, 9.0), (22.0, 14.0), (21.0, 18.0), (15.0, 18.0)]);
            curve([(15.0, 18.0), (16.0, 19.0), (16.0, 20.0), (16.0, 23.0)]);
            curve([(9.0, 18.0), (8.0, 19.0), (8.0, 20.0), (8.0, 23.0)]);
            curve([(8.0, 21.0), (4.0, 22.0), (5.0, 18.0), (2.0, 18.0)]);
        }
    }
    resp
}


// ---------------------------------------------------------------- app

#[derive(Clone, Copy, PartialEq, Default)]
enum SettingsSub {
    #[default]
    Models,
    Providers,
}

struct App {
    cfg: Config,
    /// The config file as loaded, so `save` only writes what this window changed.
    loaded: serde_json::Value,
    cwd: String,
    settings: bool,
    settings_sub: SettingsSub,
    settings_pos: Option<egui::Pos2>,
    folder: String,
    profile: profiles::Bar,
    folder_choice: usize,
    custom_folder: String,
    running: (std::time::Instant, bool),
    logo: Option<egui::TextureHandle>,
    folders: Vec<String>,
    /// The tier whose model picker is open, and the provider it is browsing ("" = official).
    tiers: TierEditor,
    /// The Config window's content: providers, accounts, switches.
    providers: providers_ui::ProvidersEditor,
    err: String,
    claude_list: Option<mpsc::Receiver<Result<Vec<String>, String>>>,
    release_rx: Option<mpsc::Receiver<Result<String, String>>>,
    /// Latest GitHub release tag, once the background check is back (or from the cached one).
    latest: Option<Result<String, String>>,
    /// Plans of Antigravity accounts saved before plans were recorded: (email, plan id).
    plans: Option<mpsc::Receiver<Vec<(String, String)>>>,
    out: Arc<Mutex<Option<Launch>>>,
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let r = Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(target_os = "macos")]
    let r = Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = r {
        eprintln!("ccat: could not open a browser ({e}); use “Copy link”.");
    }
}

/// "Antigravity · gemini-3.8-flash-tiered", "Claude · Claude Code's pick".
fn route_text(r: &Route) -> String {
    target_text(&r.provider, &r.model)
}

fn target_text(provider: &str, model: &str) -> String {
    let provider = if provider.is_empty() { OFFICIAL } else { provider };
    let model = if model.is_empty() { "Claude Code's pick" } else { model };
    format!("{provider} · {model}")
}

/// What a selector shows: "Default", else the model id alone, plus ` (provider)` when 2+ providers offer that
/// same id (Claude counts when it lists it), so the same name on two providers can be told apart.
fn selector_text(provider: &str, model: &str, claude: &[String], providers: &[Provider]) -> String {
    if provider.is_empty() && model.is_empty() {
        return "Default".into();
    }
    let offered = claude.iter().any(|m| m == model) as usize + providers.iter().filter(|p| p.models.iter().chain(&p.custom).any(|m| m == model)).count();
    if offered < 2 {
        return model.into();
    }
    format!("{model} ({})", if provider.is_empty() { OFFICIAL } else { provider })
}

impl App {
    fn new((cfg, loaded): (Config, serde_json::Value), cwd: String, out: Arc<Mutex<Option<Launch>>>, ctx: &egui::Context) -> Self {
        // Checked recently (maybe by another ccat window): reuse that answer instead of asking again.
        let fresh = now().saturating_sub(cfg.checked) < REFRESH_SECS && config_path().with_file_name("pricing.json").exists();
        let (mut claude_list, mut release_rx) = (None, None);
        if !fresh {
            let (tx, rx) = mpsc::channel();
            let repaint = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(fetch_claude_models());
                repaint.request_repaint();
            });
            let (rtx, rrx) = mpsc::channel();
            let repaint = ctx.clone();
            std::thread::spawn(move || {
                let _ = rtx.send(fetch_latest_release());
                repaint.request_repaint();
            });
            (claude_list, release_rx) = (Some(rx), Some(rrx));
        }
        // Accounts added before plans were recorded learn theirs in the background (it orders the rotation).
        let unknown: Vec<antigravity::Account> = cfg.providers.iter().filter(|p| p.antigravity).flat_map(|p| p.accounts.iter()).filter(|a| a.plan.is_empty()).cloned().collect();
        let plans = (!unknown.is_empty()).then(|| {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(unknown.iter().filter_map(|a| antigravity::plan_of(a).ok().map(|p| (a.email.clone(), p))).collect());
            });
            rx
        });
        let latest = fresh.then(|| if cfg.latest.is_empty() { Err("the last check failed; trying again later".to_string()) } else { Ok(cfg.latest.clone()) });
        // Start in the invoking terminal's directory; the last workspace is one click away.
        let folders = cfg.recents.clone();
        Self {
            folder: cwd.clone(),
            custom_folder: cwd.clone(),
            profile: Default::default(),
            folder_choice: 0,
            running: (std::time::Instant::now(), live::running()),
            logo: None,
            folders,
            cfg,
            loaded,
            cwd,
            settings: false,
            settings_sub: SettingsSub::Models,
            settings_pos: None,
            tiers: TierEditor::default(),
            providers: Default::default(),
            err: String::new(),
            claude_list,
            release_rx,
            latest,
            plans,
            out,
        }
    }

    /// What stops tier `t` from launching, and whether Config should open for it.
    fn problem(&self, t: usize) -> Option<(String, bool)> {
        let r = &self.cfg.tiers[t];
        let check = |what: String, provider: &str, model: &str| {
            if provider.is_empty() {
                return None;
            }
            let Some(i) = self.cfg.providers.iter().position(|p| p.name == provider) else {
                return Some((format!("{what}: provider “{provider}” no longer exists."), false));
            };
            let p = &self.cfg.providers[i];
            match () {
                _ if p.disabled => Some((format!("“{}” is turned off.", p.name), true)),
                _ if model.trim().is_empty() => Some((format!("{what} needs a model on {}.", p.name), false)),
                _ if p.antigravity && p.accounts.is_empty() => Some((format!("“{}” needs at least one Google account.", p.name), true)),
                _ if p.antigravity && p.accounts.iter().all(|a| a.disabled) => Some((format!("“{}” has every Google account turned off.", p.name), true)),
                _ if p.chatgpt && p.chatgpt_accounts.is_empty() => Some((format!("“{}” needs a signed-in ChatGPT account.", p.name), true)),
                _ if p.chatgpt && p.chatgpt_accounts.iter().all(|a| a.disabled) => Some((format!("“{}” has every account turned off.", p.name), true)),
                _ if p.chatgpt && !p.chatgpt_accounts.iter().any(|a| a.usable()) => Some((format!("“{}”: no account has plan usage granted; sign in again.", p.name), true)),
                _ if p.muse && museai::is_local_bridge(&p.url) => muse_blocked(p, museai::active_account).map(|why| (why, true)),
                // A local Ollama needs no key; Ollama Cloud's is only required by the server.
                _ if p.ollama => p.url.trim().is_empty().then(|| (format!("“{}” needs a URL.", p.name), true)),
                _ if !p.antigravity && !p.chatgpt && (p.url.trim().is_empty() || p.key.trim().is_empty()) => Some((format!("“{}” needs a URL and a key.", p.name), true)),
                _ => None,
            }
        };
        let tier = TIERS[t].0;
        check(tier.to_string(), &r.provider, &r.model)
            // A fallback on a provider that's turned off is simply dropped (see `retarget`), not a reason not to launch.
            .or_else(|| r.fallbacks.iter().enumerate().filter(|(_, f)| !self.cfg.providers.iter().any(|p| p.disabled && p.name == f.provider)).find_map(|(n, f)| check(format!("{tier}'s fallback {}", n + 1), &f.provider, &f.model)))
    }

    fn launch(&mut self, ui: &egui::Ui) {
        if !Path::new(&self.folder).is_dir() {
            self.err = "Choose an existing folder before launching.".into();
            return;
        }
        if let Some((why, open)) = (0..TIERS.len()).find_map(|t| self.problem(t)) {
            self.err = why;
            self.settings |= open;
            return;
        }
        self.cfg.recents.retain(|f| *f != self.folder);
        self.cfg.recents.insert(0, self.folder.clone());
        self.cfg.recents.truncate(12);
        let (tiers, providers, claude_models) = (self.cfg.tiers.clone(), self.cfg.providers.clone(), self.cfg.claude_models.clone());
        *self.out.lock().unwrap() = Some(Launch { tiers, providers, claude_models, claude_off: self.cfg.claude_off, folder: self.folder.clone(), bypass: self.cfg.bypass, fail_on_limit: self.cfg.fail_on_limit, no_paid_fallback: self.cfg.no_paid_fallback, compress_tool_results: self.cfg.compress_tool_results });
        ui.send_viewport_cmd(ViewportCommand::Close);
    }

    fn launch_tab(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let p = pal(ui);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if self.logo.is_none() {
                let icon = app_icon();
                let image = egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
                self.logo = Some(ui.ctx().load_texture("ccat-logo", image, Default::default()));
            }
            ui.add(egui::Image::new(self.logo.as_ref().unwrap()).fit_to_exact_size(vec2(30.0, 30.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if win_btn(ui, Btn::Min).on_hover_text("Minimize").clicked() {
                    ui.send_viewport_cmd(ViewportCommand::Minimized(true));
                }
                if icon_btn(ui, Icon::Settings).on_hover_text("Settings").clicked() {
                    self.settings = !self.settings;
                }
                let update = match &self.latest {
                    Some(Ok(tag)) if is_newer(tag, VERSION) => format!("{VERSION} → {tag} available"),
                    Some(Err(e)) => format!("{VERSION} · Update check failed: {e}"),
                    None => format!("{VERSION} · Checking for updates…"),
                    _ => format!("{VERSION} · Up to date"),
                };
                if icon_btn(ui, Icon::GitHub).on_hover_text(format!("{REPO_URL}\n{update}")).clicked() {
                    open_browser(REPO_URL);
                }
                let drag = ui.available_rect_before_wrap();
                if ui.interact(drag, ui.id().with("launcher-drag"), Sense::drag()).drag_started() {
                    ui.send_viewport_cmd(ViewportCommand::StartDrag);
                }
            });
        });
        ui.add_space(8.0);

        if self.running.0.elapsed() >= Duration::from_secs(1) {
            self.running = (std::time::Instant::now(), live::running());
        }
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        if !self.running.1 {
            self.profile.show(ui, &mut self.cfg, frame, |ui| {
                ui.label(RichText::new("Profile").size(14.0));
            });
            ui.add_space(6.0);
        }
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 24.0), Layout::right_to_left(Align::Center), |ui| {
            switch(ui, &mut self.cfg.bypass, "").on_hover_text("Bypass permissions: allow Claude Code to run tools without asking.");
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.label(RichText::new("Bypass Permission").size(14.0));
            });
        });
        ui.add_space(8.0);
        ui.label(RichText::new("Folders").size(13.0));
        ui.add_space(2.0);
        let previous = self.folders.first().filter(|f| Path::new(f).is_dir()).cloned();
        let name = |path: &str| Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.into());
        let choices = [
            format!("Current location\n/{}", name(&self.cwd)),
            previous.as_ref().map_or_else(|| "Previous workspace\nNone yet".into(), |f| format!("Previous workspace\n/{}", name(f))),
            "Custom".into(),
        ];
        ui.vertical_centered(|ui| {
            let before = self.folder_choice;
            let items = [(0, choices[0].as_str()), (1, choices[1].as_str()), (2, choices[2].as_str())];
            if usage::segmented_full_width(ui, &mut self.folder_choice, &items) {
                if self.folder_choice == 1 && previous.is_none() {
                    self.folder_choice = before;
                } else {
                    self.err.clear();
                }
            }
        }).response.on_hover_text(match self.folder_choice {
            1 => previous.as_deref().unwrap_or("No previous workspace"),
            2 => "Select a recent folder or browse for another",
            _ => &self.cwd,
        });
        if self.folder_choice == 2 {
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 30.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.add_sized([82.0, 30.0], egui::Button::new("Browse")).clicked() {
                    if let Some(d) = rfd::FileDialog::new().set_parent(frame).set_directory(&self.custom_folder).pick_folder() {
                        self.custom_folder = d.display().to_string();
                        self.err.clear();
                    }
                }
                let mut folders = self.folders.clone();
                for folder in [&self.cwd, &self.custom_folder] {
                    if !folders.contains(folder) {
                        folders.push(folder.clone());
                    }
                }
                ui.spacing_mut().interact_size.y = 30.0;
                egui::ComboBox::from_id_salt("custom-folder")
                    .width(ui.available_width())
                    .truncate()
                    .selected_text(&self.custom_folder)
                    .show_ui(ui, |ui| {
                        for folder in folders {
                            if ui.selectable_value(&mut self.custom_folder, folder.clone(), &folder).changed() {
                                self.err.clear();
                            }
                        }
                    });
            });
        }
        self.folder = match self.folder_choice {
            1 => previous.unwrap_or_else(|| self.cwd.clone()),
            2 => self.custom_folder.clone(),
            _ => self.cwd.clone(),
        };
        if !self.err.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new(&self.err).size(12.0).color(p.danger));
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
            if ui.add_sized([width, 30.0], egui::Button::new("Cancel").fill(p.hover).stroke(Stroke::NONE).corner_radius(6)).clicked() {
                ui.send_viewport_cmd(ViewportCommand::Close);
            }
            let launch = egui::Button::new(RichText::new("Launch").strong().color(LIGHT.fg)).fill(Color32::WHITE).stroke(Stroke::NONE).corner_radius(6);
            if ui.add_sized([width, 30.0], launch).clicked() {
                self.launch(ui);
            }
        });
    }

}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if let Some(r) = self.claude_list.as_ref().and_then(|rx| rx.try_recv().ok()) {
            match r {
                Ok(models) if !models.is_empty() => (self.cfg.claude_models, self.cfg.checked) = (models, now()),
                Ok(_) => {}
                Err(e) => eprintln!("ccat: could not refresh Claude models from models.dev ({e}); using the saved list"),
            }
            self.claude_list = None;
        }
        if let Some(found) = self.plans.as_ref().and_then(|rx| rx.try_recv().ok()) {
            for a in self.cfg.providers.iter_mut().flat_map(|p| p.accounts.iter_mut()) {
                if let Some((_, plan)) = found.iter().find(|(e, _)| *e == a.email) {
                    a.plan = plan.clone();
                }
            }
            self.plans = None;
        }
        if let Some(r) = self.release_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            if let Ok(tag) = &r {
                self.cfg.latest = tag.clone();
            }
            self.latest = Some(r);
            self.release_rx = None;
        }
        self.providers.poll(&mut self.cfg);
        if !self.profile.active() && !self.tiers.active() && ui.input(|i| i.key_pressed(Key::Escape)) {
            ui.send_viewport_cmd(ViewportCommand::Close);
        }
        window(ui, "", &[], |ui| self.launch_tab(ui, frame));
        let typing = ui.ctx().egui_wants_keyboard_input();
        if !typing && !self.settings && !self.profile.active() && !self.tiers.active() && ui.input(|i| i.key_pressed(Key::Enter)) {
            self.launch(ui);
        }

        if self.settings {
            if self.settings_pos.is_none() {
                self.settings_pos = ui.input(|i| i.viewport().outer_rect).map(|r| r.right_top() + vec2(8.0, 0.0));
            }
            self.settings_window(ui.ctx(), frame);
        }
    }
}

impl App {
    fn settings_tab(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let p = pal(ui);
        ui.add_space(2.0);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 28.0), Layout::right_to_left(Align::Center), |ui| {
            if win_btn(ui, Btn::Close).on_hover_text("Close").clicked() {
                self.settings = false;
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                if self.settings_sub == SettingsSub::Models && !self.running.1 {
                    self.tiers.bar.show(ui, &mut self.cfg, frame, |ui| {
                        usage::segmented(ui, &mut self.settings_sub, &[(SettingsSub::Models, "Models"), (SettingsSub::Providers, "Providers")]);
                    });
                } else {
                    usage::segmented(ui, &mut self.settings_sub, &[(SettingsSub::Models, "Models"), (SettingsSub::Providers, "Providers")]);
                    let drag = ui.available_rect_before_wrap();
                    if ui.interact(drag, ui.id().with("settings-drag"), Sense::drag()).drag_started() {
                        ui.send_viewport_cmd(ViewportCommand::StartDrag);
                    }
                }
            });
        });
        ui.add_space(8.0);

        if self.settings_sub == SettingsSub::Providers {
            self.tiers.pick = None;
            egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(580.0).show(ui, |ui| {
                self.providers.show(ui, &mut self.cfg, 420.0);
                ui.add_space(10.0);
                usage::display_block(ui, &mut self.cfg);
            });
            return;
        }

        let before = self.cfg.tiers.clone();
        egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(580.0).show(ui, |ui| {
            self.tiers.rows(ui, &mut self.cfg);
            if self.cfg.tiers != before {
                self.err.clear();
            }
            ui.add_space(6.0);
            ui.add(egui::Label::new(egui::RichText::new("Changes reach running ccat sessions within seconds, and every next launch. Claude names and context windows stay as each session started. A session launched with every tier on Claude and no fallback runs plain claude: relaunch it to switch.").size(11.0).color(p.subtle)).wrap());
            ui.add_space(8.0);
            let mut wait = !self.cfg.fail_on_limit;
            ui.horizontal(|ui| {
                switch(ui, &mut wait, "Wait out usage limits")
                    .on_hover_text("When every route and fallback of a request is at its usage limit, the request waits until one can run, so an agent pauses instead of stopping. Esc in Claude Code still cancels it. Routed sessions only.");
            });
            self.cfg.fail_on_limit = !wait;
            ui.add_space(6.0);
            ui.horizontal(|ui| no_paid_switch(ui, &mut self.cfg));
            ui.add_space(6.0);
            ui.horizontal(|ui| compress_switch(ui, &mut self.cfg));
            ui.add_space(10.0);
            usage::display_block(ui, &mut self.cfg);
        });
    }

    /// Providers and tier settings live in their own OS window (an egui immediate viewport).
    fn settings_window(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let mut builder = egui::ViewportBuilder::default()
            .with_title("ccat — Settings")
            .with_icon(app_icon())
            .with_decorations(false)
            .with_resizable(false)
            .with_inner_size([440.0, 520.0]);
        if let Some(pos) = self.settings_pos {
            builder = builder.with_position(pos);
        }
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("providers"), builder, |ui, _| {
            // Esc cancels a rename in progress before it closes the window.
            if ui.input(|i| (i.key_pressed(Key::Escape) && !self.providers.active() && !self.tiers.active()) || i.viewport().close_requested()) {
                self.settings = false;
            }
            window(ui, "", &[], |ui| self.settings_tab(ui, frame));
        });
    }
}

impl Drop for App {
    fn drop(&mut self) {
        profiles::sync(&profiles::dir(), &self.cfg);
        if let Err(e) = save(&self.cfg, &self.loaded) {
            eprintln!("ccat: could not save settings: {e}");
        }
    }
}

// ---------------------------------------------------------------- tier editor

/// Which selector of a tier: its model and fallbacks in order (0 = the model), or its Claude name.
const SPOOF_ROW: usize = usize::MAX;

/// The four tiers, each: its name and the Spoofing switch, the Claude name it's shown as (while spoofing), its
/// model and fallbacks numbered in the order they're tried, and "+ New fallback". In the launcher, and in the
/// usage window's Settings, where a change reaches running sessions.
#[derive(Default)]
struct TierEditor {
    /// The selector whose dropdown is open: (tier, row).
    pick: Option<(usize, usize)>,
    query: String,
    focus: bool,
    /// The configuration button above the tiers, its menu and dialogs.
    bar: profiles::Bar,
    /// This frame's row labels, (tier, row, rect): where the route tree is drawn.
    at: Vec<(usize, usize, egui::Rect)>,
    /// What running sessions report (see `live`), read at most once a second.
    live: Option<(std::time::Instant, std::collections::HashMap<String, live::Tier>)>,
}

/// Left of each row's number: room for the route tree.
const GUTTER: f32 = 12.0;

/// One dropdown entry: the (provider, model) it picks, its text, and a tag on the right.
struct Choice {
    provider: String,
    model: String,
    text: String,
    tag: &'static str,
}

impl TierEditor {
    /// A dropdown, menu or dialog is up: Esc and Enter belong to it, and a popup needs room for it.
    fn active(&self) -> bool {
        self.pick.is_some() || self.bar.active()
    }

    fn rows(&mut self, ui: &mut egui::Ui, cfg: &mut Config) {
        let p = pal(ui);
        self.at.clear();
        if self.live.as_ref().is_none_or(|(t, _)| t.elapsed() > Duration::from_secs(1)) {
            self.live = Some((std::time::Instant::now(), live::read(now())));
        }
        let live = self.live.as_ref().map(|l| l.1.clone()).unwrap_or_default();
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        let before = cfg.tiers.clone();
        let (names, _) = tier_names(&cfg.tiers, routed(&cfg.tiers));
        let claude = claude_choices(&cfg.claude_models, None);
        let claude_off = cfg.claude_off;
        let providers = &cfg.providers;
        let tiers = &mut cfg.tiers;
        let mut saved: Vec<(String, String)> = Vec::new();
        for (t, (tier, _, env)) in TIERS.iter().enumerate() {
            if t > 0 {
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(2.0);
            }
            let family: Vec<String> = claude.iter().filter(|m| m.contains(TIERS[t].1)).cloned().collect();
            let r = &mut tiers[t];
            // Name, and Spoofing when the model isn't a Claude one already.
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                if !r.provider.is_empty() {
                    let mut on = !r.claude_as.is_empty();
                    if switch(ui, &mut on, "Spoofing").on_hover_text("Claude Code treats this tier as the Claude model you pick (context window, thinking) and sends the real id via modelOverrides.").changed() {
                        r.claude_as = if on { family.first().cloned().unwrap_or_default() } else { String::new() };
                    }
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.label(RichText::new(*tier).size(12.5).color(p.muted).strong()).on_hover_text(*env);
                });
            });
            let full = ui.available_width() - 24.0;
            if !r.provider.is_empty() && !r.claude_as.is_empty() {
                let shown = self.selector(ui, t, SPOOF_ROW, "=", full, &r.claude_as.clone(), false);
                if let Some(rect) = shown.filter(|_| self.pick == Some((t, SPOOF_ROW))) {
                    let choices: Vec<Choice> = family.iter().map(|m| Choice { provider: String::new(), model: m.clone(), text: m.clone(), tag: "" }).collect();
                    if let Some(c) = self.dropdown(ui, rect, &[(OFFICIAL.to_string(), choices)], ("", &r.claude_as.clone())) {
                        r.claude_as = c.model;
                    }
                }
                if names[t] != r.claude_as {
                    ui.label(RichText::new(format!("{} already stands for another tier; this one keeps its real id.", r.claude_as)).size(11.5).color(p.danger));
                }
            }
            // The model, then the fallbacks: one numbered chain.
            let mut chain: Vec<(String, String)> = std::iter::once((r.provider.clone(), r.model.clone())).chain(r.fallbacks.iter().map(|f| (f.provider.clone(), f.model.clone()))).collect();
            let mut remove = None;
            let mut picked = None;
            let only = chain.len() == 1;
            for (n, (provider, model)) in chain.iter().enumerate() {
                ui.horizontal(|ui| {
                    let placeholder = provider.is_empty() && model.is_empty();
                    let shown = self.selector(ui, t, n, &(n + 1).to_string(), full - 36.0, &selector_text(provider, model, &claude, providers), placeholder);
                    // Nothing to remove from a lone Default; its space stays so every selector keeps its width.
                    if only && placeholder {
                        ui.allocate_space(vec2(28.0, 28.0));
                    } else {
                        let tip = if n == 0 { "Remove (the first fallback takes its place)" } else { "Remove this fallback" };
                        if icon_btn(ui, Icon::Delete).on_hover_text(tip).clicked() {
                            remove = Some(n);
                        }
                    }
                    if let Some(rect) = shown.filter(|_| self.pick == Some((t, n))) {
                        if let Some(c) = self.dropdown(ui, rect, &model_groups(providers, &(self.query.trim().to_string(), claude.clone()), claude_off), (provider, model)) {
                            picked = Some((n, c));
                        }
                    }
                });
            }
            self.tree(ui, t, &chain, live.get(TIERS[t].1));
            // Lined up with the selectors: past the tree's gutter and the number column `selector` draws.
            ui.horizontal(|ui| {
                ui.add_space(GUTTER + 16.0 + 2.0 * ui.spacing().item_spacing.x);
                if ui.add(egui::Label::new(RichText::new(format!("+ New {tier} fallback")).size(12.0).color(p.subtle)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    chain.push((String::new(), String::new()));
                    self.open(t, chain.len() - 1);
                }
            });
            if let Some((n, c)) = picked {
                if !c.provider.is_empty() && !c.model.is_empty() {
                    saved.push((c.provider.clone(), c.model.clone()));
                }
                chain[n] = (c.provider, c.model);
                self.pick = None;
            }
            if let Some(n) = remove {
                chain.remove(n);
                if chain.is_empty() {
                    chain.push((String::new(), String::new())); // back to Claude Code's own pick
                }
                self.pick = None;
            }
            (r.provider, r.model) = chain[0].clone();
            r.fallbacks = chain[1..].iter().map(|(provider, model)| Fallback { provider: provider.clone(), model: model.clone() }).collect();
        }
        // A typed id stays on offer under its provider.
        for (provider, model) in saved {
            if let Some(p) = cfg.providers.iter_mut().find(|p| p.name == provider) {
                if !p.models.contains(&model) && !p.custom.contains(&model) {
                    p.custom.push(model);
                }
            }
        }
        // The applied configuration follows, so switching away and back finds the edit.
        if cfg.tiers != before {
            profiles::sync(&profiles::dir(), cfg);
        }
    }

    /// Tier `t`'s route tree in the gutter: from its Claude name (or its first route) to each route, the branch that
    /// served its last request (in a running session, the last 15 minutes) in the accent colour, an orange dot on each
    /// route a held request waits for (the usage window says what for, at its bottom).
    fn tree(&self, ui: &mut egui::Ui, t: usize, chain: &[(String, String)], lt: Option<&live::Tier>) {
        let p = pal(ui);
        let rect = |row: usize| self.at.iter().find(|(tt, r, _)| *tt == t && *r == row).map(|x| x.2);
        let rows: Vec<egui::Rect> = (0..chain.len()).filter_map(rect).collect();
        let row_of = |h: &live::Hop| chain.iter().position(|(pv, m)| live::matches(h, pv, m));
        let held = lt.filter(|l| l.held > 0);
        // A tier that is waiting isn't running anything: no branch lit.
        let active = lt.filter(|_| held.is_none()).and_then(|l| l.used.as_ref()).and_then(row_of);
        let waiting: Vec<usize> = held.map(|l| l.waiting.iter().filter_map(|(h, _)| row_of(h)).collect()).unwrap_or_default();
        let orange = hex(0xe08a2e);
        if let (Some(root), Some(last)) = (rect(SPOOF_ROW).or(rows.first().copied()), rows.last()) {
            if rows.len() > 1 || root != rows[0] {
                let (x, end) = (root.left() - GUTTER + 3.0, root.left() - 3.0);
                let (grey, lit) = (Stroke::new(1.5, p.border_strong), Stroke::new(2.0, p.accent));
                let pt = ui.painter();
                pt.line_segment([egui::pos2(x, root.center().y), egui::pos2(x, last.center().y)], grey);
                for r in rows.iter().chain((root != rows[0]).then_some(&root)) {
                    pt.line_segment([egui::pos2(x, r.center().y), egui::pos2(end, r.center().y)], grey);
                }
                if let Some(n) = active {
                    let y = rows[n].center().y;
                    for seg in [[egui::pos2(x, root.center().y), egui::pos2(x, y)], [egui::pos2(x, y), egui::pos2(end, y)], [egui::pos2(x, root.center().y), egui::pos2(end, root.center().y)]] {
                        pt.line_segment(seg, lit);
                    }
                }
                for n in &waiting {
                    let center = egui::pos2(end, rows[*n].center().y);
                    pt.circle_filled(center, 3.0, orange);
                    if let Some(l) = held {
                        let dot_rect = egui::Rect::from_center_size(center, egui::vec2(14.0, 14.0));
                        ui.interact(dot_rect, ui.id().with(("waiting_dot", t, *n)), egui::Sense::hover())
                            .on_hover_text(format!("Waiting at usage limits: {}", live::waits(l, now())));
                    }
                }
            }
        }
    }

    fn open(&mut self, t: usize, row: usize) {
        (self.pick, self.focus) = (Some((t, row)), true);
        self.query.clear();
    }

    /// A numbered row's selector; clicking toggles its dropdown. Returns its rect while that dropdown is open.
    #[allow(clippy::too_many_arguments)]
    fn selector(&mut self, ui: &mut egui::Ui, t: usize, row: usize, label: &str, w: f32, text: &str, placeholder: bool) -> Option<egui::Rect> {
        let p = pal(ui);
        let mut open = None;
        ui.horizontal(|ui| {
            ui.add_space(GUTTER);
            let at = ui.add_sized([16.0, 26.0], egui::Label::new(RichText::new(label).size(12.5).color(p.subtle))).rect;
            self.at.push((t, row, at));
            let resp = picker_btn(ui, w - GUTTER - ui.spacing().item_spacing.x, text, placeholder);
            if resp.clicked() {
                if self.pick == Some((t, row)) {
                    self.pick = None;
                } else {
                    self.open(t, row);
                }
            }
            open = (self.pick == Some((t, row))).then_some(resp.rect);
        });
        open
    }

    /// The open selector's list, under it: a search box, then the choices grouped by provider. Returns the choice
    /// clicked (or the first one on Enter); closes on a click elsewhere or Esc.
    fn dropdown(&mut self, ui: &egui::Ui, anchor: egui::Rect, groups: &[(String, Vec<Choice>)], cur: (&str, &str)) -> Option<Choice> {
        let p = pal(ui);
        let mut chosen: Option<Choice> = None;
        let q = self.query.trim().to_lowercase();
        // Below the selector, or above it when there's more room there; the list shrinks to fit.
        let screen = ui.ctx().content_rect();
        let (below, above) = (screen.bottom() - anchor.bottom() - 8.0, anchor.top() - screen.top() - 8.0);
        let (pos, pivot, room) = if below >= 260.0 || below >= above {
            (anchor.left_bottom() + vec2(0.0, 4.0), Align2::LEFT_TOP, below)
        } else {
            (anchor.left_top() - vec2(0.0, 4.0), Align2::LEFT_BOTTOM, above)
        };
        let list_h = (room - 70.0).clamp(90.0, 300.0);
        let area = egui::Area::new(ui.id().with("tier-dropdown"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .pivot(pivot)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                    ui.set_width(anchor.width() - 16.0);
                    let te = ui.add(TextEdit::singleline(&mut self.query).hint_text("Search for models, or type an id").desired_width(f32::INFINITY));
                    if self.focus {
                        te.request_focus();
                        self.focus = false;
                    }
                    let enter = te.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.add_space(4.0);
                    let mut first: Option<&Choice> = None;
                    egui::ScrollArea::vertical().max_height(list_h).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        for (group, choices) in groups {
                            let shown: Vec<&Choice> = choices.iter().filter(|c| q.is_empty() || c.text.to_lowercase().contains(&q) || c.model.to_lowercase().contains(&q) || c.tag == "custom").collect();
                            if shown.is_empty() {
                                continue;
                            }
                            ui.add_space(4.0);
                            ui.label(RichText::new(group).size(11.0).color(p.subtle));
                            for c in shown {
                                first.get_or_insert(c);
                                let checked = (c.provider.as_str(), c.model.as_str()) == cur;
                                let row = pick_row(ui, checked, &c.text, !c.model.is_empty(), c.tag);
                                let row = if c.text != c.model && !c.model.is_empty() { row.on_hover_text(&c.model) } else { row };
                                if row.clicked() {
                                    chosen = Some(Choice { provider: c.provider.clone(), model: c.model.clone(), text: String::new(), tag: "" });
                                }
                            }
                        }
                        if first.is_none() {
                            hint(ui, "No models. Fetch a provider's list in Providers, or type an id.");
                        }
                    });
                    if enter && chosen.is_none() {
                        chosen = first.map(|c| Choice { provider: c.provider.clone(), model: c.model.clone(), text: String::new(), tag: "" });
                    }
                });
            });
        let click_elsewhere = ui.input(|i| i.pointer.any_click() && i.pointer.interact_pos().is_some_and(|at| !area.response.rect.contains(at) && !anchor.contains(at)));
        if click_elsewhere || ui.input(|i| i.key_pressed(Key::Escape)) {
            self.pick = None;
        }
        // A custom id is only offered under each provider while typing; it's remembered with that provider.
        chosen
    }
}

/// Every model the selectors offer, by provider: Claude first (Claude Code's own pick, then the Claude models),
/// then each provider's fetched and custom models. `typed` is offered on every non-Claude provider as a custom id.
/// Providers turned off are left out; so are the Claude models while `claude_off` (Default is Claude Code's own
/// pick and always stays).
fn model_groups(providers: &[Provider], typed_and_claude: &(String, Vec<String>), claude_off: bool) -> Vec<(String, Vec<Choice>)> {
    let (typed, claude) = typed_and_claude;
    let mut groups = vec![(
        OFFICIAL.to_string(),
        std::iter::once(Choice { provider: String::new(), model: String::new(), text: "Default".into(), tag: "" })
            .chain(claude.iter().filter(|_| !claude_off).map(|m| Choice { provider: String::new(), model: m.clone(), text: m.clone(), tag: "" }))
            .collect(),
    )];
    for p in providers.iter().filter(|p| !p.disabled) {
        // ChatGPT models show their display name; the slug is on hover.
        let shown = |m: &String| p.chatgpt_models.iter().find(|x| x.slug == *m).map(|x| x.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| m.clone());
        let mut choices: Vec<Choice> = p.models.iter().map(|m| Choice { provider: p.name.clone(), model: m.clone(), text: shown(m), tag: "" }).collect();
        choices.extend(p.custom.iter().filter(|c| !p.models.contains(c)).map(|m| Choice { provider: p.name.clone(), model: m.clone(), text: m.clone(), tag: "saved" }));
        if !typed.is_empty() && !p.models.contains(typed) && !p.custom.contains(typed) {
            choices.push(Choice { provider: p.name.clone(), model: typed.clone(), text: format!("Use “{typed}”"), tag: "custom" });
        }
        groups.push((p.name.clone(), choices));
    }
    groups
}

// ---------------------------------------------------------------- main / spawn

/// The window/taskbar icon: the repo logo, decoded once (the Providers viewport is rebuilt every frame).
fn app_icon() -> Arc<egui::IconData> {
    static ICON: std::sync::OnceLock<Arc<egui::IconData>> = std::sync::OnceLock::new();
    ICON.get_or_init(|| Arc::new(eframe::icon_data::from_png_bytes(include_bytes!("../icon.png")).expect("icon.png is a valid PNG"))).clone()
}

fn main() {
    // Everything else is forwarded to claude, so these are the only flags ccat keeps for itself
    // (the two usage ones are internal: the tray helper and its window).
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => return println!("ccat {}", &VERSION[1..]),
        #[cfg(windows)]
        Some(usage::TRAY_FLAG) => return usage::run_tray(),
        Some(usage::POPUP_FLAG) => {
            let at = |i| std::env::args().nth(i).and_then(|x| x.parse().ok()).unwrap_or(f64::MAX);
            return usage::run_popup((at(2), at(3)));
        }
        _ => usage::ensure_tray(),
    }
    let cwd =std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let out = Arc::new(Mutex::new(None));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ccat")
            .with_icon(app_icon())
            .with_decorations(false) // titlebar is drawn by App::titlebar
            .with_resizable(false)
            .with_inner_size([400.0, 300.0]),
        centered: true,
        ..Default::default()
    };
    let (app_out, app_cwd) = (out.clone(), cwd.clone());
    let result = eframe::run_native(
        "ccat",
        options,
        Box::new(move |cc| {
            setup(&cc.egui_ctx);
            let (mut cfg, loaded) = load();
            profiles::ensure(&profiles::dir(), &mut cfg);
            Ok(Box::new(App::new((cfg, loaded), app_cwd, app_out, &cc.egui_ctx)))
        }),
    );
    if let Err(e) = result {
        eprintln!("ccat: could not open window: {e}");
        return;
    }
    let Some(launch) = out.lock().unwrap().take() else { return };
    run_claude(launch, &cwd);
}


/// What `claude` gets for a launch. A routed launch has started the proxy, which lives as long as this process.
struct Prepared {
    env: Vec<(&'static str, String)>,
    overrides: serde_json::Map<String, serde_json::Value>,
    /// The proxy's MCP endpoint (Antigravity tools) as inline JSON: no file to leave behind when a terminal
    /// is closed mid-session (a file per launch used to pile up next to the config).
    mcp_config: Option<String>,
    names: [String; 4],
    routed: bool,
    uses_ag: bool,
}

/// Why the built-in MuseAI on the local bridge can't serve: nobody is signed in, or its account is turned off. It
/// signs in by cookie login and needs no key; `login` finds the muse-cli login for configs saved before accounts were.
fn muse_blocked(p: &Provider, login: impl FnOnce() -> Option<museai::Account>) -> Option<String> {
    match p.muse_account.clone().or_else(login) {
        None => Some(format!("“{}” needs a signed-in MuseAI account. Use + in Providers to sign in.", p.name)),
        Some(a) if a.disabled => Some(format!("“{}” has its account turned off.", p.name)),
        Some(_) => None,
    }
}

/// Where a tier's requests go in the proxy and the model they run there; `None` when that can't be served (a
/// provider gone, no model, no key or account). `sent` is the name Claude Code sends for the tier (fixed at launch).
fn target(provider: &str, model: &str, providers: &[Provider], claude_models: &[String], t: usize, sent: &str) -> Option<(proxy::Upstream, String)> {
    let model = bare_id(model).split('[').next().unwrap_or("").trim().to_string();
    if provider.is_empty() {
        // Claude Code's pick: the Claude name it sends, unless the tier sends a name only ccat knows (it ran on
        // another provider at launch); then the newest Claude model of the tier's family.
        let model = match model.is_empty() {
            true if sent.is_empty() || (sent.starts_with("claude-") && !sent.contains('@')) => String::new(),
            true => claude_choices(claude_models, Some(t)).into_iter().next()?,
            false => model,
        };
        return Some((proxy::Upstream::Official, model));
    }
    let p = providers.iter().find(|p| p.name == provider)?;
    match () {
        _ if p.disabled || model.is_empty() => None,
        _ if p.antigravity && p.accounts.iter().all(|a| a.disabled) => None,
        _ if p.antigravity => Some((proxy::Upstream::Antigravity, model)),
        _ if p.chatgpt => p.chatgpt_accounts.iter().any(|a| a.usable()).then_some((proxy::Upstream::ChatGPT, model)),
        _ if p.ollama => Some((proxy::Upstream::Ollama { url: p.url.clone(), key: p.key.clone() }, model)),
        _ if p.muse && museai::is_local_bridge(&p.url) => muse_blocked(p, museai::active_account).is_none().then_some((proxy::Upstream::MuseAI { url: p.url.clone(), key: p.key.clone() }, model)),
        _ if p.url.trim().is_empty() || p.key.trim().is_empty() => None,
        _ if p.muse => Some((proxy::Upstream::MuseAI { url: p.url.clone(), key: p.key.clone() }, model)),
        _ => Some((proxy::Upstream::Endpoint { name: p.name.clone(), url: p.url.clone(), key: p.key.trim().to_string() }, model)),
    }
}

/// Points the proxy's routes at `tiers`, keeping the names Claude Code sends. A tier that can't be served keeps
/// what it had; those tiers come back, by name. Fallbacks that can't be served are dropped, official Claude
/// ones too while `claude_off`.
fn retarget(routes: &mut [proxy::Route], tiers: &[Route; 4], providers: &[Provider], claude_models: &[String], claude_off: bool) -> Vec<&'static str> {
    let mut bad = Vec::new();
    for (t, (route, r)) in routes.iter_mut().zip(tiers).enumerate() {
        let sent = route.names.first().cloned().unwrap_or_default();
        match target(&r.provider, &r.model, providers, claude_models, t, &sent) {
            Some((up, model)) => (route.upstream, route.model) = (up, model),
            None => bad.push(TIERS[t].0),
        }
        route.fallbacks = r.fallbacks.iter().filter(|f| !(claude_off && f.provider.is_empty())).filter_map(|f| target(&f.provider, &f.model, providers, claude_models, t, &sent)).collect();
    }
    bad
}

/// Watches the config while a routed session runs: a model switched in ccat (launcher or Settings) reaches this
/// session's next request.
fn follow_config(px: &proxy::Proxy) {
    let (routes, disabled, chatgpt, chatgpt_models, fail_on_limit, no_paid_fallback, compress_tool_results) = (px.routes.clone(), px.disabled.clone(), px.chatgpt.clone(), px.chatgpt_models.clone(), px.fail_on_limit.clone(), px.no_paid_fallback.clone(), px.compress_tool_results.clone());
    let stamp = || fs::metadata(config_path()).and_then(|m| m.modified()).ok();
    let usage_stamp = || fs::metadata(config_path().with_file_name("usage.json")).and_then(|m| m.modified()).ok();
    let px = px.clone();
    std::thread::spawn(move || {
        let (mut seen, mut usage_seen, mut beat) = (stamp(), None, 0u32);
        loop {
            std::thread::sleep(Duration::from_secs(2));
            // A minute's heartbeat, so the usage window knows this session still runs.
            beat += 1;
            if beat % 30 == 1 {
                px.heartbeat();
            }
            // The tray refreshes usage every few minutes: a reset it sees puts the route back at once.
            if usage_stamp() != usage_seen {
                usage_seen = usage_stamp();
                px.recheck(&usage::load_usage());
            }
            if stamp() == seen {
                continue;
            }
            seen = stamp();
            // Read only: never move the user's config aside from a background thread.
            let Some(cfg) = fs::read_to_string(config_path()).ok().and_then(|t| serde_json::from_str::<Config>(&t).ok()) else { continue };
            let mut next = routes.read().unwrap().clone();
            let bad = retarget(&mut next, &cfg.tiers, &cfg.providers, &cfg.claude_models, cfg.claude_off);
            *disabled.write().unwrap() = off_accounts(&cfg.providers);
            let gpt = cfg.providers.iter().find(|p| p.chatgpt);
            *chatgpt.write().unwrap() = gpt.map(|p| p.chatgpt_accounts.clone()).unwrap_or_default();
            *chatgpt_models.write().unwrap() = gpt.map(|p| p.chatgpt_models.clone()).unwrap_or_default();
            fail_on_limit.store(cfg.fail_on_limit, std::sync::atomic::Ordering::Relaxed);
            no_paid_fallback.store(cfg.no_paid_fallback, std::sync::atomic::Ordering::Relaxed);
            compress_tool_results.store(cfg.compress_tool_results, std::sync::atomic::Ordering::Relaxed);
            for (old, new) in routes.read().unwrap().iter().zip(&next) {
                if (&old.upstream, &old.model, &old.fallbacks) != (&new.upstream, &new.model, &new.fallbacks) {
                    common::log(&format!("{} now runs {} on {:?}", old.tier, new.model, new.upstream)); // Debug names the provider, never its key
                }
            }
            if !bad.is_empty() {
                common::log(&format!("settings changed, but {} can't be served as set; kept as they were", bad.join(", ")));
            }
            *routes.write().unwrap() = next;
        }
    });
}

fn prepare(l: &Launch, folder: &str) -> Result<Prepared, String> {
    let routed = routed(&l.tiers);
    let (names, overrides) = tier_names(&l.tiers, routed);
    let provider = |r: &Route| l.providers.iter().find(|p| !r.provider.is_empty() && p.name == r.provider);
    let uses_ag = l.tiers.iter().any(|r| provider(r).is_some_and(|p| p.antigravity));

    let mut env: Vec<(&'static str, String)> = TIERS.iter().zip(&names).filter(|(_, n)| !n.is_empty()).map(|((_, _, var), n)| (*var, n.clone())).collect();
    let mut mcp_config = None;
    if routed {
        let stripped = |n: &str| n.split('[').next().unwrap_or("").trim().to_string();
        let mut routes: Vec<proxy::Route> = (0..TIERS.len())
            .map(|t| {
                // What Claude Code sends for this tier: the override target of a spoofed tier, else its name.
                let sent = overrides.get(&names[t]).and_then(|v| v.as_str()).map_or_else(|| stripped(&names[t]), String::from);
                let spoof = overrides.contains_key(&names[t]).then(|| names[t].clone());
                let names = [Some(sent), spoof].into_iter().flatten().filter(|n| !n.is_empty()).collect();
                proxy::Route { tier: TIERS[t].1, names, upstream: proxy::Upstream::Official, model: String::new(), fallbacks: Vec::new() }
            })
            .collect();
        let bad = retarget(&mut routes, &l.tiers, &l.providers, &l.claude_models, l.claude_off);
        if !bad.is_empty() {
            return Err(format!("{} can't be served as set; check it in ccat", bad.join(", ")));
        }
        {
            // Every account, used or not at launch: a tier can be switched to Antigravity while the session runs, and
            // an account turned on again joins. Those turned off are told to the pool, which skips them.
            let mut accounts = l.providers.iter().find(|p| p.antigravity).map(|p| p.accounts.clone()).unwrap_or_default();
            // Launched before the launcher learned every plan: ask now, in parallel (it orders the rotation).
            std::thread::scope(|s| {
                let asks: Vec<_> = accounts.iter_mut().filter(|a| a.plan.is_empty()).map(|a| s.spawn(move || a.plan = antigravity::plan_of(a).unwrap_or_default())).collect();
                asks.into_iter().for_each(|h| drop(h.join()));
            });
            let gpt = l.providers.iter().find(|p| p.chatgpt);
            let (gpt_accounts, gpt_models) = gpt.map(|p| (p.chatgpt_accounts.clone(), p.chatgpt_models.clone())).unwrap_or_default();
            match proxy::start(accounts, gpt_accounts, gpt_models, routes, PathBuf::from(folder)) {
                Ok(px) => {
                    *px.disabled.write().unwrap() = off_accounts(&l.providers);
                    px.fail_on_limit.store(l.fail_on_limit, std::sync::atomic::Ordering::Relaxed);
                    px.no_paid_fallback.store(l.no_paid_fallback, std::sync::atomic::Ordering::Relaxed);
                    px.compress_tool_results.store(l.compress_tool_results, std::sync::atomic::Ordering::Relaxed);
                    // A request held at a usage limit can wait for hours: Claude Code mustn't give up on it after its
                    // usual 10 minutes. A timeout the user set stays.
                    if std::env::var_os("API_TIMEOUT_MS").is_none() {
                        env.push(("API_TIMEOUT_MS", (7 * 24 * 3600 * 1000u64).to_string()));
                    }
                    if uses_ag {
                        let servers = serde_json::json!({ "mcpServers": { "antigravity": {
                            "type": "http", "url": format!("{}/mcp", px.url), "headers": { "Authorization": format!("Bearer {}", px.token) },
                        } } });
                        mcp_config = Some(servers.to_string());
                    }
                    // The token rides in a header of its own: Claude tiers need Claude Code's login in `authorization`,
                    // and any tier may become one while the session runs. Without a login, the token stands in.
                    let mine = std::env::var("ANTHROPIC_CUSTOM_HEADERS").ok().filter(|h| !h.trim().is_empty()).map_or(String::new(), |h| h + "\n");
                    env.push(("ANTHROPIC_CUSTOM_HEADERS", format!("{mine}x-ccat-token: {}", px.token)));
                    if !providers::claude::claude_set_up() {
                        env.push(("ANTHROPIC_AUTH_TOKEN", px.token.clone()));
                    }
                    env.push(("ANTHROPIC_BASE_URL", px.url.clone()));
                    follow_config(&px);
                }
                Err(e) => return Err(format!("could not start the local proxy: {e}")),
            }
        }
    }
    Ok(Prepared { env, overrides, mcp_config, names, routed, uses_ag })
}

fn claude_settings(overrides: &serde_json::Map<String, serde_json::Value>, env: &[(&str, String)]) -> serde_json::Value {
    let mut settings = serde_json::json!({});
    if !overrides.is_empty() { settings["modelOverrides"] = serde_json::json!(overrides); }
    // Claude settings can override inherited env; a routed launch owns this non-secret endpoint.
    if let Some((_, url)) = env.iter().find(|(key, _)| *key == "ANTHROPIC_BASE_URL") {
        settings["env"] = serde_json::json!({"ANTHROPIC_BASE_URL": url});
    }
    settings
}

fn run_claude(l: Launch, cwd: &str) {
    let folder = if l.folder.is_empty() { cwd.to_string() } else { l.folder.clone() };
    let Prepared { env, overrides, mcp_config, names, routed, uses_ag } = prepare(&l, &folder).unwrap_or_else(|e| {
        eprintln!("ccat: {e}");
        std::process::exit(1);
    });

    if l.tiers.iter().all(|r| *r == Route::default()) {
        eprintln!("ccat → Claude (default) · {folder}");
    } else {
        eprintln!("ccat → {folder}");
        for (t, r) in l.tiers.iter().enumerate() {
            let shown = if names[t].is_empty() || names[t] == bare_id(&r.model) { String::new() } else { format!(" as {}", names[t]) };
            eprintln!("       {:<6} {}{shown}", TIERS[t].0, route_text(r));
        }
    }
    if routed {
        eprintln!("       proxy log: {}", common::log_path().display());
    }
    if uses_ag {
        eprintln!("       Antigravity extras: WebSearch → Google Search, MCP tool mcp__antigravity__generate_image");
    }

    let build = |program: &str| {
        let mut cmd = Command::new(program);
        cmd.current_dir(&folder);
        if l.bypass {
            cmd.arg("--dangerously-skip-permissions");
        }
        let settings = claude_settings(&overrides, &env);
        if settings.as_object().is_some_and(|s| !s.is_empty()) {
            cmd.arg("--settings").arg(settings.to_string());
        }
        cmd.args(std::env::args_os().skip(1)); // `ccat --resume` → `claude --resume`
        // Last on purpose: `--mcp-config` is variadic and would swallow a positional prompt that followed it.
        if let Some(json) = &mcp_config {
            cmd.arg("--mcp-config").arg(json);
        }
        if env.iter().any(|(k, _)| *k == "ANTHROPIC_AUTH_TOKEN") {
            cmd.env_remove("ANTHROPIC_API_KEY"); // would win over AUTH_TOKEN and trigger the key-approval prompt
        }
        cmd.envs(env.iter().map(|(k, v)| (*k, v.as_str())));
        cmd
    };

    // Keep the parent alive for plain sessions too: its OS lock identifies an active CLI, not a tray or launcher.
    let _session = live::session().map_err(|e| eprintln!("ccat: could not mark the running session: {e}")).ok();

    // Ctrl+C belongs to claude; a handler fn (unlike SIG_IGN) is not inherited by the child.
    let _ = ctrlc::set_handler(|| {});
    // The usage tray is shared by every ccat: bring it back if the terminal that started it went away.
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(30));
        usage::ensure_tray();
    });
    let status = build("claude").status().or_else(|_| build("claude.cmd").status());
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("ccat: failed to start claude: {e}");
            std::process::exit(127);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routed_launch_pins_its_endpoint_without_putting_secrets_in_arguments() {
        let overrides = serde_json::Map::from_iter([("claude-haiku-5-5".into(), serde_json::json!("muse-chat@haiku"))]);
        let env = vec![("ANTHROPIC_BASE_URL", "http://127.0.0.1:54321".into()), ("ANTHROPIC_AUTH_TOKEN", "private-auth".into()), ("ANTHROPIC_CUSTOM_HEADERS", "x-ccat-token: private-header".into())];
        let settings = claude_settings(&overrides, &env);
        assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:54321");
        assert_eq!(settings["modelOverrides"]["claude-haiku-5-5"], "muse-chat@haiku");
        assert!(!settings.to_string().contains("private-"));
        assert_eq!(settings["env"].as_object().unwrap().len(), 1);
        assert!(claude_settings(&serde_json::Map::new(), &[]).as_object().unwrap().is_empty());
    }

    pub(super) fn live_config() -> Config {
        let text = fs::read_to_string(user_config_path()).expect("Existing ccat config required");
        let cfg: Config = serde_json::from_str(&text).unwrap();
        let path = config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        museai::secure_directory(path.parent().unwrap()).unwrap();
        save(&cfg, &serde_json::Value::Null).unwrap();
        cfg
    }

    #[test]
    fn unit_tests_never_use_the_live_config() {
        assert!(config_path().starts_with(std::env::temp_dir().join(format!("ccat-tests-{}", std::process::id()))));
    }

    fn route(provider: &str, model: &str, claude_as: &str) -> Route {
        Route { provider: provider.into(), model: model.into(), claude_as: claude_as.into(), ..Default::default() }
    }

    fn endpoint(name: &str) -> Provider {
        Provider { name: name.into(), url: "https://x.io/v1/".into(), key: "k".into(), ..Default::default() }
    }

    #[test]
    fn selector_names_the_provider_only_when_the_model_is_ambiguous() {
        let claude: Vec<String> = ["claude-opus-5-5".into()].to_vec();
        let or = Provider { name: "OR".into(), models: vec!["gemini-x".into(), "claude-opus-5-5".into()], ..Default::default() };
        let ag = Provider { name: AG_NAME.into(), custom: vec!["gemini-x".into()], ..Default::default() };
        let all = [or, ag];
        assert_eq!(selector_text("", "", &claude, &all), "Default");
        assert_eq!(selector_text("OR", "gemini-x", &claude, &all), "gemini-x (OR)", "two providers offer it");
        assert_eq!(selector_text(AG_NAME, "gemini-x", &claude, &all), "gemini-x (Antigravity)", "custom ids count too");
        assert_eq!(selector_text("", "claude-opus-5-5", &claude, &all), "claude-opus-5-5 (Claude)", "Claude counts when it lists the id");
        assert_eq!(selector_text("OR", "claude-opus-5-5", &claude, &all), "claude-opus-5-5 (OR)");
        assert_eq!(selector_text("OR", "only-here", &claude, &all), "only-here");
        assert_eq!(selector_text("", "claude-haiku-4-5", &claude, &all), "claude-haiku-4-5");
    }

    #[test]
    fn only_all_claude_runs_plain() {
        let mut tiers: [Route; 4] = Default::default();
        assert!(!routed(&tiers), "nothing set: plain claude");
        tiers[1] = route("", "claude-opus-5-5", "");
        assert!(!routed(&tiers), "Claude models only: still plain");
        tiers[1].fallbacks = vec![Fallback { provider: AG_NAME.into(), model: "g".into() }];
        assert!(routed(&tiers), "a fallback needs the proxy");
        tiers = std::array::from_fn(|_| route("OR", "m", ""));
        assert!(routed(&tiers), "one endpoint for all four: routed too, so it can be switched while running");
    }

    #[test]
    fn retarget_follows_settings_and_keeps_names() {
        let ag = Provider { name: AG_NAME.into(), antigravity: true, accounts: vec![antigravity::Account::default()], ..Default::default() };
        let providers = vec![ag, endpoint("OR")];
        let models: Vec<String> = ["claude-sonnet-5-5", "claude-opus-5-5"].map(String::from).to_vec();
        let mut tiers: [Route; 4] = Default::default();
        tiers[2] = route(AG_NAME, "gemini-x[1m]", "");
        let (names, _) = tier_names(&tiers, true);
        assert_eq!(names[2], "gemini-x@sonnet[1m]", "routed, every non-Claude tier sends a name of its own");
        let mut routes: Vec<proxy::Route> = (0..4)
            .map(|t| proxy::Route { tier: TIERS[t].1, names: vec![names[t].split('[').next().unwrap().to_string()].into_iter().filter(|n| !n.is_empty()).collect(), upstream: proxy::Upstream::Official, model: String::new(), fallbacks: Vec::new() })
            .collect();
        assert!(retarget(&mut routes, &tiers, &providers, &models, false).is_empty());
        assert_eq!((&routes[2].upstream, routes[2].model.as_str()), (&proxy::Upstream::Antigravity, "gemini-x"));

        // Switched while running: Sonnet to Claude Code's pick, with a fallback; Haiku to a provider that's gone.
        tiers[2] = Route { fallbacks: vec![Fallback { provider: "OR".into(), model: "vendor/m".into() }, Fallback { provider: "Gone".into(), model: "x".into() }], ..Default::default() };
        tiers[3] = route("Gone", "m", "");
        let bad = retarget(&mut routes, &tiers, &providers, &models, false);
        assert_eq!(bad, ["Haiku"]);
        assert_eq!(routes[2].model, "claude-sonnet-5-5", "the tier still sends gemini-x@sonnet: Anthropic gets its family's newest");
        assert_eq!(routes[2].fallbacks.iter().map(|f| f.1.as_str()).collect::<Vec<_>>(), ["vendor/m"], "a fallback that can't be served is skipped");
        assert_eq!(routes[3].upstream, proxy::Upstream::Official, "a tier that can't be served keeps what it had");
        assert_eq!(routes[0].model, "", "an untouched Claude tier passes Claude Code's own name through");
    }

    #[test]
    fn claude_names_become_model_overrides() {
        // The user's example: Fable and Opus official, Sonnet on Antigravity shown as Sonnet 5.5, Haiku a random model.
        let tiers = [
            route("", "claude-fable-5-1", "ignored on an official tier"),
            route("", "", ""),
            route(AG_NAME, "gemini-3.8-flash-tiered", "claude-sonnet-5-5"),
            route("OR", "anthropic/stealth/space-bunny-alpha[1m]", "claude-haiku-4-5"),
        ];
        let (names, o) = tier_names(&tiers, true);
        assert_eq!(names, ["claude-fable-5-1", "", "claude-sonnet-5-5", "claude-haiku-4-5"]);
        assert_eq!(o["claude-sonnet-5-5"], "gemini-3.8-flash-tiered@sonnet", "routed targets carry their tier");
        assert_eq!(o["claude-haiku-4-5"], "stealth/space-bunny-alpha@haiku", "the [1m] hint and vendor prefix are not sent upstream");
        assert_eq!(o.len(), 2);

        let (names, o) = tier_names(&tiers, false);
        assert_eq!(o["claude-sonnet-5-5"], "gemini-3.8-flash-tiered", "direct: the plain id");
        assert_eq!(names[3], "claude-haiku-4-5");

        // One Claude name can stand for one id only: the first tier keeps it.
        let clash = [route("OR", "a", "claude-opus-5-5"), route("OR", "b", "claude-opus-5-5"), Route::default(), route("OR", "c", "")];
        let (names, o) = tier_names(&clash, false);
        assert_eq!(names[..2], ["claude-opus-5-5", "b"]);
        assert_eq!(names[3], "c", "unspoofed: the real id");
        assert_eq!(o.len(), 1);
    }

    #[test]
    fn claude_choices_follow_the_tier_family() {
        let models: Vec<String> = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-1", "claude-haiku-4-5"].map(String::from).to_vec();
        assert_eq!(claude_choices(&models, Some(0)), ["claude-fable-5-1"]);
        assert_eq!(claude_choices(&models, Some(1)), ["claude-opus-5-5"]);
        assert_eq!(claude_choices(&models, None).len(), 4, "an official tier may run any Claude model");
        assert!(claude_choices(&[], Some(2)).iter().all(|m| m.contains("sonnet")), "offline list is used when none was fetched");
    }

    #[test]
    fn release_tags_compare_numerically() {
        assert!(is_newer("v0.2.0", "v0.1.0"));
        assert!(is_newer("v1.10.0", "v1.9.3"), "10 > 9 as numbers, not text");
        assert!(is_newer("0.1.1", "v0.1"), "missing parts count as 0");
        assert!(!is_newer("v0.1.0", "v0.1.0"));
        assert!(!is_newer("v0.1.0-beta", "v0.1.0"), "a pre-release suffix is ignored");
        assert!(!is_newer("garbage", "v0.1.0"));
    }

    #[test]
    fn antigravity_is_one_builtin_profile() {
        let acc = |e: &str| antigravity::Account { email: e.into(), refresh: "r".into(), project: "p".into(), ..Default::default() };
        // The config this came from: OpenRouter flipped to Antigravity by a stray click on the old Type box.
        let mut cfg = Config {
            providers: vec![
                Provider { name: "OpenRouter".into(), antigravity: true, url: "https://openrouter.ai/api/v1".into(), key: "k".into(), ..Default::default() },
                Provider { name: "Gehihi".into(), antigravity: true, accounts: vec![acc("a@x")], ..Default::default() },
                Provider { name: "Provider 2".into(), ..Default::default() },
                Provider { name: "Spare".into(), antigravity: true, accounts: vec![acc("a@x"), acc("b@x")], ..Default::default() },
                Provider { name: AG_NAME.into(), url: "https://x.io".into(), ..Default::default() },
            ],
            tiers: [route("Gehihi", "g", ""), route("Spare", "g", ""), route("OpenRouter", "m", ""), Route::default()],
            ..Default::default()
        };
        builtin_antigravity(&mut cfg);
        let names: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, [AG_NAME, "OpenRouter", "Provider 2", "Antigravity endpoint"], "built-in first, the rest keep their order");
        let ag = &cfg.providers[0];
        assert_eq!(ag.accounts.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), ["a@x", "b@x"], "other Antigravity accounts merged, no duplicates");
        assert!(!cfg.providers[1].antigravity && cfg.providers[1].key == "k", "OpenRouter is an endpoint again, key intact");
        let on: Vec<&str> = cfg.tiers.iter().map(|r| r.provider.as_str()).collect();
        assert_eq!(on, [AG_NAME, AG_NAME, "OpenRouter", ""], "tiers follow the merge");
        assert_eq!(cfg.providers.iter().filter(|p| p.antigravity).count(), 1);

        // Idempotent, and a fresh install gets the built-in too.
        let before: Vec<String> = cfg.providers.iter().map(|p| p.name.clone()).collect();
        builtin_antigravity(&mut cfg);
        assert_eq!(cfg.providers.iter().map(|p| p.name.clone()).collect::<Vec<_>>(), before);
        let mut fresh = Config::default();
        builtin_antigravity(&mut fresh);
        assert_eq!(fresh.providers.len(), 1);
        assert!(fresh.providers[0].antigravity && fresh.providers[0].name == AG_NAME);
    }

    #[test]
    fn chatgpt_is_one_builtin_provider_second_in_the_list() {
        let acct = |k: &str| chatgpt::ChatgptAccount { key: k.into(), email: format!("{k}@x"), granted: true, ..Default::default() };
        let mut cfg = Config {
            providers: vec![
                Provider { name: AG_NAME.into(), antigravity: true, ..Default::default() },
                endpoint("OR"),
                Provider { name: GPT_NAME.into(), url: "https://x.io".into(), key: "k".into(), ..Default::default() },
                Provider { name: "Old".into(), chatgpt: true, chatgpt_accounts: vec![acct("a")], ..Default::default() },
                Provider { name: "Spare".into(), chatgpt: true, chatgpt_accounts: vec![acct("a"), acct("b")], ..Default::default() },
            ],
            tiers: [route(GPT_NAME, "m", ""), route("Old", "g", ""), route("Spare", "g", ""), Route::default()],
            ..Default::default()
        };
        builtin_antigravity(&mut cfg);
        builtin_chatgpt(&mut cfg);
        let names: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, [AG_NAME, GPT_NAME, "OR", "ChatGPT endpoint"], "Antigravity, then ChatGPT, then the rest");
        assert_eq!(cfg.providers.iter().filter(|p| p.chatgpt).count(), 1);
        assert_eq!(cfg.providers[1].chatgpt_accounts.iter().map(|a| a.key.as_str()).collect::<Vec<_>>(), ["a", "b"], "accounts merged, no duplicates");
        assert!(!cfg.providers[3].chatgpt && cfg.providers[3].key == "k", "the user's endpoint is renamed, key intact");
        let on: Vec<&str> = cfg.tiers.iter().map(|r| r.provider.as_str()).collect();
        assert_eq!(on, ["ChatGPT endpoint", GPT_NAME, GPT_NAME, ""], "tiers follow");
        let before: Vec<String> = cfg.providers.iter().map(|p| p.name.clone()).collect();
        builtin_chatgpt(&mut cfg);
        assert_eq!(cfg.providers.iter().map(|p| p.name.clone()).collect::<Vec<_>>(), before, "idempotent");
        let mut fresh = Config::default();
        builtin_antigravity(&mut fresh);
        builtin_chatgpt(&mut fresh);
        assert_eq!(fresh.providers.iter().map(|p| (p.name.as_str(), p.chatgpt)).collect::<Vec<_>>(), [(AG_NAME, false), (GPT_NAME, true)]);
        assert!(check_rename(&fresh.providers, 0, "chatgpt").is_err(), "the name is reserved");
    }

    #[test]
    fn chatgpt_needs_a_usable_account_to_route() {
        let mut gpt = Provider { name: GPT_NAME.into(), chatgpt: true, ..Default::default() };
        let claude: Vec<String> = Vec::new();
        assert!(target(GPT_NAME, "gpt-x", &[gpt.clone()], &claude, 1, "").is_none(), "no account");
        gpt.chatgpt_accounts = vec![chatgpt::ChatgptAccount { key: "k".into(), granted: false, ..Default::default() }];
        assert!(target(GPT_NAME, "gpt-x", &[gpt.clone()], &claude, 1, "").is_none(), "plan usage not granted");
        gpt.chatgpt_accounts[0].granted = true;
        assert_eq!(target(GPT_NAME, "gpt-x", &[gpt.clone()], &claude, 1, "").map(|(u, m)| (format!("{u:?}"), m)), Some(("ChatGPT".into(), "gpt-x".into())));
        gpt.chatgpt_accounts[0].disabled = true;
        assert!(target(GPT_NAME, "gpt-x", &[gpt.clone()], &claude, 1, "").is_none(), "turned off");
        // The picker shows the display name; the slug is what is sent.
        gpt.chatgpt_accounts[0].disabled = false;
        gpt.models = vec!["gpt-x".into(), "gpt-y".into()];
        gpt.chatgpt_models = vec![chatgpt::ChatgptModel { slug: "gpt-x".into(), name: "GPT X".into(), ..Default::default() }];
        let groups = model_groups(&[gpt], &(String::new(), claude), false);
        let (text, model): (Vec<&str>, Vec<&str>) = groups[1].1.iter().map(|c| (c.text.as_str(), c.model.as_str())).unzip();
        assert_eq!((text, model), (vec!["GPT X", "gpt-y"], vec!["gpt-x", "gpt-y"]));
    }

    #[test]
    fn ollama_is_one_builtin_provider_third_in_the_list() {
        let mut cfg = Config {
            providers: vec![
                Provider { name: AG_NAME.into(), antigravity: true, ..Default::default() },
                Provider { name: GPT_NAME.into(), chatgpt: true, ..Default::default() },
                endpoint("OR"),
                Provider { name: OLLAMA_NAME.into(), url: "http://192.168.1.5:11434".into(), ..Default::default() },
            ],
            tiers: [route(OLLAMA_NAME, "llama3.3", ""), Route::default(), Route::default(), Route::default()],
            ..Default::default()
        };
        builtin_antigravity(&mut cfg);
        builtin_chatgpt(&mut cfg);
        builtin_ollama(&mut cfg);
        builtin_muse(&mut cfg);
        let names: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, [AG_NAME, GPT_NAME, OLLAMA_NAME, MUSE_NAME, "OR", "Ollama endpoint"], "Antigravity, ChatGPT, Ollama, MuseAI, then endpoints");
        assert_eq!(cfg.providers.iter().filter(|p| p.ollama).count(), 1);
        assert_eq!(cfg.providers[2].url, ollama::DEFAULT_URL);
        assert_eq!(cfg.tiers[0].provider, "Ollama endpoint", "tiers follow rename");

        let claude: Vec<String> = Vec::new();
        let ol = &cfg.providers[2];
        let t = target(OLLAMA_NAME, "qwen2.5-coder:32b", &[ol.clone()], &claude, 0, "");
        assert_eq!(t.map(|(u, m)| (format!("{u:?}"), m)), Some(("Ollama".into(), "qwen2.5-coder:32b".into())));

        let mu = Provider { muse_account: Some(museai::Account::default()), ..cfg.providers[3].clone() };
        let t_mu = target(MUSE_NAME, "muse-chat", &[mu], &claude, 0, "");
        assert_eq!(t_mu.map(|(u, m)| (format!("{u:?}"), m)), Some(("MuseAI".into(), "muse-chat".into())));
    }

    #[test]
    fn muse_account_launcher_and_routing_agreements() {
        let claude: Vec<String> = Vec::new();
        let make_app = |p: Provider| {
            let cfg = Config {
                providers: vec![p],
                tiers: [route(MUSE_NAME, "muse-chat", ""), Route::default(), Route::default(), Route::default()],
                ..Default::default()
            };
            App {
                cfg,
                loaded: serde_json::Value::Null,
                cwd: String::new(),
                settings: false,
                settings_sub: SettingsSub::default(),
                settings_pos: None,
                folder: String::new(),
                profile: profiles::Bar::default(),
                folder_choice: 0,
                custom_folder: String::new(),
                running: (std::time::Instant::now(), false),
                logo: None,
                folders: Vec::new(),
                tiers: TierEditor::default(),
                providers: providers_ui::ProvidersEditor::default(),
                err: String::new(),
                claude_list: None,
                release_rx: None,
                latest: None,
                plans: None,
                out: Arc::new(Mutex::new(None)),
            }
        };

        // 1. Valid signed-in enabled local Muse: must be ready with no manual API key
        let ready_mu = Provider {
            name: MUSE_NAME.into(),
            muse: true,
            url: museai::DEFAULT_URL.into(),
            muse_account: Some(museai::Account { viewer_id: "test-viewer".into(), disabled: false, ..Default::default() }),
            ..Default::default()
        };
        let app = make_app(ready_mu.clone());
        assert_eq!(app.problem(0), None, "signed-in enabled Muse must be ready to launch with no key");
        assert_eq!(target(MUSE_NAME, "muse-chat", &[ready_mu.clone()], &claude, 0, "").map(|(u, m)| (format!("{u:?}"), m)), Some(("MuseAI".into(), "muse-chat".into())));

        // 2. Disabled local Muse account: must not be treated ready or routed
        let disabled_acc_mu = Provider {
            name: MUSE_NAME.into(),
            muse: true,
            url: museai::DEFAULT_URL.into(),
            muse_account: Some(museai::Account { viewer_id: "test-viewer".into(), disabled: true, ..Default::default() }),
            ..Default::default()
        };
        let app_disabled_acc = make_app(disabled_acc_mu.clone());
        assert_eq!(app_disabled_acc.problem(0), Some(("“MuseAI” has its account turned off.".into(), true)));
        assert!(target(MUSE_NAME, "muse-chat", &[disabled_acc_mu], &claude, 0, "").is_none(), "disabled account must not route");

        // 3. Signed out (no saved account, no login on disk): the message points at + sign-in, not key fields.
        let signed_out = Provider { muse_account: None, ..ready_mu.clone() };
        let why = muse_blocked(&signed_out, || None).unwrap();
        assert!(why.contains("signed-in") && why.contains("+") && !why.contains("key"), "{why}");
        // ... but a muse-cli login on disk from before accounts were saved still counts.
        assert_eq!(muse_blocked(&signed_out, || Some(museai::Account::default())), None);
        assert_eq!(muse_blocked(&signed_out, || Some(museai::Account { disabled: true, ..Default::default() })).as_deref(), Some("“MuseAI” has its account turned off."));

        // 4. Parent provider turned off wins over a good account.
        let off = Provider { disabled: true, ..ready_mu.clone() };
        assert_eq!(make_app(off.clone()).problem(0), Some(("“MuseAI” is turned off.".into(), true)));
        assert!(target(MUSE_NAME, "muse-chat", &[off], &claude, 0, "").is_none());

        // 5. A custom remote Muse endpoint keeps the generic URL + key rules, in both places.
        let remote = Provider { url: "https://muse.example.com".into(), muse_account: None, ..ready_mu.clone() };
        assert_eq!(make_app(remote.clone()).problem(0), Some(("“MuseAI” needs a URL and a key.".into(), true)));
        assert!(target(MUSE_NAME, "muse-chat", &[remote.clone()], &claude, 0, "").is_none());
        let remote = Provider { key: "k".into(), ..remote };
        assert_eq!(make_app(remote.clone()).problem(0), None);
        assert!(target(MUSE_NAME, "muse-chat", &[remote], &claude, 0, "").is_some());

        // 6. A plain endpoint without a key is still refused.
        let keyless = Provider { key: String::new(), ..endpoint("OR") };
        let mut app = make_app(keyless.clone());
        app.cfg.tiers[0] = route("OR", "m", "");
        assert_eq!(app.problem(0), Some(("“OR” needs a URL and a key.".into(), true)));
        assert!(target("OR", "m", &[keyless], &claude, 0, "").is_none());

        // 7. A local Ollama needs only its URL (the route is named MuseAI here only because `make_app` routes to it).
        let local = Provider { name: MUSE_NAME.into(), ollama: true, url: "http://localhost:11434".into(), ..Default::default() };
        assert_eq!(make_app(local.clone()).problem(0), None);
        assert_eq!(make_app(Provider { url: " ".into(), ..local }).problem(0), Some(("“MuseAI” needs a URL.".into(), true)));
    }

    #[test]
    fn a_single_fallback_becomes_the_chain() {
        let c: Config = serde_json::from_str(r#"{"tiers":[{"provider":"","fallback":{"provider":"OR","model":"m"}},{},{},{}]}"#).unwrap();
        let mut r = c.tiers[0].clone();
        r.fallbacks.extend(r.fallback.take());
        assert_eq!(r.fallbacks, [Fallback { provider: "OR".into(), model: "m".into() }]);
        assert!(serde_json::to_string(&r).unwrap().contains("\"fallbacks\""), "saved as the chain only");
        assert!(!serde_json::to_string(&r).unwrap().contains("\"fallback\""));
    }

    #[test]
    fn old_config_still_loads() {
        let c: Config = serde_json::from_str(r#"{"providers":[{"name":"x","url":"u","default":"m"}],"bypass":true}"#).unwrap();
        assert!(c.bypass && c.tiers.iter().all(|r| *r == Route::default()));
    }

    #[test]
    fn per_provider_tiers_become_routes() {
        // The shape the user's config had: one Antigravity profile picked in the launcher, everything on its Default.
        let raw = serde_json::json!({ "provider": "Antigravity", "providers": [
            { "name": "OpenRouter", "default": "or-model" },
            { "name": "Antigravity", "antigravity": true, "default": "gemini-3.8-flash-tiered", "haiku": "gemini-3.5-flash-lite",
              "claude_as": ["claude-sonnet-5-5", "claude-fable-5-1", "claude-opus-5-5", "", "claude-haiku-4-5"] },
        ] });
        let t = migrate_tiers(&raw);
        assert_eq!(t[0], route(AG_NAME, "gemini-3.8-flash-tiered", "claude-fable-5-1"), "an empty tier meant the Default model");
        assert_eq!(t[2], route(AG_NAME, "gemini-3.8-flash-tiered", "claude-sonnet-5-5"), "the Default tier's Claude name lands on its family's tier");
        assert_eq!(t[3], route(AG_NAME, "gemini-3.5-flash-lite", "claude-haiku-4-5"));
        assert_eq!(migrate_tiers(&serde_json::json!({ "provider": "", "providers": [] })), <[Route; 4]>::default(), "launched as plain Claude");
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ccat-main-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn blank_routes() -> Vec<proxy::Route> {
        (0..4).map(|t| proxy::Route { tier: TIERS[t].1, names: Vec::new(), upstream: proxy::Upstream::Official, model: String::new(), fallbacks: Vec::new() }).collect()
    }

    #[test]
    fn rename_and_delete_reach_every_profile_file() {
        let d = tmp_dir("provider-edit");
        let mut cfg = Config { providers: vec![endpoint("OR"), endpoint("Other")], profile: "A".into(), ..Default::default() };
        cfg.tiers = [route("OR", "m", ""), route("Other", "x", ""), Route { fallbacks: vec![Fallback { provider: "OR".into(), model: "f".into() }], ..Default::default() }, Route::default()];
        let other = [
            route("OR", "o", ""),
            Route::default(),
            route("Other", "y", ""),
            Route { fallbacks: vec![Fallback { provider: "OR".into(), model: "g".into() }, Fallback { provider: "Other".into(), model: "h".into() }], ..Default::default() },
        ];
        profiles::write(&d, "A", &cfg.tiers).unwrap();
        profiles::write(&d, "B", &other).unwrap();

        rename_provider(&mut cfg, &d, 0, "Router");
        assert_eq!(cfg.providers[0].name, "Router");
        assert_eq!((cfg.tiers[0].provider.as_str(), cfg.tiers[2].fallbacks[0].provider.as_str()), ("Router", "Router"), "the applied tiers follow");
        let b = profiles::load(&d, "B").unwrap();
        assert_eq!((b[0].provider.as_str(), b[3].fallbacks[0].provider.as_str(), b[3].fallbacks[1].provider.as_str()), ("Router", "Router", "Other"), "so does a profile that isn't applied");
        assert_eq!(profiles::load(&d, "A").unwrap(), cfg.tiers, "and the applied one's file");

        delete_provider(&mut cfg, &d, 0);
        assert_eq!(cfg.providers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Other"]);
        assert_eq!(cfg.tiers[0], Route::default(), "its tier is back to Claude Code's pick");
        assert!(cfg.tiers[2].fallbacks.is_empty(), "and no fallback uses it");
        assert_eq!(cfg.tiers[1].provider, "Other", "the others stay");
        let b = profiles::load(&d, "B").unwrap();
        assert_eq!(b[0], Route::default(), "reset in the other profile too");
        assert_eq!(b[2].provider, "Other");
        assert_eq!(b[3].fallbacks, [Fallback { provider: "Other".into(), model: "h".into() }], "only the deleted provider's fallbacks go");
        assert_eq!(profiles::load(&d, "A").unwrap(), cfg.tiers);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn rename_needs_a_free_name() {
        let all = [Provider { name: AG_NAME.into(), antigravity: true, ..Default::default() }, endpoint("OR"), endpoint("Mine")];
        assert_eq!(check_rename(&all, 1, "  Router "), Ok("Router".into()));
        assert_eq!(check_rename(&all, 1, "or"), Ok("or".into()), "it may change its own case");
        assert!(check_rename(&all, 1, "  ").is_err(), "an empty name would read as the official login");
        assert!(check_rename(&all, 1, "mine").is_err(), "taken, whatever the case");
        assert!(check_rename(&all, 2, "antigravity").is_err(), "the built-in's name is reserved");
    }

    #[test]
    fn turned_off_providers_are_not_served_or_offered() {
        let mut off = endpoint("Off");
        off.disabled = true;
        off.models = vec!["m".into()];
        let mut ag = Provider { name: AG_NAME.into(), antigravity: true, accounts: vec![antigravity::Account { email: "a@x".into(), ..Default::default() }], ..Default::default() };
        let providers = vec![ag.clone(), endpoint("On"), off];
        let claude: Vec<String> = ["claude-opus-5-5".into()].to_vec();
        assert!(target("Off", "m", &providers, &claude, 1, "").is_none(), "a provider that's off can't be a tier's target");
        assert!(target("On", "m", &providers, &claude, 1, "").is_some());
        assert!(target(AG_NAME, "g", &providers, &claude, 1, "").is_some());
        ag.accounts[0].disabled = true;
        assert!(target(AG_NAME, "g", &[ag], &claude, 1, "").is_none(), "Antigravity with every account off can't serve either");

        // Live: a fallback on the provider that was turned off is dropped; a primary on it keeps its old route.
        let mut tiers: [Route; 4] = Default::default();
        tiers[1] = route("On", "m", "");
        tiers[1].fallbacks = vec![Fallback { provider: "Off".into(), model: "m".into() }, Fallback { provider: "On".into(), model: "n".into() }];
        let mut routes = blank_routes();
        assert!(retarget(&mut routes, &tiers, &providers, &claude, false).is_empty());
        assert_eq!(routes[1].fallbacks.iter().map(|f| f.1.as_str()).collect::<Vec<_>>(), ["n"]);
        tiers[1].provider = "Off".into();
        assert_eq!(retarget(&mut routes, &tiers, &providers, &claude, false), ["Opus"], "a primary on a provider that's off is reported");
        assert_eq!(routes[1].model, "m", "and keeps its old route");

        // The pickers leave it out; Claude's models go with claude_off, Default stays.
        let typed = (String::new(), claude.clone());
        let names = |off: bool| model_groups(&providers, &typed, off).into_iter().map(|(g, c)| (g, c.len())).collect::<Vec<_>>();
        assert_eq!(names(false), [(OFFICIAL.to_string(), 2), (AG_NAME.to_string(), 0), ("On".to_string(), 0)]);
        assert_eq!(names(true)[0], (OFFICIAL.to_string(), 1), "only Default is left of Claude");
    }

    #[test]
    fn claude_off_drops_official_fallbacks_only() {
        let providers = vec![endpoint("OR")];
        let claude: Vec<String> = ["claude-sonnet-5-5".into()].to_vec();
        let mut tiers: [Route; 4] = Default::default();
        tiers[2] = route("", "claude-sonnet-5-5", "");
        tiers[2].fallbacks = vec![Fallback { provider: String::new(), model: "claude-sonnet-5-5".into() }, Fallback { provider: "OR".into(), model: "m".into() }];
        let mut routes = blank_routes();
        assert!(retarget(&mut routes, &tiers, &providers, &claude, false).is_empty());
        assert_eq!(routes[2].fallbacks.len(), 2);
        assert!(retarget(&mut routes, &tiers, &providers, &claude, true).is_empty(), "a tier whose primary is official keeps working");
        assert_eq!((&routes[2].upstream, routes[2].model.as_str()), (&proxy::Upstream::Official, "claude-sonnet-5-5"));
        assert_eq!(routes[2].fallbacks.len(), 1, "the official fallback is skipped");
        assert_eq!(routes[2].fallbacks[0].1, "m");
    }

    #[test]
    fn switches_load_on_by_default_and_round_trip() {
        let c: Config = serde_json::from_str(r#"{"providers":[{"name":"x","accounts":[{"email":"a@x"}]}]}"#).unwrap();
        assert!(!c.claude_off && !c.providers[0].disabled && !c.providers[0].accounts[0].disabled);
        let mut c = c;
        c.claude_off = true;
        c.providers[0].accounts[0].disabled = true;
        let back: Config = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
        assert!(back.claude_off && back.providers[0].accounts[0].disabled);
        assert_eq!(off_accounts(&back.providers), HashSet::from(["a@x".to_string()]));
        let p = Provider { models: vec!["a".into(), "b".into()], custom: vec!["b".into(), "c".into()], ..Default::default() };
        assert_eq!(model_count(&p), 3, "a custom id the list already has isn't counted twice");
    }

    #[test]
    fn save_keeps_what_another_window_wrote() {
        let loaded = serde_json::json!({ "recents": ["a"], "providers": [{ "name": "P" }] });
        let theirs = serde_json::json!({ "recents": ["a"], "providers": [{ "name": "P" }, { "name": "Added elsewhere" }] });
        let mine = serde_json::json!({ "recents": ["b", "a"], "providers": [{ "name": "P" }] });
        let out = merged(Some(theirs), &loaded, mine.clone());
        assert_eq!(out["recents"], serde_json::json!(["b", "a"]), "this window's change lands");
        assert_eq!(out["providers"].as_array().unwrap().len(), 2, "the other window's provider survives");
        assert_eq!(merged(None, &loaded, mine.clone()), mine, "no file yet: all of it");
    }

    /// Live, through real `claude -p`: the requested setup (Fable and Opus official, Sonnet on Antigravity shown as
    /// Claude Sonnet 5.5, Haiku left to Claude Code). Needs a Claude login and an Antigravity account in the ccat
    /// config; spends two tiny requests. `cargo test -- --ignored live_mixed --nocapture`
    #[test]
    #[ignore = "uses your Claude login and your Antigravity account"]
    fn live_mixed_tiers() {
        let cfg = live_config();
        let tiers = [Route::default(), Route::default(), route(AG_NAME, "gemini-3.8-flash-tiered", "claude-sonnet-5-5"), Route::default()];
        let l = Launch { tiers, providers: cfg.providers, claude_models: cfg.claude_models, claude_off: false, folder: String::new(), bypass: false, fail_on_limit: false, no_paid_fallback: false, compress_tool_results: false };
        let dir = std::env::temp_dir().display().to_string();
        let p = prepare(&l, &dir).unwrap();
        assert!(p.routed && !p.env.iter().any(|(k, _)| *k == "ANTHROPIC_AUTH_TOKEN"), "official tiers keep the Claude login");
        // Claude Code reports usage under the name it was told; only Anthropic's own answers carry `iterations`.
        for (tier, official) in [("haiku", true), ("sonnet", false)] {
            let cmd = |program: &str| {
                let mut c = Command::new(program);
                // A parent Claude Code session (e.g. the one running this test) passes its own settings down; drop them.
                for (k, _) in std::env::vars_os() {
                    let k = k.to_string_lossy().to_ascii_uppercase();
                    if k.starts_with("CLAUDE") || k.starts_with("ANTHROPIC") {
                        c.env_remove(&k);
                    }
                }
                c.current_dir(&dir).envs(p.env.iter().map(|(k, v)| (*k, v.as_str())));
                // The SDK's debug log prints response headers: the plan limits the status line shows ride on them.
                c.env("ANTHROPIC_LOG", "debug");
                c.args(["-p", "Reply with the single word pong.", "--model", tier, "--output-format", "json"]);
                c.arg("--settings").arg(serde_json::json!({ "modelOverrides": p.overrides }).to_string());
                c.arg("--mcp-config").arg(p.mcp_config.as_deref().expect("an Antigravity tier brings the MCP tools"));
                c.output()
            };
            let out = cmd("claude").or_else(|_| cmd("claude.cmd")).unwrap();
            let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            let text = stdout.lines().rfind(|l| l.starts_with("{\"")).unwrap_or_default();
            println!("{tier}: {text}");
            let v: serde_json::Value = serde_json::from_str(text).unwrap();
            let limits = format!("{stdout}{stderr}").contains("anthropic-ratelimit-unified-5h-utilization");
            assert_eq!(limits, official, "{tier}: Claude plan limits reach Claude Code only from the official API");
            assert_eq!(v["is_error"], false, "{tier}: {text}");
            assert!(v["result"].as_str().unwrap_or("").to_lowercase().contains("pong"), "{tier}: {text}");
            let anthropic = v["usage"]["iterations"].as_array().is_some_and(|a| !a.is_empty());
            assert_eq!(anthropic, official, "{tier} went to the wrong provider: {text}");
        }
    }


    /// Live: one session, switched while it runs. Sonnet starts on Antigravity (shown as Claude Sonnet 5.5); the
    /// settings move it to Claude; the next request goes to Anthropic. Uses a copy of the config in a temp folder.
    /// `cargo test -- --ignored live_switch --nocapture`
    #[test]
    #[ignore = "uses your Claude login and your Antigravity account"]
    fn live_switch_while_running() {
        let mut cfg = live_config();
        cfg.tiers = [Route::default(), Route::default(), route(AG_NAME, "gemini-3.8-flash-tiered", "claude-sonnet-5-5"), Route::default()];
        save(&cfg, &serde_json::Value::Null).unwrap();
        let l = Launch { tiers: cfg.tiers.clone(), providers: cfg.providers.clone(), claude_models: cfg.claude_models.clone(), claude_off: false, folder: String::new(), bypass: false, fail_on_limit: false, no_paid_fallback: false, compress_tool_results: false };
        let work = std::env::temp_dir().display().to_string();
        let p = prepare(&l, &work).unwrap();
        let ask = || {
            let mut c = Command::new("claude");
            for (k, _) in std::env::vars_os() {
                let k = k.to_string_lossy().to_ascii_uppercase();
                if k.starts_with("CLAUDE") || k.starts_with("ANTHROPIC") {
                    c.env_remove(&k);
                }
            }
            c.current_dir(&work).envs(p.env.iter().map(|(k, v)| (*k, v.as_str())));
            c.args(["-p", "Reply with the single word pong.", "--model", "sonnet", "--output-format", "json"]);
            c.arg("--settings").arg(serde_json::json!({ "modelOverrides": p.overrides }).to_string());
            let out = c.output().or_else(|_| Command::new("claude.cmd").output()).unwrap();
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            let v: serde_json::Value = serde_json::from_str(text.lines().rfind(|l| l.starts_with("{\"")).unwrap_or("{}")).unwrap();
            assert_eq!(v["is_error"], false, "{text}");
            v["usage"]["iterations"].as_array().is_some_and(|a| !a.is_empty()) // only Anthropic's answers carry these
        };
        assert!(!ask(), "Sonnet starts on Antigravity");
        cfg.tiers[2] = Route::default();
        save(&cfg, &serde_json::to_value(&l.tiers).unwrap()).unwrap();
        std::thread::sleep(Duration::from_secs(5));
        assert!(ask(), "the same session's next Sonnet request went to Anthropic");
    }

}
