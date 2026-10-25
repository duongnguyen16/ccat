//! The Settings window: its own process (`ccat --ccat-settings [page]`), opened from the launcher, the tray menu and
//! the usage popup. A search box and grouped pages on the left, the page on the right; every change is saved the
//! frame it is made, so running ccat sessions follow within seconds, and what another process saves shows up here.
//! One window at a time: asking again brings the open one to the front (`open_settings`).

mod glyph;
mod insights;
pub(crate) mod providers;

use crate::providers::{antigravity, chatgpt, claude, endpoint, museai, ollama, Kind};
use crate::{pal, popup_frame, switch, usage, Config, Disk, TierEditor};
use glyph::Glyph;
use eframe::egui::{self, vec2, Align, Align2, FontId, Key, Layout, RichText, Sense, TextEdit};
use providers::ProvidersEditor;
use std::time::Duration;

/// The window's title, which is also how a second request finds the first (see `win::raise`).
pub(crate) const TITLE: &str = "ccat Settings";
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
    Tokens,
    Display,
    About,
    Dashboard,
    Analytics,
    History,
    Claude,
    Antigravity,
    ChatGpt,
    Ollama,
    Muse,
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
        general(Page::Tokens, "Token saving", Glyph::Leaf),
        general(Page::Display, "Display", Glyph::Monitor),
        general(Page::About, "About", Glyph::Info),
        item(Group::Insights, Page::Dashboard, "Dashboard", Glyph::Dashboard, false),
        item(Group::Insights, Page::Analytics, "Analytics", Glyph::Bars, false),
        item(Group::Insights, Page::History, "History", Glyph::Clock, false),
    ];
    let provider = |page, label: &str, glyph, off| item(Group::Providers, page, label, glyph, off);
    v.push(provider(Page::Claude, "Claude", Glyph::Claude, cfg.claude_off));
    for (kind, page, glyph) in [(Kind::Antigravity, Page::Antigravity, Glyph::Antigravity), (Kind::ChatGpt, Page::ChatGpt, Glyph::ChatGpt), (Kind::Ollama, Page::Ollama, Glyph::Ollama), (Kind::Muse, Page::Muse, Glyph::Muse)] {
        if let Some(p) = cfg.providers.iter().find(|p| p.kind() == kind) {
            v.push(provider(page, crate::providers::label(&p.name), glyph, p.disabled));
        }
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
}

impl Settings {
    fn new(cfg: Config, loaded: serde_json::Value) -> Self {
        Self { cfg, loaded, disk: Disk::new(), page: Page::Models, query: String::new(), tiers: TierEditor::default(), providers: ProvidersEditor::default(), feed: usage::Feed::new(), insights: insights::Insights::new(), logo: None, logos: Default::default() }
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
        ui.add(TextEdit::singleline(&mut self.query).hint_text("Search").desired_width(f32::INFINITY).margin(egui::Margin::symmetric(10, 6)));
        let all = items(&self.cfg);
        let mut go = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let shown = filter(&all, &self.query);
            for (group, caption) in [(Group::General, "General"), (Group::Insights, "Insights"), (Group::Providers, "Providers")] {
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
                Page::Tokens => self.tokens(ui),
                Page::Display => {
                    heading(ui, "Display");
                    group(ui, |ui, g| usage::display_block(ui, g, &mut self.cfg));
                }
                Page::About => self.about(ui),
                Page::Dashboard => self.insights.dashboard(ui),
                Page::Analytics => self.insights.analytics(ui),
                Page::History => self.insights.history(ui),
                Page::Claude => claude::page(ui, &mut self.cfg, &self.feed),
                Page::Antigravity => antigravity::page(&mut self.providers, ui, &mut self.cfg, &self.feed),
                Page::ChatGpt => chatgpt::page(&mut self.providers, ui, &mut self.cfg, &self.feed),
                Page::Ollama => ollama::page(&mut self.providers, ui, &mut self.cfg, &self.feed),
                Page::Muse => museai::page(&mut self.providers, ui, &mut self.cfg, &self.feed),
                Page::Endpoint(i) => {
                    if let Some(act) = endpoint::page(&mut self.providers, ui, &mut self.cfg, &self.feed, i) {
                        let deleted = matches!(act, providers::Act::Delete(_));
                        self.providers.apply(&mut self.cfg, act);
                        if deleted {
                            self.page = Page::Claude;
                        }
                    }
                }
                Page::Add => {}
            }
            ui.add_space(20.0);
        });
    }

    fn models(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        let p = pal(ui);
        heading(ui, "Models & tiers");
        ui.scope(|ui| {
            ui.set_max_width(ui.available_width().min(TIERS_W));
            group(ui, |ui, g| {
                g.item(ui, |ui| {
                    self.tiers.bar.show(ui, &mut self.cfg, frame, |ui| {
                        ui.label(RichText::new("Configuration").size(14.0));
                    });
                });
                self.tiers.rows(ui, g, &mut self.cfg);
            });
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
        g.setting(ui, "Never fall back to a paid API key", "A tier's fallbacks that are custom Anthropic-compatible endpoints (a paid API key) are skipped; a tier's own first choice still runs. Subscription plans (Claude, Antigravity, ChatGPT, MuseAI) and Ollama stay eligible. Routed sessions only.", |ui| {
            switch(ui, &mut self.cfg.no_paid_fallback, "");
        });
        g.setting(ui, "Compact earlier on non-Claude models", "A tier shown to Claude Code under a Claude name gets that model's context window, often 1M, so sessions grow huge and slow before compacting. ccat asks Claude Code to compact at this size instead. Applies to the next launch; the whole session uses it.", |ui| {
            switch(ui, &mut compact, "");
            if compact {
                // ponytail: the field names the saved size even when it is none of the four.
                let mut sizes: Vec<(String, u64)> = [128u64, 200, 272, 400].iter().map(|k| (format!("{k}k"), k * 1000)).collect();
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
        // ponytail: a saved size outside the four choices (hand-edited) shows as is until another is picked.
        if compact != (self.cfg.compact_window > 0) {
            self.cfg.compact_window = if compact { 200_000 } else { 0 };
        }
        paragraph(ui, "Circuit breaker", "A provider that fails 3 times in a row with a 5xx error or a timeout is skipped for 30 seconds, so a request goes straight to the tier's next route instead of waiting on a provider that is down. One request then tries it again. If every route is skipped, they are all tried anyway.");
    }

    fn tokens(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Token saving");
        group(ui, |ui, g| g.setting(ui, "Compress tool output (third-party models)", "Lossless cleanup of command output (Bash/PowerShell) before it goes to a third-party model: ANSI colours, progress-bar redraws, trailing spaces, runs of identical lines. Never on Claude, never on error results, never on file contents (Read, Grep). Routed sessions only.", |ui| {
            switch(ui, &mut self.cfg.compress_tool_results, "");
        }));
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
        self.providers.poll(&ctx, &mut self.cfg);
        // Esc belongs to a rename or a picker first, then to the search box, then it closes the window.
        if !self.providers.active() && !self.tiers.active() && !select_open && ui.input(|i| i.key_pressed(Key::Escape)) {
            if self.query.is_empty() {
                ui.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                self.query.clear();
            }
        }
        // No title bar: the layout draws the logo, the drag strips and the close button itself.
        egui::CentralPanel::default().frame(egui::Frame::new().fill(pal(ui).content)).show(ui, |ui| self.layout(ui, frame));
        crate::outline(ui);
        resize_zones(ui);
        if serde_json::to_value(&self.cfg).unwrap_or_default() != before {
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
        assert_eq!(names[..8], ["Models & tiers", "Routing", "Token saving", "Display", "About", "Dashboard", "Analytics", "History"]);
        assert_eq!(names[8..], ["Claude", "Antigravity", "Codex", "Ollama", "MuseAI", "OpenRouter", "Add endpoint"], "Claude, the built-ins, then endpoints by name");
        assert_eq!((Page::parse("codex", &cfg), Page::parse("ChatGPT", &cfg)), (Some(Page::ChatGpt), Some(Page::ChatGpt)), "the old page name still opens it");
        assert_eq!(Page::parse("models", &cfg), Some(Page::Models), "the first word of a label is enough");
        assert_eq!((Page::parse("analytics", &cfg), Page::parse("History", &cfg)), (Some(Page::Analytics), Some(Page::History)));
        assert_eq!(Page::parse("dashboard", &cfg), Some(Page::Dashboard), "--ccat-settings dashboard");
        assert_eq!(Page::parse("openrouter", &cfg), Some(Page::Endpoint(4)));
        assert_eq!(Page::parse("add", &cfg), None);
    }

    /// Only the five built-in providers carry a logo: the white marks are tinted, the brand ones keep their colours; every
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
        assert_eq!(logo("MuseAI"), Some(("Muse-logo.png", false)));
        for plain in ["Models & tiers", "Routing", "Token saving", "Display", "About", "Dashboard", "Analytics", "History", "OpenRouter", "Add endpoint"] {
            assert_eq!(logo(plain), None, "{plain}");
        }
        for g in [Glyph::Claude, Glyph::Antigravity, Glyph::ChatGpt, Glyph::Ollama, Glyph::Muse] {
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
