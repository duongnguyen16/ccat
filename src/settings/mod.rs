//! The Settings window: its own process (`ccat --ccat-settings [page]`), opened from the launcher, the tray menu and
//! the usage popup. A search box and grouped pages on the left, the page on the right; every change is saved the
//! frame it is made, so running ccat sessions follow within seconds, and what another process saves shows up here.
//! One window at a time: asking again brings the open one to the front (`open_settings`).

pub(crate) mod providers;

use crate::providers::{antigravity, chatgpt, claude, endpoint, museai, ollama, Kind};
use crate::{pal, switch, usage, Btn, Config, Disk, TierEditor};
use eframe::egui::{self, vec2, Align, Align2, FontId, Key, Layout, RichText, Sense, Stroke, TextEdit};
use providers::ProvidersEditor;
use std::time::Duration;

/// The window's title, which is also how a second request finds the first (see `win::raise`).
pub(crate) const TITLE: &str = "ccat Settings";
const NAV_W: f32 = 220.0;
/// The widest the page's text and rows go, however wide the window.
const PANE_W: f32 = 720.0;
/// The tier editor's width: its selectors read better narrower than the pane.
const TIERS_W: f32 = 560.0;

#[derive(Clone, Debug, PartialEq)]
enum Page {
    Models,
    Routing,
    Tokens,
    Display,
    About,
    Claude,
    Antigravity,
    ChatGpt,
    Ollama,
    Muse,
    /// A custom endpoint: its index in `Config::providers`.
    Endpoint(usize),
    /// Not a page: the sidebar's "+ Add endpoint", which makes one and goes to it.
    Add,
}

#[derive(Clone, Copy, PartialEq)]
enum Group {
    General,
    Providers,
}

/// One sidebar entry.
struct Item {
    group: Group,
    page: Page,
    label: String,
    /// The provider is turned off.
    off: bool,
}

/// The sidebar's entries for `cfg`: the settings pages, then Claude, the built-in providers and each endpoint.
fn items(cfg: &Config) -> Vec<Item> {
    let general = |page, label: &str| Item { group: Group::General, page, label: label.into(), off: false };
    let mut v = vec![general(Page::Models, "Models & tiers"), general(Page::Routing, "Routing"), general(Page::Tokens, "Token saving"), general(Page::Display, "Display"), general(Page::About, "About")];
    let provider = |page, label: &str, off| Item { group: Group::Providers, page, label: label.into(), off };
    v.push(provider(Page::Claude, "Claude", cfg.claude_off));
    for (kind, page) in [(Kind::Antigravity, Page::Antigravity), (Kind::ChatGpt, Page::ChatGpt), (Kind::Ollama, Page::Ollama), (Kind::Muse, Page::Muse)] {
        if let Some(p) = cfg.providers.iter().find(|p| p.kind() == kind) {
            v.push(provider(page, &p.name, p.disabled));
        }
    }
    for (i, p) in cfg.providers.iter().enumerate().filter(|(_, p)| p.kind() == Kind::Endpoint) {
        v.push(provider(Page::Endpoint(i), &p.name, p.disabled));
    }
    v.push(provider(Page::Add, "+ Add endpoint", false));
    v
}

/// The entries whose label has `query` in it, whatever the case; all of them for an empty query.
fn filter<'a>(items: &'a [Item], query: &str) -> Vec<&'a Item> {
    let q = query.trim().to_lowercase();
    items.iter().filter(|i| q.is_empty() || i.label.to_lowercase().contains(&q)).collect()
}

impl Page {
    /// The page a command-line argument names: a sidebar label, or its first word ("models", "token").
    fn parse(arg: &str, cfg: &Config) -> Option<Page> {
        let a = arg.trim().to_lowercase();
        items(cfg).into_iter().filter(|i| i.page != Page::Add).find(|i| {
            let label = i.label.to_lowercase();
            label == a || label.split_whitespace().next() == Some(a.as_str())
        }).map(|i| i.page)
    }
}

// ---------------------------------------------------------------- widgets

/// A page or provider name, big: what the headings are made of.
pub(crate) fn title(text: &str) -> RichText {
    RichText::new(text).family(crate::serif()).size(22.0)
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(title(text));
    ui.add_space(8.0);
}

/// A titled paragraph of grey text under the rows.
fn paragraph(ui: &mut egui::Ui, head: &str, text: &str) {
    let p = pal(ui);
    ui.add_space(16.0);
    if !head.is_empty() {
        ui.label(RichText::new(head).size(14.5).color(p.fg));
        ui.add_space(3.0);
    }
    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(p.subtle)).wrap());
}

/// One setting: its title and a grey description on the left, the control right-aligned, a thin rule under it.
fn setting(ui: &mut egui::Ui, title: &str, desc: &str, control: impl FnOnce(&mut egui::Ui)) {
    const CONTROL_W: f32 = 200.0;
    const PAD: f32 = 14.0;
    let p = pal(ui);
    let w = ui.available_width();
    let text_w = (w - CONTROL_W - 24.0).max(160.0);
    let t = ui.painter().layout(title.to_string(), FontId::proportional(14.0), p.fg, text_w);
    let d = (!desc.is_empty()).then(|| ui.painter().layout(desc.to_string(), FontId::proportional(12.0), p.subtle, text_w));
    let (th, dh) = (t.size().y, d.as_ref().map_or(0.0, |d| d.size().y + 3.0));
    let (rect, _) = ui.allocate_exact_size(vec2(w, th + dh + 2.0 * PAD), Sense::hover());
    ui.painter().galley(rect.min + vec2(0.0, PAD), t, p.fg);
    if let Some(d) = d {
        ui.painter().galley(rect.min + vec2(0.0, PAD + th + 3.0), d, p.subtle);
    }
    let at = egui::Rect::from_min_max(egui::pos2(rect.right() - CONTROL_W, rect.top()), rect.right_bottom());
    ui.scope_builder(egui::UiBuilder::new().max_rect(at).layout(Layout::right_to_left(Align::Center)), control);
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, p.border));
}

/// A sidebar row: the selected one is a filled, rounded row.
fn nav_item(ui: &mut egui::Ui, text: &str, selected: bool, off: bool) -> egui::Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 7.0, p.hover);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 7.0, p.hover.gamma_multiply(0.5));
    }
    let color = if selected || !off { p.fg } else { p.muted };
    let right = if off { 38.0 } else { 10.0 };
    ui.painter().with_clip_rect(rect.with_max_x(rect.right() - right)).text(rect.left_center() + vec2(12.0, 0.0), Align2::LEFT_CENTER, text, FontId::proportional(13.5), color);
    if off {
        ui.painter().text(rect.right_center() - vec2(10.0, 0.0), Align2::RIGHT_CENTER, "off", FontId::proportional(11.0), p.subtle);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// ---------------------------------------------------------------- the window

struct Settings {
    cfg: Config,
    /// The config file as this window last read or wrote it, so `save` writes back only what this window changed.
    loaded: serde_json::Value,
    disk: Disk,
    page: Page,
    query: String,
    tiers: TierEditor,
    providers: ProvidersEditor,
    /// `usage.json` as the provider pages' limits blocks show it.
    feed: usage::Feed,
}

impl Settings {
    fn new(cfg: Config, loaded: serde_json::Value) -> Self {
        Self { cfg, loaded, disk: Disk::new(), page: Page::Models, query: String::new(), tiers: TierEditor::default(), providers: ProvidersEditor::default(), feed: usage::Feed::new() }
    }

    /// Takes in what another process saved (the launcher, a second Settings, a session's model switch), unless
    /// something is being edited here: then it waits.
    fn sync(&mut self, busy: bool) {
        if self.disk.changed(busy) {
            if let Some((cfg, loaded)) = crate::reload(&self.cfg, &self.loaded) {
                (self.cfg, self.loaded) = (cfg, loaded);
            }
        }
    }

    fn save(&mut self) {
        crate::profiles::sync(&crate::profiles::dir(), &self.cfg);
        if let Err(e) = crate::save(&self.cfg, &self.loaded) {
            eprintln!("ccat: could not save settings: {e}");
        }
        // What's on disk now: the next save only writes what changes after this.
        self.loaded = serde_json::to_value(&self.cfg).unwrap_or_default();
    }

    fn go(&mut self, page: Page) {
        self.tiers.pick = None; // a picker left open would keep Esc from closing the window
        self.page = match page {
            Page::Add => Page::Endpoint(self.providers.add(&mut self.cfg)),
            p => p,
        };
    }

    fn layout(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let p = pal(ui);
        egui::Panel::left("settings-nav")
            .exact_size(NAV_W)
            .resizable(false)
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin { left: 12, right: 12, top: 14, bottom: 8 }))
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin { left: 32, right: 24, top: 18, bottom: 8 })).show(ui, |ui| self.pane(ui, frame));
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        ui.add(TextEdit::singleline(&mut self.query).hint_text("Search").desired_width(f32::INFINITY).margin(egui::Margin::symmetric(10, 6)));
        let all = items(&self.cfg);
        let mut go = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let shown = filter(&all, &self.query);
            for (group, caption) in [(Group::General, "General"), (Group::Providers, "Providers")] {
                let mine: Vec<&&Item> = shown.iter().filter(|i| i.group == group).collect();
                if mine.is_empty() {
                    continue;
                }
                ui.add_space(14.0);
                ui.label(RichText::new(caption).size(11.5).color(p.subtle));
                ui.add_space(2.0);
                for it in mine {
                    if nav_item(ui, &it.label, it.page == self.page, it.off).clicked() {
                        go = Some(it.page.clone());
                    }
                }
            }
            if shown.is_empty() {
                ui.add_space(14.0);
                ui.label(RichText::new("Nothing matches.").size(12.0).color(p.subtle));
            }
        });
        if let Some(page) = go {
            self.go(page);
        }
    }

    fn pane(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        // A page that no longer exists (an endpoint another window deleted) gives way to the first one.
        if let Page::Endpoint(i) = self.page {
            if self.cfg.providers.get(i).is_none_or(|p| p.kind() != Kind::Endpoint) {
                self.page = Page::Models;
            }
        }
        egui::ScrollArea::vertical().id_salt(format!("{:?}", self.page)).auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(PANE_W));
            match self.page.clone() {
                Page::Models => self.models(ui, frame),
                Page::Routing => self.routing(ui),
                Page::Tokens => self.tokens(ui),
                Page::Display => {
                    heading(ui, "Display");
                    usage::display_block(ui, &mut self.cfg);
                }
                Page::About => self.about(ui),
                Page::Claude => claude::page(ui, &mut self.cfg, &self.feed),
                Page::Antigravity => antigravity::page(&mut self.providers, ui, &mut self.cfg, &self.feed),
                Page::ChatGpt => chatgpt::page(&mut self.providers, ui, &mut self.cfg, &self.feed),
                Page::Ollama => ollama::page(&mut self.providers, ui, &mut self.cfg, &self.feed),
                Page::Muse => museai::page(&mut self.providers, ui, &mut self.cfg, &self.feed),
                Page::Endpoint(i) => {
                    if let Some(act) = endpoint::page(&mut self.providers, ui, &mut self.cfg, i) {
                        let deleted = matches!(act, providers::Act::Delete(_));
                        self.providers.apply(&mut self.cfg, act);
                        if deleted {
                            self.page = Page::Claude;
                        }
                    }
                }
                Page::Add => {}
            }
            if matches!(self.page, Page::Claude | Page::Antigravity | Page::ChatGpt | Page::Ollama | Page::Muse | Page::Endpoint(_)) {
                ui.add_space(14.0);
                crate::hint(ui, "Switches reach running ccat sessions within seconds.");
            }
            ui.add_space(20.0);
        });
    }

    fn models(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let p = pal(ui);
        heading(ui, "Models & tiers");
        ui.scope(|ui| {
            ui.set_max_width(ui.available_width().min(TIERS_W));
            self.tiers.bar.show(ui, &mut self.cfg, frame, |ui| {
                ui.label(RichText::new("Configuration").size(14.0));
            });
            ui.add_space(8.0);
            self.tiers.rows(ui, &mut self.cfg);
            ui.add_space(6.0);
            ui.add(egui::Label::new(RichText::new("Changes reach running ccat sessions within seconds, and every next launch. Claude names and context windows stay as each session started. A session launched with every tier on Claude and no fallback runs plain claude: relaunch it to switch.").size(11.0).color(p.subtle)).wrap());
        });
    }

    fn routing(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Routing");
        let mut wait = !self.cfg.fail_on_limit;
        setting(ui, "Wait out usage limits", "When every route and fallback of a request is at its usage limit, the request waits until one can run, so an agent pauses instead of stopping. Esc in Claude Code still cancels it. Routed sessions only.", |ui| {
            switch(ui, &mut wait, "");
        });
        self.cfg.fail_on_limit = !wait;
        setting(ui, "Never fall back to a paid API key", "A tier's fallbacks that are custom Anthropic-compatible endpoints (a paid API key) are skipped; a tier's own first choice still runs. Subscription plans (Claude, Antigravity, ChatGPT, MuseAI) and Ollama stay eligible. Routed sessions only.", |ui| {
            switch(ui, &mut self.cfg.no_paid_fallback, "");
        });
        let mut compact = self.cfg.compact_window > 0;
        setting(ui, "Compact earlier on non-Claude models", "A tier shown to Claude Code under a Claude name gets that model's context window, often 1M, so sessions grow huge and slow before compacting. ccat asks Claude Code to compact at this size instead. Applies to the next launch; the whole session uses it.", |ui| {
            switch(ui, &mut compact, "");
            if compact {
                egui::ComboBox::from_id_salt("compact-window")
                    .selected_text(format!("{}k", self.cfg.compact_window / 1000))
                    .width(70.0)
                    .show_ui(ui, |ui| {
                        for k in [128, 200, 272, 400] {
                            ui.selectable_value(&mut self.cfg.compact_window, k * 1000, format!("{k}k"));
                        }
                    });
            }
        });
        // ponytail: a saved size outside the four choices (hand-edited) shows as is until another is picked.
        if compact != (self.cfg.compact_window > 0) {
            self.cfg.compact_window = if compact { 200_000 } else { 0 };
        }
        paragraph(ui, "Circuit breaker", "A provider that fails 3 times in a row with a 5xx error or a timeout is skipped for 30 seconds, so a request goes straight to the tier's next route instead of waiting on a provider that is down. One request then tries it again. If every route is skipped, they are all tried anyway.");
    }

    fn tokens(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Token saving");
        setting(ui, "Compress tool output (third-party models)", "Lossless cleanup of command output (Bash/PowerShell) before it goes to a third-party model: ANSI colours, progress-bar redraws, trailing spaces, runs of identical lines. Never on Claude, never on error results, never on file contents (Read, Grep). Routed sessions only.", |ui| {
            switch(ui, &mut self.cfg.compress_tool_results, "");
        });
        paragraph(ui, "", "Build logs and test runs (command output) are mostly noise. Cleaning them up before they are sent means fewer tokens used and more room left in the model's context window, without changing what the output says.");
        paragraph(ui, "Prompt cache", "ccat keeps each provider's prompt prefix byte-stable, and Claude request bodies are forwarded exactly as Claude Code sent them, so each provider's own prompt cache keeps hitting. Nothing to set.");
    }

    fn about(&mut self, ui: &mut egui::Ui) {
        heading(ui, "About");
        let latest = &self.cfg.latest;
        let status = if latest.is_empty() {
            "No update check has finished yet.".to_string()
        } else if crate::is_newer(latest, crate::VERSION) {
            format!("{latest} is available.")
        } else {
            format!("Up to date (latest release {latest}).")
        };
        setting(ui, &format!("ccat {}", crate::VERSION), &status, |ui| {
            if ui.button("GitHub").on_hover_text(crate::REPO_URL).clicked() {
                crate::open_browser(crate::REPO_URL);
            }
        });
        let path = crate::config_path();
        setting(ui, "Config file", &path.display().to_string(), |ui| {
            if ui.button("Open folder").clicked() {
                crate::profiles::reveal(path.parent().unwrap_or(&path));
            }
        });
    }
}

impl eframe::App for Settings {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // The file is looked at once a second, and sign-ins and fetches report back through `poll`.
        ctx.request_repaint_after(Duration::from_secs(1));
        let editing = self.providers.active() || self.providers.signing_in() || self.tiers.active() || ctx.egui_wants_keyboard_input();
        self.sync(editing);
        self.feed.poll();
        let before = serde_json::to_value(&self.cfg).unwrap_or_default();
        self.providers.poll(&ctx, &mut self.cfg);
        // Esc belongs to a rename or a picker first, then to the search box, then it closes the window.
        if !self.providers.active() && !self.tiers.active() && ui.input(|i| i.key_pressed(Key::Escape)) {
            if self.query.is_empty() {
                ui.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                self.query.clear();
            }
        }
        if crate::window(ui, "Settings", &[Btn::Close], |ui| self.layout(ui, frame)) == Some(Btn::Close) {
            ui.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        resize_zones(ui);
        if serde_json::to_value(&self.cfg).unwrap_or_default() != before {
            self.save();
        }
    }
}

/// Thin hit zones along the borderless window's edges and corners: the matching resize cursor, and a drag hands
/// the resize to the OS. Corners are registered last so they win over the edges they overlap.
/// ponytail: no zone shrinks for the title bar's close button, whose top 5 px resize instead.
fn resize_zones(ui: &mut egui::Ui) {
    use egui::{pos2, CursorIcon as C, Rect, ResizeDirection as D};
    const E: f32 = 5.0;
    let r = ui.ctx().content_rect();
    let (l, t, rt, b) = (r.left(), r.top(), r.right(), r.bottom());
    let zones = [
        (D::North, C::ResizeVertical, Rect::from_min_max(pos2(l, t), pos2(rt, t + E))),
        (D::South, C::ResizeVertical, Rect::from_min_max(pos2(l, b - E), pos2(rt, b))),
        (D::West, C::ResizeHorizontal, Rect::from_min_max(pos2(l, t), pos2(l + E, b))),
        (D::East, C::ResizeHorizontal, Rect::from_min_max(pos2(rt - E, t), pos2(rt, b))),
        (D::NorthWest, C::ResizeNwSe, Rect::from_min_max(pos2(l, t), pos2(l + 2.0 * E, t + 2.0 * E))),
        (D::NorthEast, C::ResizeNeSw, Rect::from_min_max(pos2(rt - 2.0 * E, t), pos2(rt, t + 2.0 * E))),
        (D::SouthWest, C::ResizeNeSw, Rect::from_min_max(pos2(l, b - 2.0 * E), pos2(l + 2.0 * E, b))),
        (D::SouthEast, C::ResizeNwSe, Rect::from_min_max(pos2(rt - 2.0 * E, b - 2.0 * E), pos2(rt, b))),
    ];
    for (n, (dir, cursor, rect)) in zones.into_iter().enumerate() {
        let resp = ui.interact(rect, ui.id().with(("resize", n)), Sense::drag());
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(cursor);
        }
        if resp.drag_started() {
            ui.send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
        }
    }
}

/// The Settings process: `page` names the one to open on (see `Page::parse`).
pub fn run(page: Option<String>) {
    #[cfg(windows)]
    if !win::claim() {
        win::raise();
        return;
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_icon(crate::app_icon())
            .with_decorations(false)
            .with_resizable(true)
            .with_inner_size([900.0, 620.0])
            .with_min_inner_size([680.0, 440.0]),
        centered: true,
        ..Default::default()
    };
    let _ = eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| {
            crate::setup(&cc.egui_ctx);
            let (mut cfg, mut loaded) = crate::load();
            // First open after an update: the current tiers become the `Default` configuration, saved right away.
            if crate::profiles::ensure(&crate::profiles::dir(), &mut cfg) {
                let _ = crate::save(&cfg, &loaded);
                loaded = serde_json::to_value(&cfg).unwrap_or_default();
            }
            let mut app = Settings::new(cfg, loaded);
            if let Some(p) = page.as_deref().and_then(|a| Page::parse(a, &app.cfg)) {
                app.page = p;
            }
            Ok(Box::new(app))
        }),
    );
}

/// Opens Settings, or brings the open one to the front.
pub fn open_settings() {
    open_settings_at("");
}

/// Same, on the page `page` names (`--ccat-settings <page>`); an open window only comes forward.
pub fn open_settings_at(page: &str) {
    #[cfg(windows)]
    if win::raise() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(usage::SETTINGS_FLAG);
    if !page.is_empty() {
        cmd.arg(page);
    }
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no console, and not killed with the terminal that asked.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
        // SAFETY: the caller was just clicked, so it may hand the foreground on to the new window.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(u32::MAX) };
    }
    let _ = cmd.spawn();
}

#[cfg(windows)]
mod win {
    use super::TITLE;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS},
        System::Threading::CreateMutexW,
        UI::WindowsAndMessaging::{FindWindowW, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE},
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Takes the single-instance mutex; `false` when another Settings process holds it. The handle lives until exit.
    pub fn claim() -> bool {
        // SAFETY: a named mutex; the handle is kept for the process lifetime when it's ours, closed when it isn't.
        unsafe {
            let h = CreateMutexW(std::ptr::null(), 0, wide("Local\\ccat-settings").as_ptr());
            if h.is_null() {
                return true; // can't tell: open anyway
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(h);
                return false;
            }
            true
        }
    }

    /// Restores and focuses the open Settings window, if there is one.
    pub fn raise() -> bool {
        // SAFETY: a top-level window found by its title; restoring and focusing it takes no pointers.
        unsafe {
            let hwnd = FindWindowW(std::ptr::null(), wide(TITLE).as_ptr());
            if hwnd.is_null() {
                return false;
            }
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::CONFIG_FILE;
    use std::{fs, time::Instant};

    fn labels(items: &[&Item]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn the_sidebar_filters_by_label_whatever_the_case() {
        let cfg = crate::from_raw(serde_json::json!({ "providers": [{ "name": "OpenRouter" }] })); // the built-ins come with it
        let all = items(&cfg);
        assert_eq!(filter(&all, "").len(), all.len(), "no query: everything");
        assert_eq!(filter(&all, "  ").len(), all.len(), "blanks don't filter");
        assert_eq!(labels(&filter(&all, "OPENrou")), ["OpenRouter"]);
        assert_eq!(labels(&filter(&all, "tier")), ["Models & tiers"]);
        assert!(filter(&all, "zzz").is_empty());
        let names: Vec<&str> = all.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(names[..5], ["Models & tiers", "Routing", "Token saving", "Display", "About"]);
        assert_eq!(names[5..], ["Claude", "Antigravity", "ChatGPT", "Ollama", "MuseAI", "OpenRouter", "+ Add endpoint"], "Claude, the built-ins, then endpoints by name");
        assert_eq!(Page::parse("models", &cfg), Some(Page::Models), "the first word of a label is enough");
        assert_eq!(Page::parse("openrouter", &cfg), Some(Page::Endpoint(4)));
        assert_eq!(Page::parse("add", &cfg), None);
    }

    /// The two windows share one file: what another process wrote is picked up, an edit not yet saved survives it, and
    /// a later save leaves the fields it didn't change as the other process left them.
    #[test]
    fn reload_does_not_clobber_and_a_save_keeps_what_it_did_not_change() {
        let _files = CONFIG_FILE.lock().unwrap_or_else(|e| e.into_inner());
        let path = crate::config_path();
        let _ = fs::remove_dir_all(path.parent().unwrap());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"recents":["a"]}"#).unwrap();
        let (cfg, loaded) = crate::load();
        let mut s = Settings::new(cfg, loaded);
        let other_process = |json: &str| {
            std::thread::sleep(Duration::from_millis(30)); // a new modified time
            fs::write(&path, json).unwrap();
        };
        let poll = |s: &mut Settings, busy| {
            s.disk.polled = Instant::now() - Duration::from_secs(2);
            s.sync(busy);
        };

        other_process(r#"{"recents":["a"],"bypass":true}"#);
        poll(&mut s, true);
        assert!(!s.cfg.bypass, "mid-edit: the change waits");
        poll(&mut s, false);
        assert!(s.cfg.bypass, "picked up once nothing is being edited");

        s.cfg.recents = vec!["mine".into()];
        other_process(r#"{"recents":["a"],"bypass":true,"fail_on_limit":true}"#);
        poll(&mut s, false);
        assert!(s.cfg.fail_on_limit && s.cfg.bypass, "their newer fields come in");
        assert_eq!(s.cfg.recents, ["mine"], "an edit not saved yet isn't overwritten by the reload");

        s.cfg.no_paid_fallback = true;
        s.save();
        let disk: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((disk["no_paid_fallback"].as_bool(), disk["bypass"].as_bool(), disk["fail_on_limit"].as_bool()), (Some(true), Some(true), Some(true)), "this save keeps theirs");
        assert_eq!(disk["recents"], serde_json::json!(["mine"]));

        // Unreadable file: what is in memory stays, and the file is not moved aside.
        other_process("{ not json");
        poll(&mut s, false);
        assert!(s.cfg.no_paid_fallback && path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
