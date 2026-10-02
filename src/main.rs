//! ccat — pick a provider/folder in a small native window, then run `claude` in this terminal.
//! Overrides live only in the child process env; no Claude config file is read or written.
//! An Antigravity provider is served by an in-process proxy (see `proxy`), so ccat stays alive while claude runs.

mod antigravity;
mod mcp;
mod proxy;

use eframe::egui::{
    self, vec2, Align, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Key, Layout,
    RichText, Sense, Stroke, StrokeKind, TextEdit, TextStyle, Theme, ViewportCommand,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

/// (UI label, env var). Index 0 is the default model.
const TIERS: [(&str, &str); 5] = [
    ("Default", "ANTHROPIC_MODEL"),
    ("Fable", "ANTHROPIC_DEFAULT_FABLE_MODEL"),
    ("Opus", "ANTHROPIC_DEFAULT_OPUS_MODEL"),
    ("Sonnet", "ANTHROPIC_DEFAULT_SONNET_MODEL"),
    ("Haiku", "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
];
const NO_PROVIDER: &str = "Default";
/// The built-in profile's fixed name; every other profile is an Anthropic-compatible endpoint.
const AG_NAME: &str = "Antigravity";

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
struct Provider {
    name: String,
    url: String,
    key: String,
    models: Vec<String>,
    custom: Vec<String>,
    default: String,
    fable: String,
    opus: String,
    sonnet: String,
    haiku: String,
    /// Served through the local Antigravity proxy from `accounts` instead of `url` + `key`.
    antigravity: bool,
    accounts: Vec<antigravity::Account>,
    /// Per tier (TIERS order): the Claude model name Claude Code is told ("" = the real id). A `modelOverrides`
    /// entry sends the real id, so Claude Code's catalog (context window, thinking…) applies — for any endpoint.
    claude_as: [String; 5],
}

impl Provider {
    fn tier_mut(&mut self, t: usize) -> &mut String {
        match t {
            0 => &mut self.default,
            1 => &mut self.fable,
            2 => &mut self.opus,
            3 => &mut self.sonnet,
            _ => &mut self.haiku,
        }
    }
    fn tier(&self, t: usize) -> &str {
        [&self.default, &self.fable, &self.opus, &self.sonnet, &self.haiku][t.min(4)]
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Config {
    providers: Vec<Provider>,
    recents: Vec<String>,
    bypass: bool,
    provider: String,
    /// Claude model ids to spoof as, newest first; refreshed from models.dev on every start.
    claude_models: Vec<String>,
}

/// Offline fallback for `Config::claude_models`.
const CLAUDE_MODELS: [&str; 6] = ["claude-sonnet-5-5", "claude-opus-5-5", "claude-fable-5-1", "claude-opus-4-6", "claude-sonnet-4-6", "claude-haiku-4-5-20251001"];

/// Current Claude model ids, newest first, from models.dev (public, no key).
fn fetch_claude_models() -> Result<Vec<String>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).build().into();
    let body = agent.get("https://models.dev/api.json").call().map_err(|e| e.to_string())?.body_mut().with_config().limit(64 << 20).read_to_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
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

/// Spoof candidates for tier `t`: its own family (Fable rows only Fable…); Default may take any.
fn claude_choices(models: &[String], t: usize) -> Vec<String> {
    let family = TIERS[t].0.to_lowercase();
    let all: Vec<String> = if models.is_empty() { CLAUDE_MODELS.map(String::from).to_vec() } else { models.to_vec() };
    all.into_iter().filter(|m| t == 0 || m.contains(&family)).collect()
}

struct Launch {
    provider: Option<Provider>,
    folder: String,
    bypass: bool,
}

// ---------------------------------------------------------------- storage / network

fn config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("ccat").join("config.json")
}

/// The model list prefixes ids with the vendor ("anthropic/…"); endpoints want the bare id.
fn bare_id(m: &str) -> String {
    let m = m.trim();
    m.strip_prefix("anthropic/").unwrap_or(m).to_string()
}

fn load() -> Config {
    let path = config_path();
    let text = fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let mut cfg: Config = serde_json::from_str(&text).unwrap_or_else(|e| {
        // Don't let the save-on-exit overwrite a file we couldn't read.
        let bad = path.with_extension("json.bad");
        let _ = fs::rename(&path, &bad);
        eprintln!("ccat: {} is invalid ({e}); moved to {}", path.display(), bad.display());
        Config::default()
    });
    // ponytail: one-off migration of ids saved before bare_id existed; cheap, so it just runs every load.
    for p in &mut cfg.providers {
        for m in p.models.iter_mut().chain(p.custom.iter_mut()) {
            *m = bare_id(m);
        }
        p.models.dedup();
        for t in 0..TIERS.len() {
            let v = bare_id(p.tier(t));
            *p.tier_mut(t) = v;
        }
    }
    builtin_antigravity(&mut cfg);
    cfg
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
    if cfg.provider == ag.name {
        cfg.provider = AG_NAME.into();
    }
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
        if cfg.provider == p.name {
            cfg.provider = AG_NAME.into();
        }
    }
    ag.name = AG_NAME.into();
    cfg.providers = std::iter::once(ag).chain(rest).collect();
}

fn save(cfg: &Config) -> Result<(), String> {
    let p = config_path();
    fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(&p, serde_json::to_string_pretty(cfg).unwrap()).map_err(|e| e.to_string())
}

fn models_endpoint(url: &str) -> String {
    let base = url.trim().trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    format!("{base}/v1/models?limit=1000")
}

/// Works for Anthropic-style and OpenAI-style `/v1/models`: both return `{ data: [{ id }] }`.
fn fetch_models(url: &str, key: &str) -> Result<Vec<String>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
    let mut res = agent
        .get(&models_endpoint(url))
        .header("x-api-key", key)
        .header("authorization", format!("Bearer {key}"))
        .header("anthropic-version", "2023-06-01")
        .call()
        .map_err(|e| e.to_string())?;
    let body = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("response is not JSON: {e}"))?;
    let data = v["data"].as_array().ok_or("response has no `data` array")?;
    Ok(data.iter().filter_map(|m| m["id"].as_str().map(bare_id)).collect())
}

/// Env for one provider. An empty tier means "same as default".
fn provider_env(p: &Provider) -> Vec<(&'static str, String)> {
    // Claude Code appends /v1/messages itself, so a pasted ".../v1" base would 404 as ".../v1/v1/messages".
    let url = p.url.trim().trim_end_matches('/');
    let url = url.strip_suffix("/v1").unwrap_or(url);
    let default = bare_id(&p.default);
    let mut env = vec![("ANTHROPIC_BASE_URL", url.to_string()), ("ANTHROPIC_AUTH_TOKEN", p.key.trim().to_string())];
    for (i, (_, var)) in TIERS.iter().enumerate() {
        let m = Some(bare_id(p.tier(i))).filter(|m| !m.is_empty()).unwrap_or_else(|| default.clone());
        if !m.is_empty() {
            env.push((var, m));
        }
    }
    env
}

/// Renames tiers that have a Claude name and returns Claude Code's `modelOverrides` (Claude name → real id).
/// A Claude name already taken by another real model is skipped (first tier wins; the UI flags the clash).
fn spoof_models(p: &mut Provider) -> serde_json::Map<String, serde_json::Value> {
    let default = bare_id(&p.default);
    let mut overrides = serde_json::Map::new();
    for t in 0..TIERS.len() {
        let real = Some(bare_id(p.tier(t))).filter(|m| !m.is_empty()).unwrap_or_else(|| default.clone());
        // Claude Code strips a `[1m]` hint from a model name, but sends an override target verbatim.
        let target = real.split('[').next().unwrap_or("").trim().to_string();
        let name = p.claude_as[t].trim().to_string();
        if name.is_empty() || target.is_empty() || overrides.get(&name).is_some_and(|v| *v != target) {
            continue;
        }
        overrides.insert(name.clone(), target.into());
        *p.tier_mut(t) = name;
    }
    overrides
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
    let faces: [(&str, FontFamily, Option<Vec<u8>>); 6] = [
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
    fonts.families.get_mut(&serif()).unwrap().extend(rest);
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

fn label(ui: &mut egui::Ui, text: &str) {
    let p = pal(ui);
    ui.label(RichText::new(text).size(12.0).color(p.muted).strong());
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
    const BAR: f32 = 32.0;
    const PAD: i8 = 14;
    let p = pal(ui);
    let mut clicked = None;
    egui::Panel::top(egui::Id::new(("titlebar", title)))
        .exact_size(BAR)
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
    let mut used = 0.0;
    egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin { left: PAD, right: PAD, top: 4, bottom: PAD })).show(ui, |ui| {
        used = ui
            .scope(|ui| {
                ui.label(RichText::new(title).family(serif()).size(20.0));
                ui.add_space(4.0);
                body(ui)
            })
            .response
            .rect
            .height();
    });

    // Not resizable, so the window simply follows its content — no dead space at the bottom.
    let screen = ui.ctx().content_rect();
    let want = (BAR + 4.0 + PAD as f32 + used).ceil();
    if (screen.height() - want).abs() > 1.0 {
        ui.send_viewport_cmd(ViewportCommand::InnerSize(vec2(screen.width(), want)));
    }
    ui.ctx()
        .layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("outline")))
        .rect_stroke(screen, 0.0, Stroke::new(1.0, p.border_strong), StrokeKind::Inside);
    clicked
}

/// Toggle switch + label; the whole row is the hit target.
fn switch(ui: &mut egui::Ui, on: &mut bool, text: &str) -> egui::Response {
    let p = pal(ui);
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(13.0), p.fg);
    let track = vec2(30.0, 16.0);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(track.x + 8.0 + galley.size().x, 20.0), Sense::click());
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
    Clone,
    Delete,
    GitHub,
}

/// 28×28 icon button, drawn with strokes (no icon font): fill only on hover.
fn icon_btn(ui: &mut egui::Ui, icon: Icon) -> egui::Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    let hot = resp.hovered();
    let danger = matches!(icon, Icon::Delete);
    if hot {
        ui.painter().rect_filled(rect, 6.0, p.hover);
    }
    let color = match (hot, danger) {
        (true, true) => p.danger,
        (true, false) => p.fg,
        _ => p.muted,
    };
    let (c, s) = (rect.center(), Stroke::new(1.3, color));
    let pt = ui.painter();
    match icon {
        Icon::New => {
            pt.line_segment([c - vec2(5.5, 0.0), c + vec2(5.5, 0.0)], s);
            pt.line_segment([c - vec2(0.0, 5.5), c + vec2(0.0, 5.5)], s);
        }
        Icon::Clone => {
            let r = egui::Rect::from_center_size(c + vec2(1.5, 1.5), vec2(8.0, 8.0));
            pt.rect_stroke(r, 1.5, s, StrokeKind::Middle);
            pt.line_segment([c + vec2(-4.5, 1.5), c + vec2(-4.5, -4.5)], s);
            pt.line_segment([c + vec2(-4.5, -4.5), c + vec2(1.5, -4.5)], s);
        }
        Icon::Delete => {
            pt.line_segment([c + vec2(-5.5, -3.5), c + vec2(5.5, -3.5)], s);
            pt.line_segment([c + vec2(-2.0, -5.5), c + vec2(2.0, -5.5)], s);
            pt.rect_stroke(egui::Rect::from_min_max(c + vec2(-4.0, -3.5), c + vec2(4.0, 5.5)), 1.0, s, StrokeKind::Middle);
        }
        Icon::GitHub => {
            // The GitHub mark, simplified: a disc with the octocat (head, ears, body, tail) cut out in the background colour.
            let cut = if hot { p.hover } else { p.bg };
            pt.circle_filled(c, 7.0, color);
            pt.circle_filled(c + vec2(0.0, -0.6), 3.4, cut);
            pt.add(egui::Shape::convex_polygon(vec![c + vec2(-3.0, -5.2), c + vec2(-0.6, -3.6), c + vec2(-3.4, -1.4)], cut, Stroke::NONE));
            pt.add(egui::Shape::convex_polygon(vec![c + vec2(3.0, -5.2), c + vec2(3.4, -1.4), c + vec2(0.6, -3.6)], cut, Stroke::NONE));
            pt.rect_filled(egui::Rect::from_min_max(c + vec2(-1.7, 2.0), c + vec2(1.7, 7.5)), 0.0, cut);
            pt.line_segment([c + vec2(-1.7, 4.3), c + vec2(-4.2, 2.8)], Stroke::new(1.2, cut));
        }
    }
    resp
}

/// Dropdown rows: hover shows only a fill — a 1px border on the hovered row reads as the text jumping.
fn plain_rows(ui: &mut egui::Ui) {
    let w = &mut ui.visuals_mut().widgets;
    w.hovered.bg_stroke = Stroke::NONE;
    w.active.bg_stroke = Stroke::NONE;
}

// ---------------------------------------------------------------- app

struct App {
    cfg: Config,
    cwd: String,
    settings: bool,
    settings_pos: Option<egui::Pos2>,
    sel: Option<usize>,
    edit: usize,
    folder: String,
    folders: Vec<String>,
    show_key: bool,
    fetch: Option<(usize, mpsc::Receiver<Result<Vec<String>, String>>)>,
    fetch_err: String,
    pick: Option<usize>,
    pick_focus: bool,
    query: String,
    err: String,
    login: Option<LoginJob>,
    login_err: String,
    claude_list: Option<mpsc::Receiver<Result<Vec<String>, String>>>,
    release_rx: Option<mpsc::Receiver<Result<String, String>>>,
    /// Latest GitHub release tag, once the background check is back.
    latest: Option<Result<String, String>>,
    out: Arc<Mutex<Option<Launch>>>,
}

/// A Google sign-in running in the background for provider `provider`.
struct LoginJob {
    provider: usize,
    url: String,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<Result<antigravity::Account, String>>,
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

impl App {
    fn new(cfg: Config, cwd: String, out: Arc<Mutex<Option<Launch>>>, ctx: &egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(fetch_claude_models());
            repaint.request_repaint();
        });
        let (rtx, release_rx) = mpsc::channel();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let _ = rtx.send(fetch_latest_release());
            repaint.request_repaint();
        });
        let sel = cfg.providers.iter().position(|p| p.name == cfg.provider);
        // Most recently launched folder first and preselected; the terminal's cwd stays one click away.
        let mut folders = cfg.recents.clone();
        if !folders.contains(&cwd) {
            folders.push(cwd.clone());
        }
        Self {
            folder: folders[0].clone(),
            folders,
            cfg,
            cwd,
            settings: false,
            settings_pos: None,
            sel,
            edit: 0,
            show_key: false,
            fetch: None,
            fetch_err: String::new(),
            pick: None,
            pick_focus: false,
            query: String::new(),
            err: String::new(),
            login: None,
            login_err: String::new(),
            claude_list: Some(rx),
            release_rx: Some(release_rx),
            latest: None,
            out,
        }
    }

    fn launch(&mut self, ui: &egui::Ui) {
        let provider = self.sel.map(|i| self.cfg.providers[i].clone());
        let missing = provider.as_ref().and_then(|p| match p.antigravity {
            true if p.accounts.is_empty() => Some("needs at least one Google account"),
            // Without one Claude Code asks for its own model names, which Antigravity doesn't serve.
            true if p.default.trim().is_empty() => Some("needs a Default model"),
            false if p.url.trim().is_empty() || p.key.trim().is_empty() => Some("needs a URL and a key"),
            _ => None,
        });
        if let (Some(p), Some(why)) = (provider.as_ref(), missing) {
            self.err = format!("“{}” {why}.", p.name);
            self.edit = self.sel.unwrap_or(0);
            self.settings = true;
            return;
        }
        self.cfg.recents.retain(|f| *f != self.folder);
        self.cfg.recents.insert(0, self.folder.clone());
        self.cfg.recents.truncate(12);
        *self.out.lock().unwrap() = Some(Launch { provider, folder: self.folder.clone(), bypass: self.cfg.bypass });
        ui.send_viewport_cmd(ViewportCommand::Close);
    }

    fn launch_tab(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let p = pal(ui);
        label(ui, "Provider");
        let sel_text = self.sel.map_or(NO_PROVIDER.to_string(), |i| self.cfg.providers[i].name.clone());
        egui::ComboBox::from_id_salt("prov").width(ui.available_width()).selected_text(sel_text).show_ui(ui, |ui| {
            plain_rows(ui);
            ui.selectable_value(&mut self.sel, None, NO_PROVIDER);
            for (i, pr) in self.cfg.providers.iter().enumerate() {
                ui.selectable_value(&mut self.sel, Some(i), &pr.name);
            }
        });
        let prov = self.sel.map(|i| &self.cfg.providers[i]);
        let none_hint = if self.cfg.providers.is_empty() { "Uses your normal Claude login. Add providers in Config." } else { "Uses your normal Claude login and server." };
        let text = prov.map_or(none_hint.to_string(), |p| match p.antigravity {
            true => format!("Google Antigravity · {} account(s), rotated on quota", p.accounts.len()),
            false if p.url.is_empty() => "No URL set".into(),
            false => p.url.clone(),
        });
        hint(ui, &text);
        ui.add_space(6.0);

        label(ui, "Folder");
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
            if ui.add(egui::Button::new("Browse…").min_size(vec2(68.0, 0.0))).clicked() {
                if let Some(d) = rfd::FileDialog::new().set_parent(frame).set_directory(&self.folder).pick_folder() {
                    self.folder = d.display().to_string();
                    if !self.folders.contains(&self.folder) {
                        self.folders.insert(0, self.folder.clone());
                    }
                }
            }
            egui::ComboBox::from_id_salt("folder").width(ui.available_width()).truncate().selected_text(&self.folder).show_ui(ui, |ui| {
            plain_rows(ui);
                for f in &self.folders {
                    let text = if *f == self.cwd { format!("{f}  (current)") } else { f.clone() };
                    ui.selectable_value(&mut self.folder, f.clone(), text);
                }
            });
        });
        ui.add_space(6.0);

        switch(ui, &mut self.cfg.bypass, "Bypass permissions");
        if !self.err.is_empty() {
            ui.add_space(4.0);
            ui.label(RichText::new(&self.err).size(12.0).color(p.danger));
        }

        ui.add_space(12.0);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 28.0), Layout::right_to_left(Align::Center), |ui| {
            let go = egui::Button::new(RichText::new("Launch").color(p.bg).strong()).fill(p.fg).stroke(Stroke::NONE).min_size(vec2(72.0, 28.0));
            if ui.add(go).clicked() {
                self.launch(ui);
            }
            ui.add_space(8.0);
            if ui.add(egui::Button::new(RichText::new("Cancel").color(p.muted)).frame(false)).clicked() {
                ui.send_viewport_cmd(ViewportCommand::Close);
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                // Frame only on hover / while open: fill appears, nothing shifts.
                let cfg = egui::Button::new(RichText::new("Config").color(if self.settings { p.fg } else { p.muted }))
                    .frame_when_inactive(self.settings)
                    .stroke(Stroke::NONE);
                if ui.add(cfg).clicked() {
                    self.settings = !self.settings;
                }
                if icon_btn(ui, Icon::GitHub).on_hover_text(REPO_URL).clicked() {
                    open_browser(REPO_URL);
                }
                let (text, color, tip) = match &self.latest {
                    None => (VERSION.to_string(), p.subtle, "Checking for updates…".to_string()),
                    Some(Ok(tag)) if is_newer(tag, VERSION) => (format!("{VERSION} → {tag}"), p.accent, format!("{tag} is available — open the release")),
                    Some(Ok(_)) => (VERSION.to_string(), p.subtle, "Up to date".to_string()),
                    Some(Err(e)) => (VERSION.to_string(), p.subtle, format!("Could not check for updates: {e}")),
                };
                let v = ui.add(egui::Label::new(RichText::new(text).size(11.5).color(color)).sense(Sense::click()));
                if v.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    open_browser(&format!("{REPO_URL}/releases"));
                }
            });
        });
    }

    fn add_provider(&mut self) {
        let mut name = "Provider".to_string();
        let mut i = 1;
        while self.cfg.providers.iter().any(|p| p.name == name) {
            i += 1;
            name = format!("Provider {i}");
        }
        self.push_provider(Provider { name, ..Default::default() });
    }

    /// Copy of the provider being edited (URL, key, model list, tier picks) under a fresh name.
    fn clone_provider(&mut self) {
        let mut copy = self.cfg.providers[self.edit].clone();
        let base = format!("{} copy", copy.name);
        copy.name = base.clone();
        let mut i = 1;
        while self.cfg.providers.iter().any(|p| p.name == copy.name) {
            i += 1;
            copy.name = format!("{base} {i}");
        }
        self.push_provider(copy);
    }

    fn push_provider(&mut self, p: Provider) {
        self.cfg.providers.push(p);
        self.edit = self.cfg.providers.len() - 1;
        self.fetch_err.clear();
    }

    fn providers_tab(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        // Never empty: `load` always puts the built-in Antigravity profile first.
        self.edit = self.edit.min(self.cfg.providers.len() - 1);

        ui.horizontal(|ui| {
            let w = ui.available_width() - 3.0 * (28.0 + 8.0);
            let cur = self.cfg.providers[self.edit].name.clone();
            egui::ComboBox::from_id_salt("pedit").width(w).selected_text(cur).show_ui(ui, |ui| {
            plain_rows(ui);
                for (i, pr) in self.cfg.providers.iter().enumerate() {
                    if ui.selectable_value(&mut self.edit, i, &pr.name).clicked() {
                        self.fetch_err.clear();
                    }
                }
            });
            if icon_btn(ui, Icon::New).on_hover_text("New endpoint provider").clicked() {
                self.add_provider();
            }
            // The built-in Antigravity profile is one of a kind: no copies, no deleting.
            let builtin = self.cfg.providers[self.edit].antigravity;
            ui.add_enabled_ui(!builtin, |ui| {
                if icon_btn(ui, Icon::Clone).on_hover_text("Clone this provider").clicked() {
                    self.clone_provider();
                }
                // A running sign-in reports back by provider index, so the list must not shift under it.
                if icon_btn(ui, Icon::Delete).on_hover_text("Delete this provider").clicked() && self.login.is_none() {
                    let i = self.edit;
                    self.cfg.providers.remove(i);
                    self.edit = i - 1;
                    self.sel = match self.sel {
                        Some(s) if s == i => None,
                        Some(s) if s > i => Some(s - 1),
                        s => s,
                    };
                }
            })
            .response
            .on_disabled_hover_text("Antigravity is built in: it can't be copied or deleted.");
        });
        ui.add_space(8.0);

        let pr = &mut self.cfg.providers[self.edit];
        if pr.antigravity {
            hint(ui, "Built in: Google Antigravity, served through ccat's local proxy from your Google accounts.");
        } else {
            label(ui, "Name");
            ui.add(TextEdit::singleline(&mut pr.name).desired_width(f32::INFINITY));
            hint(ui, "Anthropic-compatible endpoint.");
        }
        ui.add_space(4.0);
        if pr.antigravity {
            label(ui, "Google accounts");
            if pr.accounts.is_empty() {
                hint(ui, "Add one or more. When an account runs out of quota, requests move to the next.");
            }
            let mut remove = None;
            for (i, a) in pr.accounts.iter().enumerate() {
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                    if icon_btn(ui, Icon::Delete).on_hover_text("Remove this account").clicked() {
                        remove = Some(i);
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.label(RichText::new(&a.email).monospace()).on_hover_text(format!("Cloud Code project {}", a.project));
                    });
                });
            }
            if let Some(i) = remove {
                pr.accounts.remove(i);
            }
            ui.horizontal(|ui| match &self.login {
                Some(job) => {
                    ui.spinner();
                    hint(ui, "Finish signing in in your browser…");
                    if ui.button("Copy link").clicked() {
                        ui.ctx().copy_text(job.url.clone());
                    }
                    if ui.button("Cancel").clicked() {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                }
                None => {
                    if ui.button("Add Google account").clicked() {
                        match antigravity::Login::start() {
                            Ok(login) => {
                                open_browser(&login.url);
                                let (tx, rx) = mpsc::channel();
                                let cancel = Arc::new(AtomicBool::new(false));
                                let (url, stop, ctx) = (login.url.clone(), cancel.clone(), ui.ctx().clone());
                                std::thread::spawn(move || {
                                    let _ = tx.send(login.finish(&stop));
                                    ctx.request_repaint();
                                });
                                self.login = Some(LoginJob { provider: self.edit, url, cancel, rx });
                                self.login_err.clear();
                            }
                            Err(e) => self.login_err = e,
                        }
                    }
                }
            });
            if !self.login_err.is_empty() {
                ui.label(RichText::new(&self.login_err).size(12.0).color(p.danger));
            }
        } else {
            label(ui, "URL");
            ui.add(TextEdit::singleline(&mut pr.url).hint_text("https://api.example.com").desired_width(f32::INFINITY));
            ui.add_space(4.0);
            label(ui, "Key");
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(if self.show_key { "Hide" } else { "Show" }).min_size(vec2(48.0, 0.0))).clicked() {
                    self.show_key = !self.show_key;
                }
                ui.add(TextEdit::singleline(&mut pr.key).password(!self.show_key).desired_width(ui.available_width()));
            });
        }
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            let busy = self.fetch.is_some();
            if ui.add_enabled(!busy, egui::Button::new("Fetch model list")).clicked() {
                if pr.antigravity && pr.accounts.is_empty() {
                    self.fetch_err = "Add a Google account first.".into();
                } else if !pr.antigravity && pr.url.trim().is_empty() {
                    self.fetch_err = "Set a URL first.".into();
                } else {
                    let (tx, rx) = mpsc::channel();
                    let (pr, ctx) = (pr.clone(), ui.ctx().clone());
                    std::thread::spawn(move || {
                        let models = if pr.antigravity { antigravity::list_models(&pr.accounts[0]) } else { fetch_models(&pr.url, &pr.key) };
                        let _ = tx.send(models);
                        ctx.request_repaint();
                    });
                    self.fetch = Some((self.edit, rx));
                    self.fetch_err.clear();
                }
            }
            if busy {
                ui.spinner();
            } else if !self.fetch_err.is_empty() {
                ui.label(RichText::new(&self.fetch_err).size(12.0).color(p.danger));
            } else if pr.models.is_empty() {
                hint(ui, "Not fetched yet — you can still type custom ids.");
            } else {
                hint(ui, &format!("{} models fetched", pr.models.len()));
            }
        });
        ui.add_space(8.0);

        let pick_w = ui.available_width() - 66.0;
        let mut spoofed = pr.clone();
        spoof_models(&mut spoofed);
        egui::Grid::new("tiers").num_columns(2).spacing([10.0, 4.0]).min_col_width(56.0).show(ui, |ui| {
            for (t, (name, env)) in TIERS.iter().enumerate() {
                ui.label(RichText::new(*name).size(12.0).color(p.muted).strong()).on_hover_text(*env);
                let v = pr.tier(t);
                let empty = v.is_empty();
                let text = if !empty { v } else if t == 0 { "Not set — Claude defaults" } else { "Same as default" };
                if picker_btn(ui, pick_w, text, empty).clicked() {
                    self.pick = Some(t);
                    self.pick_focus = true;
                    self.query.clear();
                }
                ui.end_row();

                // Under the model: show Claude Code a Claude name instead, picked from this tier's family.
                ui.label("");
                ui.horizontal(|ui| {
                    let choices = claude_choices(&self.cfg.claude_models, t);
                    let mut on = !pr.claude_as[t].is_empty();
                    if switch(ui, &mut on, "Spoofing").on_hover_text("Claude Code treats this tier as the Claude model you pick (context window, thinking) and sends the real id via modelOverrides.").changed() {
                        pr.claude_as[t] = if on { choices.first().cloned().unwrap_or_default() } else { String::new() };
                    }
                    if on {
                        egui::ComboBox::from_id_salt(("claude_as", t)).width(ui.available_width()).selected_text(&pr.claude_as[t]).show_ui(ui, |ui| {
                            plain_rows(ui);
                            for m in &choices {
                                ui.selectable_value(&mut pr.claude_as[t], m.clone(), m);
                            }
                        });
                    }
                });
                ui.end_row();
                if !pr.claude_as[t].is_empty() && spoofed.tier(t) != pr.claude_as[t] {
                    ui.label("");
                    ui.label(RichText::new(format!("{} already stands for another model; this tier keeps its real id.", pr.claude_as[t])).size(11.5).color(p.danger));
                    ui.end_row();
                }
            }
        });
    }

    fn picker(&mut self, ui: &mut egui::Ui) {
        let Some(t) = self.pick else { return };
        let mut chosen: Option<String> = None;
        let pr = &self.cfg.providers[self.edit];
        let modal_frame = egui::Frame::popup(ui.style()).inner_margin(egui::Margin { left: 18, right: 18, top: 16, bottom: 14 });
        let resp = egui::Modal::new(egui::Id::new("pick")).frame(modal_frame).show(ui.ctx(), |ui| {
            ui.set_width(340.0);
            ui.label(RichText::new("Choose model").family(serif()).size(18.0));
            ui.add_space(2.0);
            hint(ui, &format!("{} · {}", pr.name, TIERS[t].0));
            ui.add_space(10.0);
            let te = ui.add(TextEdit::singleline(&mut self.query).hint_text("Search, or type a custom model id").desired_width(f32::INFINITY));
            if self.pick_focus {
                te.request_focus();
                self.pick_focus = false;
            }
            let enter = te.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));

            let q = self.query.trim();
            let ql = q.to_lowercase();
            let cur = pr.tier(t);
            // (value, text, mono, tag)
            let mut rows: Vec<(String, String, bool, &str)> = Vec::new();
            if q.is_empty() {
                rows.push(match t {
                    0 => (String::new(), "Not set".into(), false, "Claude picks"),
                    _ => (String::new(), "Same as default".into(), false, if pr.default.is_empty() { "" } else { &pr.default }),
                });
            }
            let mut all: Vec<&String> = pr.models.iter().collect();
            all.extend(pr.custom.iter().filter(|c| !pr.models.contains(c)));
            for m in all.iter().filter(|m| m.to_lowercase().contains(&ql)) {
                let tag = if pr.models.contains(m) { "" } else { "custom" };
                rows.push(((*m).clone(), (*m).clone(), true, tag));
            }
            if !q.is_empty() && !all.iter().any(|m| m.as_str() == q) {
                rows.push((q.to_string(), format!("Use “{q}”"), true, "custom"));
            }

            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                if rows.is_empty() {
                    hint(ui, "No models. Fetch the list or type an id.");
                }
                for (v, text, mono, tag) in &rows {
                    if pick_row(ui, v == cur, text, *mono, tag).clicked() {
                        chosen = Some(v.clone());
                    }
                }
            });
            if enter {
                chosen = rows.first().map(|r| r.0.clone());
            }
        });
        if let Some(v) = chosen {
            let pr = &mut self.cfg.providers[self.edit];
            if !v.is_empty() && !pr.models.contains(&v) && !pr.custom.contains(&v) {
                pr.custom.push(v.clone());
            }
            *pr.tier_mut(t) = v;
            self.pick = None;
        } else if resp.should_close() {
            self.pick = None;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if let Some(r) = self.claude_list.as_ref().and_then(|rx| rx.try_recv().ok()) {
            match r {
                Ok(models) if !models.is_empty() => self.cfg.claude_models = models,
                Ok(_) => {}
                Err(e) => eprintln!("ccat: could not refresh Claude models from models.dev ({e}); using the saved list"),
            }
            self.claude_list = None;
        }
        if let Some(r) = self.release_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.latest = Some(r);
            self.release_rx = None;
        }
        if let Some((i, rx)) = &self.fetch {
            if let Ok(r) = rx.try_recv() {
                match r {
                    Ok(models) => {
                        if let Some(pr) = self.cfg.providers.get_mut(*i) {
                            pr.models = models;
                        }
                    }
                    Err(e) => self.fetch_err = e,
                }
                self.fetch = None;
            }
        }
        if let Some((i, r)) = self.login.as_ref().and_then(|j| j.rx.try_recv().ok().map(|r| (j.provider, r))) {
            match r {
                Ok(acc) => {
                    if let Some(p) = self.cfg.providers.get_mut(i) {
                        // Signing in again refreshes that account instead of duplicating it.
                        match p.accounts.iter_mut().find(|a| a.email == acc.email) {
                            Some(a) => *a = acc,
                            None => p.accounts.push(acc),
                        }
                    }
                }
                Err(e) => self.login_err = e,
            }
            self.login = None;
        }
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            ui.send_viewport_cmd(ViewportCommand::Close);
        }
        let typing = ui.ctx().memory(|m| m.focused().is_some());
        if !typing && ui.input(|i| i.key_pressed(Key::Enter)) {
            self.launch(ui);
        }

        match window(ui, "Launch Claude Code with", &[Btn::Close, Btn::Min], |ui| self.launch_tab(ui, frame)) {
            Some(Btn::Close) => ui.send_viewport_cmd(ViewportCommand::Close),
            Some(Btn::Min) => ui.send_viewport_cmd(ViewportCommand::Minimized(true)),
            None => {}
        }
        if self.settings {
            if self.settings_pos.is_none() {
                self.settings_pos = ui.input(|i| i.viewport().outer_rect).map(|r| r.right_top() + vec2(8.0, 0.0));
            }
            self.settings_window(ui.ctx());
        }
    }
}

impl App {
    /// Providers live in their own OS window (an egui immediate viewport).
    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut builder = egui::ViewportBuilder::default()
            .with_title("ccat — Providers")
            .with_icon(app_icon())
            .with_decorations(false)
            .with_resizable(false)
            .with_inner_size([440.0, 480.0]);
        if let Some(pos) = self.settings_pos {
            builder = builder.with_position(pos);
        }
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("providers"), builder, |ui, _| {
            let esc = self.pick.is_none() && ui.input(|i| i.key_pressed(Key::Escape));
            if esc || ui.input(|i| i.viewport().close_requested()) {
                self.settings = false;
            }
            if window(ui, "Providers", &[Btn::Close], |ui| self.providers_tab(ui)) == Some(Btn::Close) {
                self.settings = false;
            }
            self.picker(ui);
        });
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.cfg.provider = self.sel.map(|i| self.cfg.providers[i].name.clone()).unwrap_or_default();
        if let Err(e) = save(&self.cfg) {
            eprintln!("ccat: could not save settings: {e}");
        }
    }
}

// ---------------------------------------------------------------- main / spawn

/// The window/taskbar icon: the repo logo, decoded once (the Providers viewport is rebuilt every frame).
fn app_icon() -> Arc<egui::IconData> {
    static ICON: std::sync::OnceLock<Arc<egui::IconData>> = std::sync::OnceLock::new();
    ICON.get_or_init(|| Arc::new(eframe::icon_data::from_png_bytes(include_bytes!("../icon.png")).expect("icon.png is a valid PNG"))).clone()
}

fn main() {
    // Everything else is forwarded to claude, so these two are the only flags ccat keeps for itself.
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V")) {
        println!("ccat {}", &VERSION[1..]);
        return;
    }
    let cwd =std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let out = Arc::new(Mutex::new(None));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ccat")
            .with_icon(app_icon())
            .with_decorations(false) // titlebar is drawn by App::titlebar
            .with_resizable(false)
            .with_inner_size([480.0, 400.0]),
        centered: true,
        ..Default::default()
    };
    let (app_out, app_cwd) = (out.clone(), cwd.clone());
    let result = eframe::run_native(
        "ccat",
        options,
        Box::new(move |cc| {
            setup(&cc.egui_ctx);
            Ok(Box::new(App::new(load(), app_cwd, app_out, &cc.egui_ctx)))
        }),
    );
    if let Err(e) = result {
        eprintln!("ccat: could not open window: {e}");
        return;
    }
    let Some(launch) = out.lock().unwrap().take() else { return };
    run_claude(launch, &cwd);
}

fn run_claude(l: Launch, cwd: &str) {
    let folder = if l.folder.is_empty() { cwd.to_string() } else { l.folder };
    let mut provider = l.provider;
    let proxied = provider.as_ref().is_some_and(|p| p.antigravity);

    // The proxy's MCP endpoint (Antigravity tools), handed to claude for this launch only.
    let mut mcp_config: Option<PathBuf> = None;
    let real_model = provider.as_ref().map(|p| bare_id(&p.default)).unwrap_or_default();
    // Claude Code sends real ids for Claude-named tiers itself (modelOverrides), so the proxy only needs to know them;
    // any other name Claude Code picks on its own runs the Default model instead of 404ing at Google.
    let real_ids: HashMap<String, String> = provider
        .as_ref()
        .map(|p| (0..TIERS.len()).map(|t| bare_id(p.tier(t))).map(|m| m.split('[').next().unwrap_or("").trim().to_string()).filter(|m| !m.is_empty()).map(|m| (m.clone(), m)).collect())
        .unwrap_or_default();
    let overrides = provider.as_mut().map(spoof_models).unwrap_or_default();
    if let Some(p) = provider.as_mut().filter(|p| p.antigravity) {
        let fallback = Some(real_model.split('[').next().unwrap_or("").trim().to_string()).filter(|m| !m.is_empty());
        match proxy::start(p.accounts.clone(), real_ids, fallback, PathBuf::from(&folder)) {
            Ok(px) => {
                let path = config_path().with_file_name(format!("mcp-{}.json", &px.token[..8]));
                let servers = serde_json::json!({ "mcpServers": { "antigravity": {
                    "type": "http", "url": format!("{}/mcp", px.url), "headers": { "Authorization": format!("Bearer {}", px.token) },
                } } });
                match fs::write(&path, servers.to_string()) {
                    Ok(()) => mcp_config = Some(path),
                    Err(e) => eprintln!("ccat: Antigravity tools unavailable, could not write {}: {e}", path.display()),
                }
                (p.url, p.key) = (px.url, px.token);
            }
            Err(e) => {
                eprintln!("ccat: could not start the Antigravity proxy: {e}");
                std::process::exit(1);
            }
        }
    }
    let env = provider.as_ref().map(provider_env).unwrap_or_default();
    let model = env.iter().find(|(k, _)| *k == "ANTHROPIC_MODEL").map_or("default model", |(_, v)| v.as_str());
    match &provider {
        Some(p) if proxied => {
            let shown = if model == real_model { model.to_string() } else { format!("{model} → {real_model}") };
            eprintln!("ccat → {} (Antigravity, {} account(s)) · {shown} · {folder}", p.name, p.accounts.len());
            eprintln!("       proxy log: {}", antigravity::log_path().display());
            eprintln!("       Antigravity extras: WebSearch → Google Search, MCP tool mcp__antigravity__generate_image");
        }
        Some(p) if model != real_model => eprintln!("ccat → {} · {model} → {real_model} · {folder}", p.name),
        Some(p) => eprintln!("ccat → {} · {model} · {folder}", p.name),
        None => eprintln!("ccat → Claude (default) · {folder}"),
    }

    let build = |program: &str| {
        let mut cmd = Command::new(program);
        cmd.current_dir(&folder);
        if l.bypass {
            cmd.arg("--dangerously-skip-permissions");
        }
        if !overrides.is_empty() {
            cmd.arg("--settings").arg(serde_json::json!({ "modelOverrides": overrides }).to_string());
        }
        cmd.args(std::env::args_os().skip(1)); // `ccat --resume` → `claude --resume`
        // Last on purpose: `--mcp-config` is variadic and would swallow a positional prompt that followed it.
        if let Some(path) = &mcp_config {
            cmd.arg("--mcp-config").arg(path);
        }
        if provider.is_some() {
            cmd.env_remove("ANTHROPIC_API_KEY"); // would win over AUTH_TOKEN and trigger the key-approval prompt
            cmd.envs(env.iter().map(|(k, v)| (*k, v.as_str())));
        }
        cmd
    };

    // The Antigravity proxy lives in this process, so then ccat waits for claude instead of exec-ing it.
    #[cfg(unix)]
    if !proxied {
        use std::os::unix::process::CommandExt;
        let e = build("claude").exec();
        eprintln!("ccat: failed to start claude: {e}");
        std::process::exit(127);
    }

    // Ctrl+C belongs to claude; a handler fn (unlike SIG_IGN) is not inherited by the child.
    #[cfg(windows)]
    let _ = ctrlc::set_handler(|| {});
    let status = build("claude").status().or_else(|_| build("claude.cmd").status());
    if let Some(path) = &mcp_config {
        let _ = fs::remove_file(path);
    }
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
    fn endpoint_normalizes_v1() {
        assert_eq!(models_endpoint("https://x.io/"), "https://x.io/v1/models?limit=1000");
        assert_eq!(models_endpoint("https://x.io/api/v1"), "https://x.io/api/v1/models?limit=1000");
    }

    #[test]
    fn same_as_default_fills_tiers() {
        let p = Provider { url: "https://x.io".into(), key: "k".into(), default: "m-big".into(), haiku: "m-small".into(), ..Default::default() };
        let env = provider_env(&p);
        let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("ANTHROPIC_MODEL"), Some("m-big"));
        assert_eq!(get("ANTHROPIC_DEFAULT_OPUS_MODEL"), Some("m-big"));
        assert_eq!(get("ANTHROPIC_DEFAULT_FABLE_MODEL"), Some("m-big"));
        assert_eq!(get("ANTHROPIC_DEFAULT_HAIKU_MODEL"), Some("m-small"));
        assert_eq!(get("ANTHROPIC_AUTH_TOKEN"), Some("k"));
    }

    #[test]
    fn strips_v1_and_vendor_prefix() {
        let p = Provider { url: "https://openrouter.ai/api/v1/".into(), key: "k".into(), default: "anthropic/stealth/space-bunny-alpha[1m]".into(), ..Default::default() };
        let env = provider_env(&p);
        let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("ANTHROPIC_BASE_URL"), Some("https://openrouter.ai/api"));
        assert_eq!(get("ANTHROPIC_MODEL"), Some("stealth/space-bunny-alpha[1m]"));
        assert_eq!(get("ANTHROPIC_DEFAULT_HAIKU_MODEL"), Some("stealth/space-bunny-alpha[1m]"));
    }

    #[test]
    fn no_default_sets_no_models() {
        let p = Provider { url: "u".into(), key: "k".into(), ..Default::default() };
        assert_eq!(provider_env(&p).len(), 2);
    }

    #[test]
    fn claude_names_become_model_overrides() {
        let mut p = Provider { default: "stealth/space-bunny-alpha[1m]".into(), haiku: "g-small".into(), ..Default::default() };
        assert!(spoof_models(&mut p.clone()).is_empty(), "no Claude names: nothing changes");
        p.claude_as[0] = "claude-opus-5-5".into();
        p.claude_as[4] = "claude-haiku-4-5-20251001".into();
        p.claude_as[2] = "claude-haiku-4-5-20251001".into(); // Opus (same as default) claims it first
        let o = spoof_models(&mut p);
        assert_eq!(o["claude-opus-5-5"], "stealth/space-bunny-alpha", "the [1m] hint is not sent upstream");
        assert_eq!(p.default, "claude-opus-5-5");
        assert_eq!(p.opus, "claude-haiku-4-5-20251001", "Opus (same as default) claimed the name first");
        assert_eq!(p.haiku, "g-small", "a name already standing for another model is skipped");
        assert_eq!(o.len(), 2);
        let env = provider_env(&Provider { url: "u".into(), key: "k".into(), ..p });
        let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
        assert_eq!(get("ANTHROPIC_MODEL").as_deref(), Some("claude-opus-5-5"));
        assert_eq!(get("ANTHROPIC_DEFAULT_SONNET_MODEL").as_deref(), Some("claude-opus-5-5"), "unnamed tiers follow Default");
    }

    #[test]
    fn claude_choices_follow_the_tier_family() {
        let models: Vec<String> = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-1", "claude-haiku-4-5"].map(String::from).to_vec();
        assert_eq!(claude_choices(&models, 1), ["claude-fable-5-1"]);
        assert_eq!(claude_choices(&models, 2), ["claude-opus-5-5"]);
        assert_eq!(claude_choices(&models, 0).len(), 4);
        assert!(claude_choices(&[], 3).iter().all(|m| m.contains("sonnet")), "offline list is used when none was fetched");
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
        let acc = |e: &str| antigravity::Account { email: e.into(), refresh: "r".into(), project: "p".into() };
        // The config this came from: OpenRouter flipped to Antigravity by a stray click on the old Type box.
        let mut cfg = Config {
            provider: "Gehihi".into(),
            providers: vec![
                Provider { name: "OpenRouter".into(), antigravity: true, url: "https://openrouter.ai/api/v1".into(), key: "k".into(), ..Default::default() },
                Provider { name: "Gehihi".into(), antigravity: true, accounts: vec![acc("a@x")], default: "gemini-3.8-flash-tiered".into(), ..Default::default() },
                Provider { name: "Provider 2".into(), ..Default::default() },
                Provider { name: "Spare".into(), antigravity: true, accounts: vec![acc("a@x"), acc("b@x")], ..Default::default() },
                Provider { name: AG_NAME.into(), url: "https://x.io".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        builtin_antigravity(&mut cfg);
        let names: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, [AG_NAME, "OpenRouter", "Provider 2", "Antigravity endpoint"], "built-in first, the rest keep their order");
        let ag = &cfg.providers[0];
        assert!(ag.antigravity && ag.default == "gemini-3.8-flash-tiered", "the profile with accounts became the built-in, tiers kept");
        assert_eq!(ag.accounts.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), ["a@x", "b@x"], "other Antigravity accounts merged, no duplicates");
        assert!(!cfg.providers[1].antigravity && cfg.providers[1].key == "k", "OpenRouter is an endpoint again, key intact");
        assert_eq!(cfg.provider, AG_NAME, "the launch selection follows the rename");
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
    fn old_config_still_loads() {
        let c: Config = serde_json::from_str(r#"{"providers":[{"name":"x","url":"u"}],"bypass":true}"#).unwrap();
        assert!(c.bypass && c.providers[0].default.is_empty());
    }
}
