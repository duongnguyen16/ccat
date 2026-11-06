//! The Settings window: its own process (`ccat --ccat-settings [page]`), opened from the launcher, the tray menu and
//! the usage popup. A search box and grouped pages on the left, the page on the right; every change is saved the
//! frame it is made, so running ccat sessions follow within seconds, and what another process saves shows up here.
//! One window at a time: asking again brings the open one to the front (`open_settings`).

mod glyph;
mod insights;
pub(crate) mod providers;

use crate::providers::{antigravity, chatgpt, claude, endpoint, gemini_web, ollama, Kind};
use crate::{pal, popup_frame, remote, switch, usage, Config, Disk, TierEditor};
use glyph::Glyph;
use eframe::egui::{self, vec2, Align, Align2, FontId, Key, Layout, RichText, Sense, Stroke, TextEdit};
use providers::ProvidersEditor;
use std::{path::PathBuf, time::Duration};

/// The window's title, which is also how a second request finds the first (see `win::raise`).
pub(crate) const TITLE: &str = "ccat Settings";
/// The title of this process's window: the machine it shows is in it, so asking again finds that very window.
pub(crate) fn window_title() -> String {
    remote::attached().map_or_else(|| TITLE.to_string(), |m| format!("{TITLE} - {}", m.name))
}
const NAV_W: f32 = 220.0;
/// The widest the page's text and rows go, however wide the window; the tables of Analytics and History get more.
const PANE_W: f32 = 720.0;
const WIDE_W: f32 = 980.0;
/// The window's top strip, free of content: it drags the window, and the close button sits in its corner.
const TOP_STRIP: f32 = 44.0;
const CLOSE: f32 = 28.0;
/// The tier editor's width: its selectors read better narrower than the pane.
const TIERS_W: f32 = 560.0;
/// Kept free at the pane's right, so switches and percentages never touch the scrollbar.
const SCROLL_GAP: f32 = 7.0;

#[derive(Clone, Debug, PartialEq)]
enum Page {
    Models,
    Routing,
    /// ccat's tiers served to other apps (see `gateway`).
    Gateway,
    Tokens,
    Display,
    Backup,
    About,
    Dashboard,
    Analytics,
    History,
    /// The machines Settings can show (see `remote`), and, on a cloud machine, its terminals.
    Machines,
    Instances,
    Claude,
    Antigravity,
    ChatGpt,
    Ollama,
    GeminiWeb,
    OpenCode(usize),
    /// A custom endpoint: its index in `Config::providers`.
    Endpoint(usize),
    /// Not a page: the sidebar's "Add endpoint", which makes one and goes to it.
    Add,
}

#[derive(Clone, Copy, PartialEq)]
enum Group {
    General,
    Insights,
    Providers,
    /// Only on a cloud machine.
    Cloud,
}

/// One sidebar entry.
struct Item {
    group: Group,
    page: Page,
    label: String,
    glyph: Glyph,
    /// The provider is turned off.
    off: bool,
}

/// The sidebar's entries for `cfg`: the settings pages, then Claude, the built-in providers and each endpoint.
fn items(cfg: &Config) -> Vec<Item> {
    let item = |group, page, label: &str, glyph, off| Item { group, page, label: label.into(), glyph, off };
    let general = |page, label: &str, glyph| item(Group::General, page, label, glyph, false);
    let mut v = vec![
        general(Page::Models, "Models & tiers", Glyph::Sliders),
        general(Page::Routing, "Routing", Glyph::Route),
        general(Page::Gateway, "Gateway", Glyph::Link),
        general(Page::Tokens, "Token saving", Glyph::Leaf),
        general(Page::Display, "Display", Glyph::Monitor),
        general(Page::Backup, "Backup & restore", Glyph::Clock),
        general(Page::About, "About", Glyph::Info),
        general(Page::Machines, "Cloud machines", Glyph::Cloud),
        item(Group::Insights, Page::Dashboard, "Dashboard", Glyph::Dashboard, false),
        item(Group::Insights, Page::Analytics, "Analytics", Glyph::Bars, false),
        item(Group::Insights, Page::History, "History", Glyph::Clock, false),
    ];
    if remote::attached().is_some() {
        v.push(item(Group::Cloud, Page::Instances, "Terminals", Glyph::Terminal, false));
    }
    let provider = |page, label: &str, glyph, off| item(Group::Providers, page, label, glyph, off);
    v.push(provider(Page::Claude, "Claude", Glyph::Claude, cfg.claude_off));
    for (kind, page, glyph) in [
        (Kind::Antigravity, Page::Antigravity, Glyph::Antigravity),
        (Kind::ChatGpt, Page::ChatGpt, Glyph::ChatGpt),
        (Kind::Ollama, Page::Ollama, Glyph::Ollama),
        (Kind::GeminiWeb, Page::GeminiWeb, Glyph::GeminiWeb),
    ] {
        if let Some(p) = cfg.providers.iter().find(|p| p.kind() == kind) {
            v.push(provider(page, crate::providers::label(&p.name), glyph, p.disabled));
        }
    }
    for (i, p) in cfg.providers.iter().enumerate().filter(|(_, p)| p.kind() == Kind::OpenCode) {
        v.push(provider(Page::OpenCode(i), &p.name, Glyph::Terminal, p.disabled));
    }
    for (i, p) in cfg.providers.iter().enumerate().filter(|(_, p)| p.kind() == Kind::Endpoint) {
        v.push(provider(Page::Endpoint(i), &p.name, Glyph::Link, p.disabled));
    }
    v.push(provider(Page::Add, "Add endpoint", Glyph::Plus, false));
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
        let a = if a == "chatgpt" { "codex".to_string() } else { a }; // the page's old name
        items(cfg).into_iter().filter(|i| i.page != Page::Add).find(|i| {
            let label = i.label.to_lowercase();
            label == a || label.split_whitespace().next() == Some(a.as_str())
        }).map(|i| i.page)
    }
}

// ---------------------------------------------------------------- widgets

/// The Settings look, for `ui` and what it holds: buttons and fields are borderless rounded fills (a field shows a blue
/// outline only while it has focus), and bigger than the launcher's. Dropdown panels are their own layer, so they call
/// this themselves.
pub(crate) fn style(ui: &mut egui::Ui) {
    let p = pal(ui);
    let style = ui.style_mut();
    style.spacing.button_padding = vec2(12.0, 5.0);
    style.spacing.interact_size.y = 30.0;
    style.text_styles.insert(egui::TextStyle::Button, FontId::proportional(13.0));
    let v = &mut style.visuals;
    v.extreme_bg_color = p.field;
    // The line egui draws between the side panel and the central panel.
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, p.vsep);
    v.selection.stroke = egui::Stroke::new(1.0, p.blue.gamma_multiply(0.75));
    let hover = p.btn_hover;
    for (w, fill) in [(&mut v.widgets.inactive, p.btn), (&mut v.widgets.hovered, hover), (&mut v.widgets.active, hover), (&mut v.widgets.open, hover)] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = egui::Stroke::NONE;
        w.corner_radius = egui::CornerRadius::same(8);
    }
}

/// A regular text button: a grey rounded fill, no border.
pub(crate) fn button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(13.0))).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The page's main action: the same button on a stronger fill.
pub(crate) fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let p = pal(ui);
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        w.inactive.weak_bg_fill = p.strong;
        w.hovered.weak_bg_fill = p.strong.lerp_to_gamma(p.fg, 0.12);
        w.active.weak_bg_fill = w.hovered.weak_bg_fill;
        button(ui, text)
    })
    .inner
}

/// Set while a `select` is open, and read (and cleared) once a frame: Esc then closes the list before the window.
const SELECT_OPEN: &str = "settings-select-open";

/// Whether a `select` was open in the last frame (see `SELECT_OPEN`).
fn select_open(ctx: &egui::Context) -> bool {
    ctx.data_mut(|d| d.remove_temp::<bool>(egui::Id::new(SELECT_OPEN))).unwrap_or(false)
}

/// A choice field: a rounded fill showing the current option's label with a chevron; a click opens a floating panel
/// under it, right-aligned, whose rows are the `options` (the current one on a lighter fill with a blue check). Esc and a
/// click elsewhere close it. Returns the option picked this frame.
pub(crate) fn select<T: PartialEq + Clone>(ui: &mut egui::Ui, id: &str, width: f32, options: &[(String, T)], cur: &T) -> Option<T> {
    let p = pal(ui);
    let open_id = ui.make_persistent_id(("select", id));
    let mut open = ui.data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false);
    let label = options.iter().find(|(_, v)| v == cur).map_or("", |(l, _)| l.as_str());
    let resp = crate::field_btn(ui, width, label, false, false);
    if resp.clicked() {
        open = !open;
    }
    let mut chosen = None;
    if open {
        ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new(SELECT_OPEN), true));
        let panel_w = width.max(130.0);
        let area = egui::Area::new(open_id.with("panel"))
            .order(egui::Order::Foreground)
            .fixed_pos(resp.rect.right_bottom() + vec2(0.0, 4.0))
            .pivot(Align2::RIGHT_TOP)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                popup_frame(p).show(ui, |ui| {
                    style(ui);
                    ui.set_width(panel_w - 16.0);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for (label, v) in options {
                        if crate::pick_row(ui, v == cur, label, false, "").clicked() {
                            chosen = Some(v.clone());
                        }
                    }
                });
            });
        let outside = ui.input(|i| i.pointer.any_click() && i.pointer.interact_pos().is_some_and(|at| !area.response.rect.contains(at) && !resp.rect.contains(at)));
        if outside || chosen.is_some() || ui.input(|i| i.key_pressed(Key::Escape)) {
            open = false;
        }
    }
    ui.data_mut(|d| d.insert_temp(open_id, open));
    chosen.filter(|v| v != cur)
}

/// A page or provider name, big: what the headings are made of.
pub(crate) fn title(text: &str) -> RichText {
    RichText::new(text).family(crate::serif()).size(22.0)
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(title(text));
    ui.add_space(8.0);
}

/// The corner radius at a group's outer ends, and between neighbouring items.
const BIG: u8 = 12;
const SMALL: u8 = 3;
/// The gap between the items of a group, and after a group.
const ITEM_GAP: f32 = 2.0;
const GROUP_GAP: f32 = 14.0;

/// A grouped list, as in the iOS and macOS settings: its items are cards joined by a 2 px gap, the first rounded at the
/// top, the last at the bottom, those between barely rounded; one item alone is rounded all round. `add` gets the
/// group to put items into (`Cards::item`, `Cards::setting`); a group is followed by a wider gap.
pub(crate) fn group<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui, &mut Cards) -> R) -> R {
    let inner_gap = ui.spacing().item_spacing.y;
    let mut g = Cards { prev: None, first: true, inner_gap, fill: pal(ui).card, any: false };
    let r = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing.y = ITEM_GAP;
            let r = add(ui, &mut g);
            g.paint(ui, true);
            r
        })
        .inner;
    if g.any {
        ui.add_space(GROUP_GAP - ITEM_GAP);
    }
    r
}

/// The items of a `group` so far. Each item's background is a shape reserved before its content; it is filled in once
/// the next item (or the end of the group) says whether the item has a neighbour below it.
pub(crate) struct Cards {
    prev: Option<(egui::layers::ShapeIdx, egui::Rect)>,
    first: bool,
    inner_gap: f32,
    fill: egui::Color32,
    any: bool,
}

impl Cards {
    /// One card of the group: the same padding for all, so everything inside shares one left edge.
    pub(crate) fn item<R>(&mut self, ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
        self.paint(ui, false);
        let idx = ui.painter().add(egui::Shape::Noop);
        let inner_gap = self.inner_gap;
        let out = egui::Frame::new().inner_margin(egui::Margin::symmetric(18, 14)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = inner_gap;
            add(ui)
        });
        self.prev = Some((idx, out.response.rect));
        self.any = true;
        out.inner
    }

    /// An item that is one setting (see `setting_row`).
    pub(crate) fn setting(&mut self, ui: &mut egui::Ui, title: &str, desc: &str, control: impl FnOnce(&mut egui::Ui)) {
        self.setting_w(ui, title, desc, SETTING_W, control);
    }

    /// `setting` with a control column `control_w` wide.
    pub(crate) fn setting_w(&mut self, ui: &mut egui::Ui, title: &str, desc: &str, control_w: f32, control: impl FnOnce(&mut egui::Ui)) {
        self.item(ui, |ui| setting_row(ui, title, desc, control_w, control));
    }

    /// Paints the background of the item before this one; `last`: nothing follows it.
    fn paint(&mut self, ui: &mut egui::Ui, last: bool) {
        if let Some((idx, rect)) = self.prev.take() {
            let (top, bottom) = (if self.first { BIG } else { SMALL }, if last { BIG } else { SMALL });
            let radius = egui::CornerRadius { nw: top, ne: top, sw: bottom, se: bottom };
            ui.painter().set(idx, egui::Shape::rect_filled(rect, radius, self.fill));
            self.first = false;
        }
    }
}

/// A titled paragraph of grey text under the rows.
fn paragraph(ui: &mut egui::Ui, head: &str, text: &str) {
    let p = pal(ui);
    ui.add_space(6.0);
    if !head.is_empty() {
        ui.label(RichText::new(head).size(14.5).color(p.fg));
        ui.add_space(3.0);
    }
    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(p.subtle)).wrap());
}

/// The usual width of a setting's control column.
pub(crate) const SETTING_W: f32 = 200.0;

/// One setting's row: its title and a grey description on the left, the control right-aligned in a `control_w` column.
fn setting_row(ui: &mut egui::Ui, title: &str, desc: &str, control_w: f32, control: impl FnOnce(&mut egui::Ui)) {
    let p = pal(ui);
    let w = ui.available_width();
    let text_w = (w - control_w - 24.0).max(160.0);
    let t = ui.painter().layout(title.to_string(), FontId::proportional(14.0), p.fg, text_w);
    let d = (!desc.is_empty()).then(|| ui.painter().layout(desc.to_string(), FontId::proportional(12.0), p.subtle, text_w));
    let (th, dh) = (t.size().y, d.as_ref().map_or(0.0, |d| d.size().y + 3.0));
    let (rect, _) = ui.allocate_exact_size(vec2(w, (th + dh).max(24.0)), Sense::hover());
    ui.painter().galley(rect.min + vec2(0.0, 0.0), t, p.fg);
    if let Some(d) = d {
        ui.painter().galley(rect.min + vec2(0.0, th + 3.0), d, p.subtle);
    }
    let at = egui::Rect::from_min_max(egui::pos2(rect.right() - control_w, rect.top()), rect.right_bottom());
    ui.scope_builder(egui::UiBuilder::new().max_rect(at).layout(Layout::right_to_left(Align::Center)), control);
}

/// A sidebar row: the selected one is a filled, rounded row.
fn nav_item(ui: &mut egui::Ui, logos: &mut glyph::Logos, text: &str, glyph: Glyph, selected: bool, off: bool) -> egui::Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 7.0, p.hover);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 7.0, p.hover.gamma_multiply(0.5));
    }
    let color = if selected || !off { p.fg } else { p.muted };
    let right = if off { 38.0 } else { 10.0 };
    // The icon and the label share the row's centre line, whatever the font's metrics.
    let at = rect.left_center() + vec2(20.0, 0.0);
    if !logos.paint(ui, at, 18.0, glyph, color, if selected || !off { 1.0 } else { 0.45 }) {
        glyph::paint(ui.painter(), at, glyph, color);
    }
    ui.painter().with_clip_rect(rect.with_max_x(rect.right() - right)).text(rect.left_center() + vec2(38.0, 0.0), Align2::LEFT_CENTER, text, FontId::proportional(13.5), color);
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
    /// The Dashboard, Analytics and History pages' log and filters.
    insights: insights::Insights,
    /// The logo at the top of the sidebar, loaded on first draw.
    logo: Option<egui::TextureHandle>,
    /// The providers' logos in the sidebar.
    logos: glyph::Logos,
    /// The Cloud machines page's form for a machine to add, and what is wrong with it.
    new_machine: remote::Machine,
    machine_err: String,
    /// "Turn off" was clicked once: a second click within a few seconds does it.
    confirm_off: Option<std::time::Instant>,
    backup_msg: String,
    backup_err: bool,
    pending_import: Option<PathBuf>,
    pending_files: usize,
    imported: bool,
}

impl Settings {
    fn new(cfg: Config, loaded: serde_json::Value) -> Self {
        Self { cfg, loaded, disk: Disk::new(), page: Page::Models, query: String::new(), tiers: TierEditor::default(), providers: ProvidersEditor::default(), feed: usage::Feed::new(), insights: insights::Insights::new(), logo: None, logos: Default::default(), new_machine: Default::default(), machine_err: String::new(), confirm_off: None, backup_msg: String::new(), backup_err: false, pending_import: None, pending_files: 0, imported: false }
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
        style(ui);
        let p = pal(ui);
        egui::Panel::left("settings-nav")
            .exact_size(NAV_W)
            .resizable(false)
            .frame(egui::Frame::new().fill(p.side).inner_margin(egui::Margin { left: 12, right: 12, top: 14, bottom: 8 }))
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.content).inner_margin(egui::Margin { left: 32, right: 24, top: 0, bottom: 8 })).show(ui, |ui| {
            // The strip above the page drags the window; the close button floats over its right end.
            let (strip, _) = ui.allocate_exact_size(vec2(ui.available_width(), TOP_STRIP), Sense::hover());
            drag_window(ui, strip, "pane-drag");
            self.pane(ui, frame);
        });
        close_button(ui);
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        // The logo: the window has no title bar, so this corner drags it.
        let logo = self.logo.get_or_insert_with(|| usage::icon_texture(ui.ctx())).clone();
        let (strip, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
        egui::Image::new(&logo).fit_to_exact_size(vec2(28.0, 28.0)).paint_at(ui, egui::Rect::from_min_size(strip.min + vec2(6.0, 0.0), vec2(28.0, 28.0)));
        drag_window(ui, strip, "logo-drag");
        ui.add_space(12.0);
        let mut switch_to = None;
        egui::Panel::bottom("settings-machine").frame(egui::Frame::new().inner_margin(egui::Margin { left: 0, right: 0, top: 8, bottom: 0 })).show(ui, |ui| {
            ui.label(RichText::new("Machine").size(11.5).color(p.subtle));
            ui.add_space(2.0);
            let here = remote::attached().map(|m| m.name.clone()).unwrap_or_default();
            let mut choices: Vec<(String, String)> = vec![("This PC".into(), String::new())];
            choices.extend(remote::machines().into_iter().map(|m| (m.name.clone(), m.name)));
            choices.push(("Add machine...".into(), ADD_MACHINE.into()));
            let width = ui.available_width();
            if let Some(v) = select(ui, "machine", width, &choices, &here) {
                if v == ADD_MACHINE {
                    self.page = Page::Machines;
                } else {
                    switch_to = Some(v);
                }
            }
            if remote::attached().is_some() {
                let status = remote::status();
                let color = if status == "Connected" { crate::profiles::green(ui) } else { p.danger };
                ui.add(egui::Label::new(RichText::new(if status.is_empty() { "Connecting..." } else { &status }).size(11.5).color(color)).truncate());
            }
        });
        ui.add(TextEdit::singleline(&mut self.query).hint_text("Search").desired_width(f32::INFINITY).margin(egui::Margin::symmetric(10, 6)));
        let all = items(&self.cfg);
        let mut go = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let shown = filter(&all, &self.query);
            for (group, caption) in [(Group::General, "General"), (Group::Cloud, "Cloud"), (Group::Insights, "Insights"), (Group::Providers, "Providers")] {
                let mine: Vec<&&Item> = shown.iter().filter(|i| i.group == group).collect();
                if mine.is_empty() {
                    continue;
                }
                ui.add_space(14.0);
                ui.label(RichText::new(caption).size(11.5).color(p.subtle));
                ui.add_space(2.0);
                for it in mine {
                    if nav_item(ui, &mut self.logos, &it.label, it.glyph, it.page == self.page, it.off).clicked() {
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
        if let Some(name) = switch_to {
            switch_machine(&name);
            ui.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// The machines Settings can show, and a form for a new one.
    fn machines(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        heading(ui, "Cloud machines");
        let all = remote::machines();
        let here = remote::attached().map(|m| m.name.clone());
        let mut remove = None;
        if !all.is_empty() {
            group(ui, |ui, g| {
                for m in &all {
                    g.setting(ui, &m.name, &format!("{}{}", m.addr, if here.as_deref() == Some(m.name.as_str()) { "  -  shown now" } else { "" }), |ui| {
                        if button(ui, "Remove").clicked() {
                            remove = Some(m.name.clone());
                        }
                        if here.as_deref() != Some(m.name.as_str()) && button(ui, "Open").clicked() {
                            switch_machine(&m.name);
                        }
                    });
                }
            });
        }
        if let Some(name) = remove {
            let rest: Vec<remote::Machine> = all.iter().filter(|m| m.name != name).cloned().collect();
            if let Err(e) = remote::save_machines(&rest) {
                self.machine_err = e;
            }
        }
        ui.add_space(6.0);
        ui.label(RichText::new("Add a machine").size(14.5).color(p.fg));
        ui.add_space(4.0);
        let m = &mut self.new_machine;
        providers::field(ui, "Name", &mut m.name, egui::Id::new("machine-name"), false, "azure");
        providers::field(ui, "Agent", &mut m.addr, egui::Id::new("machine-addr"), false, "100.84.218.57:8788");
        providers::field(ui, "Token", &mut m.token, egui::Id::new("machine-token"), true, "from `ccat --serve` on that machine");
        providers::field(ui, "SSH", &mut m.ssh, egui::Id::new("machine-ssh"), false, "user@host, for its terminals");
        providers::field(ui, "VM", &mut m.vm, egui::Id::new("machine-vm"), false, "resource-group/name, to turn it on");
        ui.add_space(6.0);
        if primary_button(ui, "Add").clicked() {
            let mut next = all.clone();
            self.machine_err = match remote::check_name(&all, &self.new_machine.name, "") {
                Err(e) => e,
                Ok(_) if self.new_machine.addr.trim().is_empty() || self.new_machine.token.trim().is_empty() => "The agent's address and token are needed.".into(),
                Ok(name) => {
                    next.push(remote::Machine { name, ..std::mem::take(&mut self.new_machine) });
                    remote::save_machines(&next).err().unwrap_or_default()
                }
            };
        }
        if !self.machine_err.is_empty() {
            providers::note(ui, &self.machine_err, p.danger);
        }
        paragraph(ui, "How it works", "A cloud machine runs `ccat --serve` (its agent prints where the token is kept). Pick it in the Machine list at the bottom of the sidebar: every page then shows and edits that machine's settings, and its sessions follow within seconds, as local ones do. Sign-ins (Google, ChatGPT...) stay on the machine they were made on. Terminals of a machine open as SSH windows here.");
    }

    /// Server: on and off, a sleep timer, stay awake. The machine stops by itself (its idle watchdog); this is the
    /// switch for it.
    fn server(&mut self, ui: &mut egui::Ui, m: &remote::Machine) {
        let p = pal(ui);
        let power = remote::power();
        let hhmm = |t: u64| {
            let (_, h, mi) = usage::local(t);
            format!("{h:02}:{mi:02}")
        };
        let (title, desc) = match &power {
            Some(Ok(w)) if !w.supported => ("Server is on".to_string(), "This machine has no sleep watchdog: it stays on until it is turned off.".to_string()),
            Some(Ok(w)) if w.stay_awake => ("Server is on, staying awake".to_string(), format!("It does not sleep by itself; it still stops at {} (23:00 in Vietnam).", hhmm(w.hard_stop))),
            Some(Ok(w)) if w.building => ("Server is on, busy".to_string(), "A build is running; it sleeps 15 minutes after that.".to_string()),
            Some(Ok(w)) => (
                "Server is on".to_string(),
                match w.sleep_at {
                    Some(t) => format!("Sleeps at {} ({} min from now); always at {} (23:00 in Vietnam).", hhmm(t), t.saturating_sub(w.now) / 60, hhmm(w.hard_stop)),
                    None => "Sleep time unknown.".to_string(),
                },
            ),
            Some(Err(e)) if remote::starting() => ("Server is starting...".to_string(), format!("It takes a minute or two. ({e})")),
            Some(Err(e)) => ("Server is off or unreachable".to_string(), e.clone()),
            None => ("Looking...".to_string(), String::new()),
        };
        let on = matches!(power, Some(Ok(_)));
        group(ui, |ui, g| {
            g.setting(ui, &title, &desc, |ui| {
                if on {
                    let armed = self.confirm_off.is_some_and(|t| t.elapsed() < Duration::from_secs(5));
                    if button(ui, if armed { "Confirm: turn off" } else { "Turn off" }).on_hover_text("Stops the machine now; terminals running on it end.").clicked() {
                        if armed {
                            self.confirm_off = None;
                            if let Err(e) = remote::power_set(m, &serde_json::json!({ "action": "sleep_now" })) {
                                self.machine_err = e;
                            }
                        } else {
                            self.confirm_off = Some(std::time::Instant::now());
                        }
                    }
                } else if !remote::starting() && primary_button(ui, "Turn on").clicked() {
                    if let Err(e) = remote::start_vm(m) {
                        self.machine_err = e;
                    }
                }
            });
            if let Some(Ok(w)) = power.as_ref().filter(|w| w.as_ref().is_ok_and(|w| w.supported)) {
                g.setting(ui, "Sleep in", "From now: the machine turns itself off after this long, however it is used.", |ui| {
                    let opts: Vec<(String, u64)> = [("Set a timer", 0), ("15 minutes", 15), ("30 minutes", 30), ("1 hour", 60), ("2 hours", 120), ("4 hours", 240), ("8 hours", 480)].iter().map(|(l, v)| (l.to_string(), *v)).collect();
                    if let Some(minutes) = select(ui, "sleep-timer", 140.0, &opts, &0) {
                        if let Err(e) = remote::power_set(m, &serde_json::json!({ "action": "timer", "minutes": minutes })) {
                            self.machine_err = e;
                        }
                    }
                });
                g.setting(ui, "Stay awake", "No timer: the machine only stops at 23:00 in Vietnam, or when turned off here.", |ui| {
                    let mut stay = w.stay_awake;
                    if switch(ui, &mut stay, "").changed() {
                        if let Err(e) = remote::power_set(m, &serde_json::json!({ "action": "stay", "on": stay })) {
                            self.machine_err = e;
                        }
                    }
                });
            }
        });
        if !self.machine_err.is_empty() {
            providers::note(ui, &self.machine_err, p.danger);
        }
    }

    /// The terminals (tmux sessions) of the machine shown: a click opens one in a window here.
    fn instances(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        let Some(m) = remote::attached() else { return };
        heading(ui, &format!("Terminals on {}", m.name));
        self.server(ui, m);
        let list = remote::instances();
        let existing = match &list {
            Some(Ok((_, l))) => l.clone(),
            _ => Vec::new(),
        };
        group(ui, |ui, g| {
            g.setting(ui, "New terminal", "A new session on the machine, in a window here. Run ccat in it, then close the window to leave it running there.", |ui| {
                if primary_button(ui, "New").clicked() {
                    if let Err(e) = remote::open_terminal(m, &remote::new_session_name(&existing)) {
                        self.machine_err = e;
                    }
                }
            });
        });
        match list {
            None => paragraph(ui, "", "Looking..."),
            Some(Err(e)) => providers::note(ui, &format!("Could not list them: {e}"), p.danger),
            Some(Ok((false, _))) => paragraph(ui, "", "tmux is not installed on that machine, so there are no sessions to show. Install it there (apt install tmux)."),
            Some(Ok((true, l))) if l.is_empty() => paragraph(ui, "", "Nothing is running in tmux there yet. Use New terminal."),
            Some(Ok((true, l))) => group(ui, |ui, g| {
                for s in &l {
                    let what = format!("{}  -  {}{}", s.command, s.path, if s.attached { "  -  open somewhere" } else { "" });
                    g.setting(ui, &s.name, &what, |ui| {
                        if button(ui, "Open").clicked() {
                            if let Err(e) = remote::open_terminal(m, &s.name) {
                                self.machine_err = e;
                            }
                        }
                    });
                }
            }),
        }
        if !self.machine_err.is_empty() {
            providers::note(ui, &self.machine_err, p.danger);
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
            let wide = matches!(self.page, Page::Dashboard | Page::Analytics | Page::History);
            ui.set_max_width((ui.available_width() - SCROLL_GAP).min(if wide { WIDE_W } else { PANE_W }));
            match self.page.clone() {
                Page::Models => self.models(ui, frame),
                Page::Routing => self.routing(ui),
                Page::Gateway => self.gateway(ui),
                Page::Tokens => self.tokens(ui),
                Page::Display => {
                    heading(ui, "Display");
                    group(ui, |ui, g| usage::display_block(ui, g, &mut self.cfg));
                }
                Page::Backup => self.backup(ui, frame),
                Page::About => self.about(ui),
                Page::Dashboard => self.insights.dashboard(ui),
                Page::Analytics => self.insights.analytics(ui),
                Page::History => self.insights.history(ui),
                Page::Claude => claude::page(ui, &mut self.cfg, &self.feed),
                Page::Antigravity => antigravity::page(&mut self.providers, ui, &mut self.cfg, &self.feed),
                Page::ChatGpt => chatgpt::page(&mut self.providers, ui, &mut self.cfg, &self.feed),
                Page::Ollama => ollama::page(&mut self.providers, ui, &mut self.cfg, &self.feed),
                Page::GeminiWeb => gemini_web::page(&mut self.providers, ui, &mut self.cfg, &self.feed),
                Page::OpenCode(i) => {
                    if self.cfg.providers.get(i).is_some_and(|p| p.is_opencode()) {
                        endpoint::page(&mut self.providers, ui, &mut self.cfg, &self.feed, i);
                    }
                }
                Page::Endpoint(i) => {
                    if let Some(act) = endpoint::page(&mut self.providers, ui, &mut self.cfg, &self.feed, i) {
                        let deleted = matches!(act, providers::Act::Delete(_));
                        self.providers.apply(&mut self.cfg, act);
                        if deleted {
                            self.page = Page::Claude;
                        }
                    }
                }
                Page::Machines => self.machines(ui),
                Page::Instances => self.instances(ui),
                Page::Add => {}
            }
            ui.add_space(20.0);
        });
    }

    fn models(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let p = pal(ui);
        ui.scope(|ui| {
            ui.set_max_width(ui.available_width().min(TIERS_W));
            // The page title with the profile selector on its right.
            self.tiers.bar.show(ui, &mut self.cfg, frame, |ui| {
                ui.label(title("Models & tiers"));
            });
            ui.add_space(8.0);
            self.tiers.rows(ui, &mut self.cfg);
            ui.add(egui::Label::new(RichText::new("Changes reach running ccat sessions within seconds, and every next launch. Claude names and context windows stay as each session started. A session launched with every tier on Claude and no fallback runs plain claude: relaunch it to switch.").size(11.0).color(p.subtle)).wrap());
        });
    }

    fn routing(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Routing");
        let mut wait = !self.cfg.fail_on_limit;
        let mut compact = self.cfg.compact_window > 0;
        group(ui, |ui, g| {
        g.setting(ui, "Wait out usage limits", "When every route and fallback of a request is at its usage limit, the request waits until one can run, so an agent pauses instead of stopping. Esc in Claude Code still cancels it. Routed sessions only.", |ui| {
            switch(ui, &mut wait, "");
        });
        if wait {
            g.setting(ui, "Longest wait", "How long Claude Code keeps a held request open before giving up (its API_TIMEOUT_MS). Replaces a shorter timeout inherited from your environment or settings.json; \"Keep mine\" leaves that one. Applies to the next launch.", |ui| {
                let mut holds: Vec<(String, u64)> = vec![("Keep mine".into(), 0), ("1 hour".into(), 1), ("3 hours".into(), 3), ("6 hours".into(), 6), ("12 hours".into(), 12), ("1 day".into(), 24), ("3 days".into(), 72), ("7 days".into(), 168), ("30 days".into(), 720)];
                if !holds.iter().any(|(_, v)| *v == self.cfg.hold_hours) {
                    holds.push((format!("{} hours", self.cfg.hold_hours), self.cfg.hold_hours));
                }
                if let Some(v) = select(ui, "hold-hours", 96.0, &holds, &self.cfg.hold_hours) {
                    self.cfg.hold_hours = v;
                }
            });
        }
        g.setting(ui, "Never fall back to a paid API key", "A tier's fallbacks that are custom Anthropic-compatible endpoints (a paid API key) are skipped; a tier's own first choice still runs. Subscription plans (Claude, Antigravity, ChatGPT), Ollama and OpenCode Free stay eligible. Routed sessions only.", |ui| {
            switch(ui, &mut self.cfg.no_paid_fallback, "");
        });
        g.setting(ui, "Compact earlier on non-Claude models", "A tier shown to Claude Code under a Claude name gets that model's context window, often 1M, so sessions grow huge and slow before compacting. ccat asks Claude Code to compact at this size instead. Applies to the next launch; the whole session uses it.", |ui| {
            switch(ui, &mut compact, "");
            if compact {
                // ponytail: the field names the saved size even when it is none of the listed ones.
                let mut sizes: Vec<(String, u64)> = [100u64, 128, 160, 200, 256, 272, 320, 400, 500, 600, 800].iter().map(|k| (format!("{k}k"), k * 1000)).collect();
                sizes.push(("1M".into(), 1_000_000));
                if !sizes.iter().any(|(_, v)| *v == self.cfg.compact_window) {
                    sizes.push((format!("{}k", self.cfg.compact_window / 1000), self.cfg.compact_window));
                }
                if let Some(v) = select(ui, "compact-window", 84.0, &sizes, &self.cfg.compact_window) {
                    self.cfg.compact_window = v;
                }
            }
        });
        });
        self.cfg.fail_on_limit = !wait;
        // ponytail: a saved size outside the listed choices (hand-edited) shows as is until another is picked.
        if compact != (self.cfg.compact_window > 0) {
            self.cfg.compact_window = if compact { 200_000 } else { 0 };
        }
        paragraph(ui, "Circuit breaker", "A provider that fails 3 times in a row with a 5xx error or a timeout is skipped for 30 seconds, so a request goes straight to the tier's next route instead of waiting on a provider that is down. One request then tries it again. If every route is skipped, they are all tried anyway.");
    }

    fn gateway(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Gateway");
        let p = pal(ui);
        let cfg = &mut self.cfg;
        let defaults: Vec<String> = (0..4).map(|t| crate::gateway::default_name(cfg, t)).collect();
        let routes: Vec<(bool, String)> = cfg.tiers.iter().map(|r| (r.provider.is_empty(), crate::route_text(r))).collect();
        let g = &mut cfg.gateway;
        let base = format!("http://127.0.0.1:{}", g.port);
        group(ui, |ui, c| {
            c.setting(ui, "Serve ccat to other apps", "Other apps and scripts (Cursor, Cline, opencode, your own code) use ccat's tiers through a local URL, OpenAI or Anthropic style, with each tier's fallbacks. The ccat tray runs it.", |ui| {
                switch(ui, &mut g.on, "");
            });
            c.setting(ui, "Port", "On this machine only (127.0.0.1).", |ui| {
                ui.add(egui::DragValue::new(&mut g.port).range(1024..=65535));
            });
            if g.on {
                c.item(ui, |ui| {
                    // The tray says how it went; a stale "listening" (tray gone) shows as not running.
                    let said = crate::gateway::status();
                    let up = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], g.port).into(), Duration::from_millis(150)).is_ok();
                    let (text, color) = match said.strip_prefix("listening on ") {
                        Some(addr) if up && addr.ends_with(&format!(":{}", g.port)) => ("Running".to_string(), p.fg),
                        _ if said.starts_with("cannot") => (said.clone(), p.danger),
                        _ => ("Not running yet: it starts within seconds while the ccat tray runs (tray menu → Restart if it doesn't).".to_string(), p.subtle),
                    };
                    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(color)).wrap());
                    for (label, url) in [("OpenAI base URL", format!("{base}/v1")), ("Anthropic base URL", base.clone())] {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(label).size(12.5).color(p.subtle));
                            ui.label(RichText::new(&url).monospace().size(12.5));
                            if button(ui, "Copy").clicked() {
                                ui.ctx().copy_text(url.clone());
                            }
                        });
                    }
                });
            }
        });
        group(ui, |ui, c| {
            c.setting(ui, "Accept any API key", "Any key, or none, is let in. Anything on this machine can then spend your plans and keys.", |ui| {
                switch(ui, &mut g.any_key, "");
            });
            if !g.any_key {
                let mut remove = None;
                for (i, key) in g.keys.iter_mut().enumerate() {
                    c.item(ui, |ui| {
                        ui.horizontal(|ui| {
                            let id = egui::Id::new(("gateway-key", i));
                            let masked = !ui.memory(|m| m.has_focus(id));
                            ui.add(TextEdit::singleline(key).id(id).password(masked).desired_width((ui.available_width() - 130.0).max(120.0)));
                            if button(ui, "Copy").clicked() {
                                ui.ctx().copy_text(key.clone());
                            }
                            if button(ui, "Remove").clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    g.keys.remove(i);
                }
                c.item(ui, |ui| {
                    if button(ui, "Add key").clicked() {
                        g.keys.push(crate::gateway::new_key());
                    }
                    if g.keys.is_empty() {
                        ui.label(RichText::new("No key yet: every request is refused.").size(12.5).color(p.subtle));
                    }
                });
            }
        });
        group(ui, |ui, c| {
            for (t, (tier, ..)) in crate::TIERS.iter().enumerate() {
                let (claude, route) = &routes[t];
                let desc = if *claude { format!("Runs on Claude ({route}): not served, the gateway has no Claude Code login.") } else { format!("Runs {route}.") };
                c.setting(ui, tier, &desc, |ui| {
                    ui.add(TextEdit::singleline(&mut g.names[t]).hint_text(&defaults[t]).desired_width(ui.available_width()));
                });
            }
        });
        paragraph(ui, "Model names", "The name an app asks for picks the tier; leave it empty for the tier's model id. A tier's key (fable, opus, sonnet, haiku) always works too, and /v1/models lists the names. Which model each tier runs is set in Models & tiers.");
    }

    fn tokens(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Token saving");
        group(ui, |ui, g| g.setting(ui, "Compress tool output (third-party models)", "Lossless cleanup of command output (Bash/PowerShell) before it goes to a third-party model: ANSI colours, progress-bar redraws, trailing spaces, runs of identical lines. Never on Claude, never on error results, never on file contents (Read, Grep). Routed sessions only.", |ui| {
            switch(ui, &mut self.cfg.compress_tool_results, "");
        }));
        paragraph(ui, "", "Build logs and test runs (command output) are mostly noise. Cleaning them up before they are sent means fewer tokens used and more room left in the model's context window, without changing what the output says.");
        paragraph(ui, "Prompt cache", "ccat keeps each provider's prompt prefix byte-stable, and Claude request bodies are forwarded exactly as Claude Code sent them, so each provider's own prompt cache keeps hitting. Nothing to set.");
    }

    fn backup(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        heading(ui, "Backup & restore");
        let p = pal(ui);
        let disabled = remote::attached().is_some() || self.providers.signing_in();

        group(ui, |ui, g| {
            g.setting(ui, "Export backup", "Save accounts, routing profiles, preferences and cloud machines to a JSON file.", |ui| {
                if ui.add_enabled_ui(!disabled, |ui| button(ui, "Export...")).inner.clicked() {
                    let file = rfd::FileDialog::new().set_parent(frame)
                        .set_file_name("ccat-backup.json")
                        .add_filter("ccat backup", &["json"])
                        .save_file();
                    if let Some(path) = file {
                        match crate::backup::export(&path) {
                            Ok(count) => {
                                self.backup_msg = format!("Exported {count} files to {}.", path.file_name().and_then(|n| n.to_str()).unwrap_or("backup"));
                                self.backup_err = false;
                            }
                            Err(e) => {
                                self.backup_msg = e;
                                self.backup_err = true;
                            }
                        }
                    }
                }
            });
            g.setting(ui, "Restore backup", "Inspect and restore matching saved data from a backup JSON file.", |ui| {
                if ui.add_enabled_ui(!disabled, |ui| button(ui, "Restore...")).inner.clicked() {
                    let file = rfd::FileDialog::new().set_parent(frame)
                        .add_filter("ccat backup", &["json"])
                        .pick_file();
                    if let Some(path) = file {
                        match crate::backup::inspect(&path) {
                            Ok(count) => {
                                self.pending_import = Some(path);
                                self.pending_files = count;
                                self.backup_msg.clear();
                                self.backup_err = false;
                            }
                            Err(e) => {
                                self.backup_msg = e;
                                self.backup_err = true;
                                self.pending_import = None;
                            }
                        }
                    }
                }
            });
        });

        if !self.backup_msg.is_empty() {
            let color = if self.backup_err { p.danger } else { crate::profiles::green(ui) };
            providers::note(ui, &self.backup_msg, color);
            ui.add_space(8.0);
        }

        if remote::attached().is_some() {
            providers::note(ui, "Backup and restore are available for local data only.", p.subtle);
            ui.add_space(6.0);
        } else if self.providers.signing_in() {
            providers::note(ui, "Backup and restore are disabled while a provider is signing in.", p.subtle);
            ui.add_space(6.0);
        }

        providers::note(ui, "Warning: unencrypted backup contains API keys and OAuth tokens; keep private.", p.danger);
        ui.add_space(10.0);

        paragraph(ui, "Scope", "Includes Antigravity, ChatGPT/Codex, Ollama and other non-web providers, named routing profiles, preferences, and cloud machines. Gemini Web accounts, cookies and provider settings are excluded and left untouched on restore, including imports from older backups. Also excludes Claude CLI login, environment secrets, browser profiles, and runtime logs or caches.");
        paragraph(ui, "Sessions", "Close other ccat sessions before restoring; relaunch sessions afterward.");

        if let Some(path) = self.pending_import.clone() {
            let mut do_import = false;
            let mut cancel = false;
            let m = egui::Modal::new(egui::Id::new("backup-restore-modal")).show(ui.ctx(), |ui| {
                ui.set_width(320.0);
                style(ui);
                let p = pal(ui);
                ui.label(RichText::new("Replace matching saved data").family(crate::serif()).size(17.0));
                ui.add_space(8.0);
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("backup");
                let files_label = if self.pending_files == 1 { "1 file" } else { &format!("{} files", self.pending_files) };
                ui.add(egui::Label::new(RichText::new(format!(
                    "Restoring “{name}” ({files_label}) will replace matching saved files, including account lists and settings. Accounts absent from those lists will no longer appear. Other local files stay in place."
                )).size(12.0).color(p.fg)).wrap());
                ui.add_space(6.0);
                ui.add(egui::Label::new(RichText::new("Close other ccat sessions before restoring; relaunch sessions afterward.").size(11.5).color(p.subtle)).wrap());
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let go = egui::Button::new(RichText::new("Import").color(p.bg).strong())
                        .fill(p.fg)
                        .stroke(Stroke::NONE)
                        .min_size(vec2(72.0, 30.0));
                    if ui.add(go).clicked() {
                        do_import = true;
                    }
                    if ui.add(egui::Button::new(RichText::new("Cancel").color(p.muted)).frame(false)).clicked() {
                        cancel = true;
                    }
                });
            });
            if cancel || m.should_close() {
                self.pending_import = None;
            } else if do_import {
                self.pending_import = None;
                match crate::backup::import(&path) {
                    Ok(count) => {
                        (self.cfg, self.loaded) = crate::load();
                        self.providers = ProvidersEditor::default();
                        self.tiers = TierEditor::default();
                        self.disk = Disk::new();
                        self.imported = true;
                        self.backup_msg = format!("Restored {count} files. Close other ccat sessions; relaunch sessions afterward.");
                        self.backup_err = false;
                    }
                    Err(e) => {
                        self.backup_msg = e;
                        self.backup_err = true;
                    }
                }
            }
        }
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
        let path = crate::config_path();
        group(ui, |ui, g| {
            g.setting(ui, &format!("ccat {}", crate::VERSION), &status, |ui| {
                if button(ui, "GitHub").on_hover_text(crate::REPO_URL).clicked() {
                    crate::open_browser(crate::REPO_URL);
                }
            });
            g.setting(ui, "Config file", &path.display().to_string(), |ui| {
                if button(ui, "Open folder").clicked() {
                    crate::profiles::reveal(path.parent().unwrap_or(&path));
                }
            });
        });
    }
}

impl eframe::App for Settings {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // The file is looked at once a second, and sign-ins and fetches report back through `poll`.
        ctx.request_repaint_after(Duration::from_secs(1));
        let select_open = select_open(&ctx);
        let editing = self.providers.active() || self.providers.signing_in() || self.tiers.active() || ctx.egui_wants_keyboard_input();
        self.sync(editing);
        self.feed.poll();
        let before = serde_json::to_value(&self.cfg).unwrap_or_default();
        let gpt_mode = |c: &Config| c.providers.iter().find(|p| p.chatgpt).map(|p| p.gpt_legacy());
        let mode = gpt_mode(&self.cfg);
        self.providers.poll(&mut self.cfg);
        // Esc belongs to a rename or a picker first, then to the search box, then it closes the window.
        if !self.providers.active() && !self.tiers.active() && !select_open && ui.input(|i| i.key_pressed(Key::Escape)) {
            if self.pending_import.is_some() {
                self.pending_import = None;
            } else if self.query.is_empty() {
                ui.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                self.query.clear();
            }
        }
        // No title bar: the layout draws the logo, the drag strips and the close button itself.
        egui::CentralPanel::default().frame(egui::Frame::new().fill(pal(ui).content)).show(ui, |ui| self.layout(ui, frame));
        crate::outline(ui);
        resize_zones(ui);
        if self.imported {
            self.imported = false;
        } else if serde_json::to_value(&self.cfg).unwrap_or_default() != before {
            self.save();
        }
        // A finished ChatGPT sign-in or a mode switch makes the stored usage answer (often an error) stale: ask again, now
        // that the config is saved (the refresh reads it from disk).
        if self.providers.take_gpt_signed_in() | (gpt_mode(&self.cfg) != mode) {
            std::thread::spawn(|| {
                usage::refresh_chatgpt();
            });
        }
    }
}

/// `rect` drags the window (the title bar's job, which Settings does not have).
fn drag_window(ui: &mut egui::Ui, rect: egui::Rect, id: &str) {
    if ui.interact(rect, ui.id().with(id), Sense::drag()).drag_started() {
        ui.send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
}

/// The close button: a round target with a thin X, floating at the window's top-right corner over whatever scrolls
/// under it.
fn close_button(ui: &mut egui::Ui) {
    let p = pal(ui);
    let area = egui::Area::new(egui::Id::new("settings-close")).order(egui::Order::Foreground).anchor(Align2::RIGHT_TOP, vec2(-10.0, 8.0)).show(ui.ctx(), |ui| {
        let (rect, resp) = ui.allocate_exact_size(vec2(CLOSE, CLOSE), Sense::click());
        if resp.hovered() {
            ui.painter().circle_filled(rect.center(), CLOSE / 2.0, p.hover);
        }
        let (c, d, color) = (rect.center(), 4.5, if resp.hovered() { p.fg } else { p.muted });
        let stroke = egui::Stroke::new(1.2, color);
        ui.painter().line_segment([c + vec2(-d, -d), c + vec2(d, d)], stroke);
        ui.painter().line_segment([c + vec2(d, -d), c + vec2(-d, d)], stroke);
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Close"));
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    });
    if area.inner.clicked() {
        ui.send_viewport_cmd(egui::ViewportCommand::Close);
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
/// The row of the Machine list that adds a machine.
const ADD_MACHINE: &str = "\u{1}add";

/// Opens Settings for `name` (a machine; "" is this PC), or brings its window to the front.
fn switch_machine(name: &str) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(usage::SETTINGS_FLAG).env_remove(remote::MACHINE_ENV).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    if !name.is_empty() {
        cmd.env(remote::MACHINE_ENV, name);
    }
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0000_0008 | 0x0000_0200);
    let _ = cmd.spawn();
}

pub fn run(page: Option<String>) {
    #[cfg(windows)]
    if !win::claim() {
        win::raise();
        return;
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(window_title())
            .with_icon(crate::app_icon())
            .with_decorations(false)
            .with_resizable(true)
            .with_inner_size([900.0, 620.0])
            .with_min_inner_size([680.0, 440.0]),
        centered: true,
        ..Default::default()
    };
    let _ = eframe::run_native(
        &window_title(),
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
    use super::window_title;
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
            let h = CreateMutexW(std::ptr::null(), 0, wide(&format!("Local\\ccat-settings-{}", window_title())).as_ptr());
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
            let hwnd = FindWindowW(std::ptr::null(), wide(&window_title()).as_ptr());
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
        assert_eq!(labels(&filter(&all, "backup")), ["Backup & restore"]);
        assert!(filter(&all, "zzz").is_empty());
        let names: Vec<&str> = all.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(names[..11], ["Models & tiers", "Routing", "Gateway", "Token saving", "Display", "Backup & restore", "About", "Cloud machines", "Dashboard", "Analytics", "History"]);
        assert_eq!(names[11..], ["Claude", "Antigravity", "Codex", "Ollama", "Gemini Web", "OpenCode Free", "OpenRouter", "Add endpoint"], "Claude, the built-ins, then endpoints by name");
        assert_eq!((Page::parse("codex", &cfg), Page::parse("ChatGPT", &cfg)), (Some(Page::ChatGpt), Some(Page::ChatGpt)), "the old page name still opens it");
        assert_eq!(Page::parse("models", &cfg), Some(Page::Models), "the first word of a label is enough");
        assert_eq!(Page::parse("backup", &cfg), Some(Page::Backup));
        assert_eq!(Page::parse("backup & restore", &cfg), Some(Page::Backup));
        assert_eq!((Page::parse("analytics", &cfg), Page::parse("History", &cfg)), (Some(Page::Analytics), Some(Page::History)));
        assert_eq!(Page::parse("dashboard", &cfg), Some(Page::Dashboard), "--ccat-settings dashboard");
        assert_eq!(Page::parse("openrouter", &cfg), Some(Page::Endpoint(4)));
        assert_eq!(Page::parse("add", &cfg), None);
    }

    #[test]
    fn every_migrated_opencode_provider_remains_visible() {
        let cfg = crate::from_raw(serde_json::json!({ "providers": [
            { "name": "Free A", "url": "https://opencode.ai/inference/openai/v1" },
            { "name": "Free B", "url": "https://opencode.ai/inference/openai/v1" }
        ] }));
        let all = items(&cfg);
        assert_eq!(labels(&filter(&all, "Free")), ["Free A", "Free B"]);
        assert!(all.iter().filter(|i| i.label.starts_with("Free ")).all(|i| matches!(i.page, Page::OpenCode(_))));
    }

    /// Only the four built-in providers carry a logo: the white marks are tinted, the brand ones keep their colours; every
    /// other item (pages, endpoints, "Add endpoint") stays a drawn glyph. The embedded files decode.
    #[test]
    fn provider_items_use_their_logo_and_the_rest_keep_the_glyph() {
        let cfg = crate::from_raw(serde_json::json!({ "providers": [{ "name": "OpenRouter" }] }));
        let logo = |label: &str| {
            let it = items(&cfg).into_iter().find(|i| i.label == label).unwrap();
            glyph::art(it.glyph).map(|a| (a.file, a.mono))
        };
        assert_eq!(logo("Claude"), Some(("claude-code.png", false)));
        assert_eq!(logo("Antigravity"), Some(("google-antigravity.png", false)));
        assert_eq!(logo("Codex"), Some(("openai-chatgpt.png", true)), "stored as ChatGPT, shown as Codex");
        assert_eq!(logo("Ollama"), Some(("ollama-dark.png", true)));
        for plain in ["Models & tiers", "Routing", "Token saving", "Display", "Backup & restore", "About", "Dashboard", "Analytics", "History", "Gemini Web", "OpenRouter", "Add endpoint"] {
            assert_eq!(logo(plain), None, "{plain}");
        }
        for g in [Glyph::Claude, Glyph::Antigravity, Glyph::ChatGpt, Glyph::Ollama] {
            let icon = eframe::icon_data::from_png_bytes(glyph::art(g).unwrap().png).unwrap();
            assert_eq!((icon.width, icon.height), (96, 96), "{g:?}");
            // The decoded mark paints through a real context without a window.
            let ctx = egui::Context::default();
            let mut out = ctx.run_ui(Default::default(), |ui| {
                let mut logos = glyph::Logos::default();
                assert!(logos.paint(ui, egui::pos2(20.0, 20.0), 18.0, g, egui::Color32::WHITE, 0.5), "{g:?}");
                assert!(!logos.paint(ui, egui::pos2(20.0, 20.0), 18.0, Glyph::Link, egui::Color32::WHITE, 1.0));
            });
            out.textures_delta.clear();
        }
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
