//! What the provider pages share: one `ProvidersEditor` holds the background work (model fetches, sign-ins, account
//! checks) and the rename being typed, whichever page is showing; the pages themselves live with their providers
//! (`providers::<x>::page`). Renames and deletes also rewrite the profile files (see `rename_provider`,
//! `delete_provider`); everything else just edits `Config`, and the Settings window saves it.

use crate::{antigravity, chatgpt, check_rename, delete_provider, fetch_models, icon_btn, pal, profiles, rename_provider, Config, Icon, Provider};
use eframe::egui::{self, vec2, Align, Align2, Color32, FontId, Layout, RichText, Sense, TextEdit};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

const ROW: f32 = 28.0;

/// A model list: the ids, and (ChatGPT only) what is known of each.
type Fetched = Result<(Vec<String>, Vec<chatgpt::ChatgptModel>), String>;

/// A sign-in running in the background (the browser round-trip).
pub(crate) struct LoginJob<T> {
    pub(crate) url: String,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) rx: mpsc::Receiver<Result<T, String>>,
}

#[derive(Default)]
pub struct ProvidersEditor {
    /// The endpoint being renamed (index in `Config::providers`) and the text so far; `focus` grabs the keyboard once.
    pub(crate) rename: Option<(usize, String)>,
    pub(crate) focus: bool,
    pub(crate) rename_err: String,
    /// Model-list fetches in flight, by provider name, and the last error per provider.
    fetches: Vec<(String, mpsc::Receiver<Fetched>)>,
    pub(crate) fetch_errs: HashMap<String, String>,
    pub(crate) login: Option<LoginJob<antigravity::Account>>,
    pub(crate) login_err: String,
    pub(crate) gpt_login: Option<LoginJob<chatgpt::ChatgptAccount>>,
    pub(crate) gpt_err: String,
    pub(crate) muse_login: Option<LoginJob<crate::museai::Account>>,
    pub(crate) muse_err: String,
    /// Whether each ChatGPT account (by key) must sign in again and whether the Codex backend refused its token (see
    /// `chatgpt::legacy_refused`), and when that was last looked up: the answer is in a file, so it is read every few
    /// seconds, not every frame.
    gpt_state: HashMap<String, (Instant, bool, bool)>,
    /// A ChatGPT sign-in finished since `take_gpt_signed_in` was last asked: its usage is to be fetched again.
    gpt_signed_in: bool,
    /// Accounts being looked up again (Antigravity by email: its plan; ChatGPT by key), and the answer to come.
    pub(crate) checking: HashMap<String, mpsc::Receiver<Result<String, String>>>,
}

/// What an endpoint's page asks for; `ProvidersEditor::apply` carries it out.
pub(crate) enum Act {
    Delete(usize),
    Rename(usize, String),
}

/// A model-list request on its own thread. Antigravity asks with its first account that's on.
fn start_fetch(ctx: &egui::Context, p: &Provider) -> Result<mpsc::Receiver<Fetched>, String> {
    if p.ollama {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let url = p.url.clone();
        let key = (!p.key.trim().is_empty()).then(|| p.key.clone());
        std::thread::spawn(move || {
            let _ = tx.send(crate::ollama::list_models(&url, key.as_deref()).map(|ids| (ids, Vec::new())));
            ctx.request_repaint();
        });
        return Ok(rx);
    }
    if p.muse {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let url = p.url.clone();
        let key = (!p.key.trim().is_empty()).then(|| p.key.clone());
        std::thread::spawn(move || {
            let _ = tx.send(crate::museai::list_models(&url, key.as_deref()).map(|ids| (ids, Vec::new())));
            ctx.request_repaint();
        });
        return Ok(rx);
    }
    if p.chatgpt {
        let legacy = p.gpt_legacy();
        let key = p.chatgpt_accounts.iter().find(|a| a.usable() && a.runs(legacy)).map(|a| a.key.clone()).ok_or("Sign in with ChatGPT first (and turn the account on).")?;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(chatgpt::list_models(&key, legacy).map(|m| (m.iter().map(|x| x.slug.clone()).collect(), m)));
            ctx.request_repaint();
        });
        return Ok(rx);
    }
    let account = p.accounts.iter().find(|a| !a.disabled).cloned();
    if p.antigravity && p.accounts.is_empty() {
        return Err("Add a Google account first.".into());
    } else if p.antigravity && account.is_none() {
        return Err("Turn on a Google account first.".into());
    } else if !p.antigravity && p.url.trim().is_empty() {
        return Err("Set a URL first.".into());
    }
    let (tx, rx) = mpsc::channel();
    let (p, ctx) = (p.clone(), ctx.clone());
    std::thread::spawn(move || {
        let models = match &account {
            Some(a) if p.antigravity => antigravity::list_models(a),
            _ => fetch_models(&p.url, &p.key),
        }
        .map(|ids| (ids, Vec::new()));
        let _ = tx.send(models);
        ctx.request_repaint();
    });
    Ok(rx)
}

impl<T> Drop for LoginJob<T> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for ProvidersEditor {
    fn drop(&mut self) {
        if let Some(job) = &self.muse_login {
            job.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(job) = &self.login {
            job.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(job) = &self.gpt_login {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// A provider page's header, 40 high: `add` lays out the controls on the right first (right to left: its on/off
/// switch, rename, delete), then the provider's `name`.
pub(crate) fn head(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let w = ui.available_width();
    ui.allocate_ui_with_layout(vec2(w, 40.0), Layout::right_to_left(Align::Center), |ui| {
        ui.set_min_size(vec2(w, 40.0));
        add(ui);
    });
    ui.add_space(4.0);
}

/// The provider's name as the page's heading, greyed while it is off.
pub(crate) fn name(ui: &mut egui::Ui, text: &str, on: bool) {
    let p = pal(ui);
    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
        ui.add(egui::Label::new(super::title(text).color(if on { p.fg } else { p.muted })).truncate());
    });
}

/// The section header's left end: its title, then how many models the provider offers in grey.
pub(crate) fn count(ui: &mut egui::Ui, title: &str, n: usize) {
    let p = pal(ui);
    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
        ui.label(RichText::new(title).size(14.5).color(p.fg));
        let text = if n == 0 { "· No models yet — fetch the list".to_string() } else { format!("· {n} model{}", if n == 1 { "" } else { "s" }) };
        ui.label(RichText::new(text).size(12.0).color(p.subtle));
    });
}

/// One row of `ROW` height. `add` lays out the right-hand controls first (right to left), then a left-to-right
/// layout takes what's left; `hot` says the pointer is over the row, which is when a trash icon shows.
pub(crate) fn row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui, bool)) {
    let w = ui.available_width();
    let hot = ui.rect_contains_pointer(egui::Rect::from_min_size(ui.cursor().min, vec2(w, ROW)));
    ui.allocate_ui_with_layout(vec2(w, ROW), Layout::right_to_left(Align::Center), |ui| {
        ui.set_min_size(vec2(w, ROW));
        add(ui, hot);
    });
}

/// The row's trash slot: the icon while hovered, else the same space left empty so nothing shifts.
pub(crate) fn trash(ui: &mut egui::Ui, hot: bool, tip: &str) -> bool {
    if hot {
        icon_btn(ui, Icon::Delete).on_hover_text(tip).clicked()
    } else {
        ui.allocate_space(vec2(28.0, 28.0));
        false
    }
}

/// The row's reload slot, left of the trash: while hovered (or busy), else the same space left empty.
pub(crate) fn reload(ui: &mut egui::Ui, hot: bool, busy: bool, tip: &str) -> bool {
    if busy {
        ui.add_sized([28.0, 28.0], egui::Spinner::new().size(13.0));
        false
    } else if hot {
        icon_btn(ui, Icon::Reload).on_hover_text(tip).clicked()
    } else {
        ui.allocate_space(vec2(28.0, 28.0));
        false
    }
}

/// The plan, small and rounded with a faint green tint.
pub(crate) fn badge(ui: &mut egui::Ui, text: &str, on: bool) -> Option<egui::Response> {
    if text.is_empty() {
        return None;
    }
    let (green, fade) = (profiles::green(ui), if on { 1.0 } else { 0.5 });
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(10.5), green);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(10.0, 4.0), Sense::click());
    ui.painter().rect_filled(rect, 8.0, green.gamma_multiply(0.16 * fade));
    ui.painter().galley_with_override_text_color(rect.center() - galley.size() / 2.0, galley, green.gamma_multiply(fade));
    Some(resp)
}

/// A line of small text under a row or a card, on the same left edge as what it sits in.
pub(crate) fn note(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.add(egui::Label::new(RichText::new(text).size(11.5).color(color)).wrap());
}

/// `label` and a text field; a secret shows as dots unless it has the keyboard.
pub(crate) fn field(ui: &mut egui::Ui, label: &str, text: &mut String, id: egui::Id, secret: bool, hint: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(52.0, 24.0), Sense::hover());
        ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, label, FontId::proportional(11.5), p.subtle);
        let masked = secret && !ui.memory(|m| m.has_focus(id));
        ui.add(TextEdit::singleline(text).id(id).password(masked).hint_text(hint).desired_width(ui.available_width()));
    });
}

impl ProvidersEditor {
    /// A rename is being typed: Esc and Enter belong to it.
    pub fn active(&self) -> bool {
        self.rename.is_some()
    }

    /// A sign-in is waiting on the browser, which takes focus away from the window.
    pub fn signing_in(&self) -> bool {
        self.login.is_some() || self.gpt_login.is_some() || self.muse_login.is_some()
    }

    /// (must sign in again, Codex backend refused its token) of a ChatGPT account, looked up at most every 3 seconds.
    pub(crate) fn gpt_needs(&mut self, key: &str) -> (bool, bool) {
        let fresh = self.gpt_state.get(key).filter(|(at, ..)| at.elapsed() < Duration::from_secs(3));
        match fresh {
            Some((_, a, b)) => (*a, *b),
            None => {
                let v = (chatgpt::needs_signin(key), chatgpt::legacy_refused(key));
                self.gpt_state.insert(key.to_string(), (Instant::now(), v.0, v.1));
                v
            }
        }
    }

    /// Starts a model-list fetch for `p`; its answer lands in `poll`, an error in `fetch_errs`.
    pub(crate) fn fetch(&mut self, ctx: &egui::Context, p: &Provider) {
        match start_fetch(ctx, p) {
            Ok(rx) => {
                self.fetches.push((p.name.clone(), rx));
                self.fetch_errs.remove(&p.name);
            }
            Err(e) => {
                self.fetch_errs.insert(p.name.clone(), e);
            }
        }
    }

    /// Takes in finished model lists and sign-ins; call it every frame, whether or not a provider page is showing.
    /// Whether a ChatGPT sign-in finished since the last call.
    pub(crate) fn take_gpt_signed_in(&mut self) -> bool {
        std::mem::take(&mut self.gpt_signed_in)
    }

    pub fn poll(&mut self, ctx: &egui::Context, cfg: &mut Config) {
        // Accounts looked up again: an Antigravity plan as it is now.
        let mut plans = Vec::new();
        self.checking.retain(|who, rx| match rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => true,
            Ok(r) => {
                plans.push((who.clone(), r));
                false
            }
            Err(_) => false,
        });
        for (who, r) in plans {
            match r {
                Ok(plan) => {
                    if let Some(a) = cfg.providers.iter_mut().filter(|p| p.antigravity).flat_map(|p| p.accounts.iter_mut()).find(|a| a.email == who) {
                        a.plan = plan;
                    }
                }
                Err(e) => {
                    self.fetch_errs.insert(crate::AG_NAME.to_string(), format!("{}: {e}", who.split('@').next().unwrap_or(&who)));
                }
            }
        }
        let mut done = Vec::new();
        self.fetches.retain(|(name, rx)| match rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => true,
            Ok(r) => {
                done.push((name.clone(), r));
                false
            }
            Err(_) => false,
        });
        for (name, r) in done {
            match (r, cfg.providers.iter_mut().find(|p| p.name == name)) {
                (Ok((models, meta)), Some(p)) => {
                    p.models = models;
                    if p.chatgpt {
                        // What the answer leaves out (the official `/models` may carry no names or levels) stays as the last fetch knew it.
                        let old = std::mem::replace(&mut p.chatgpt_models, meta);
                        for m in &mut p.chatgpt_models {
                            if let Some(o) = old.iter().find(|o| o.slug == m.slug) {
                                if m.name.is_empty() {
                                    m.name = o.name.clone();
                                }
                                if m.default_effort.is_empty() {
                                    m.default_effort = o.default_effort.clone();
                                }
                                if m.efforts.is_empty() {
                                    m.efforts = o.efforts.clone();
                                }
                            }
                        }
                    }
                    self.fetch_errs.remove(&name);
                }
                (Err(e), _) => {
                    self.fetch_errs.insert(name, e);
                }
                _ => {}
            }
        }
        if let Some(r) = self.login.as_ref().and_then(|j| j.rx.try_recv().ok()) {
            match r {
                Ok(acc) => {
                    if let Some(p) = cfg.providers.iter_mut().find(|p| p.antigravity) {
                        // Signing in again refreshes that account (and keeps its switch) instead of duplicating it.
                        match p.accounts.iter_mut().find(|a| a.email == acc.email) {
                            Some(a) => *a = antigravity::Account { disabled: a.disabled, ..acc },
                            None => p.accounts.push(acc),
                        }
                    }
                }
                Err(e) => self.login_err = e,
            }
            self.login = None;
        }
        if let Some(r) = self.gpt_login.as_ref().and_then(|j| j.rx.try_recv().ok()) {
            match r {
                Ok(acc) => {
                    if let Some(p) = cfg.providers.iter_mut().find(|p| p.chatgpt) {
                        // Signing in again refreshes that account (and keeps its switch) instead of duplicating it; a leftover
                        // Codex CLI import is replaced by any new sign-in.
                        p.chatgpt_accounts.retain(|a| !a.cli());
                        match p.chatgpt_accounts.iter_mut().find(|a| a.key == acc.key) {
                            Some(a) => *a = chatgpt::ChatgptAccount { disabled: a.disabled, ..acc },
                            None => p.chatgpt_accounts.push(acc),
                        }
                    }
                    self.gpt_state.clear();
                    self.gpt_signed_in = true;
                    if cfg.chatgpt_note == 0 {
                        cfg.chatgpt_note = 1;
                    }
                }
                Err(e) => self.gpt_err = e,
            }
            self.gpt_login = None;
        }
        if let Some(r) = self.muse_login.as_ref().and_then(|j| j.rx.try_recv().ok()) {
            match r {
                Ok(acc) => {
                    if let Some(p) = cfg.providers.iter_mut().find(|p| p.muse) {
                        p.muse_account = Some(acc);
                        p.disabled = false;
                        let p = p.clone();
                        self.fetch(ctx, &p);
                    }
                    self.muse_err.clear();
                    std::thread::spawn(|| {
                        crate::usage::refresh(false, false, false, false, true);
                    });
                }
                Err(e) => self.muse_err = e,
            }
            self.muse_login = None;
        }
    }

    /// The fetch button of a provider's toolbar, or a spinner while its list is coming.
    pub(crate) fn fetch_btn(&mut self, ui: &mut egui::Ui, p: &Provider) {
        if self.fetches.iter().any(|(n, _)| *n == p.name) {
            ui.add_sized([28.0, 28.0], egui::Spinner::new().size(14.0));
        } else if icon_btn(ui, Icon::Fetch).on_hover_text("Fetch model list").clicked() {
            self.fetch(ui.ctx(), p);
        }
    }

    /// A new endpoint, named and ready to be renamed; its index in `Config::providers`.
    pub fn add(&mut self, cfg: &mut Config) -> usize {
        let names: Vec<String> = cfg.providers.iter().map(|p| p.name.clone()).collect();
        let name = profiles::unique(&names, "Provider");
        cfg.providers.push(Provider { name: name.clone(), ..Default::default() });
        (self.rename, self.focus, self.rename_err) = (Some((cfg.providers.len() - 1, name)), true, String::new());
        cfg.providers.len() - 1
    }

    /// Carries out what an endpoint's page asked for.
    pub fn apply(&mut self, cfg: &mut Config, act: Act) {
        match act {
            Act::Delete(i) => {
                self.fetch_errs.remove(&cfg.providers[i].name);
                delete_provider(cfg, &profiles::dir(), i);
                (self.rename, self.rename_err) = (None, String::new());
            }
            Act::Rename(i, new) => {
                let old = cfg.providers[i].name.clone();
                rename_provider(cfg, &profiles::dir(), i, &new);
                for (n, _) in self.fetches.iter_mut().filter(|(n, _)| *n == old) {
                    *n = new.clone();
                }
                if let Some(e) = self.fetch_errs.remove(&old) {
                    self.fetch_errs.insert(new, e);
                }
                (self.rename, self.rename_err) = (None, String::new());
            }
        }
    }

    /// The rename being typed, as the action that applies it; when it can't be, the reason is kept for `note`.
    pub(crate) fn commit_rename(&mut self, cfg: &Config, i: usize) -> Option<Act> {
        let (_, text) = self.rename.as_ref()?;
        match check_rename(&cfg.providers, i, text) {
            Ok(new) => Some(Act::Rename(i, new)),
            Err(e) => {
                self.rename_err = e;
                None
            }
        }
    }
}
