//! The Providers view: every connection ccat can route to, each with an on/off switch. One component for the usage
//! popup's Settings and the launcher's Config window. Renames and deletes also rewrite the profile files (see
//! `rename_provider`, `delete_provider`); everything else just edits `Config`, and the host saves it.

use crate::{antigravity, chatgpt, check_rename, delete_provider, fetch_models, icon_btn, model_count, open_browser, pal, profiles, rename_provider, switch, Config, Icon, Provider};
use eframe::egui::{self, vec2, Align, Align2, Color32, FontId, Key, Layout, RichText, Sense, TextEdit};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

const ROW: f32 = 28.0;
/// How far the rows under a provider (accounts, URL and key, notes) are pushed in.
const INDENT: f32 = 16.0;

/// A model list: the ids, and (ChatGPT only) what is known of each.
type Fetched = Result<(Vec<String>, Vec<chatgpt::ChatgptModel>), String>;

/// A sign-in running in the background (the browser round-trip).
struct LoginJob<T> {
    url: String,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<Result<T, String>>,
}

#[derive(Default)]
pub struct ProvidersEditor {
    /// The endpoint being renamed (index in `Config::providers`) and the text so far; `focus` grabs the keyboard once.
    rename: Option<(usize, String)>,
    focus: bool,
    rename_err: String,
    /// Model-list fetches in flight, by provider name, and the last error per provider.
    fetches: Vec<(String, mpsc::Receiver<Fetched>)>,
    fetch_errs: HashMap<String, String>,
    login: Option<LoginJob<antigravity::Account>>,
    login_err: String,
    gpt_login: Option<LoginJob<chatgpt::ChatgptAccount>>,
    gpt_err: String,
    muse_login: Option<LoginJob<crate::museai::Account>>,
    muse_err: String,
    ctx: Option<egui::Context>,
    /// Whether each ChatGPT account (by key) must sign in again, and when that was last looked up: the answer is in a
    /// file, so it is read every few seconds, not every frame.
    gpt_state: HashMap<String, (Instant, bool)>,
    /// Accounts being looked up again (Antigravity by email: its plan; ChatGPT by key), and the answer to come.
    checking: HashMap<String, mpsc::Receiver<Result<String, String>>>,
}

enum Act {
    Add,
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
        let key = p.chatgpt_accounts.iter().find(|a| a.usable()).map(|a| a.key.clone()).ok_or("Sign in with ChatGPT first (and turn the account on).")?;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(chatgpt::list_models(&key).map(|m| (m.iter().map(|x| x.slug.clone()).collect(), m)));
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

fn start_login(ctx: &egui::Context) -> Result<LoginJob<antigravity::Account>, String> {
    let login = antigravity::Login::start()?;
    open_browser(&login.url);
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let (url, stop, ctx) = (login.url.clone(), cancel.clone(), ctx.clone());
    std::thread::spawn(move || {
        let _ = tx.send(login.finish(&stop));
        ctx.request_repaint();
    });
    Ok(LoginJob { url, cancel, rx })
}

fn start_muse_login(ctx: &egui::Context) -> Result<LoginJob<crate::museai::Account>, String> {
    let login = crate::museai::Login::start()?;
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let (url, stop, ctx) = (login.url.clone(), cancel.clone(), ctx.clone());
    std::thread::spawn(move || {
        let _ = tx.send(login.finish(&stop));
        ctx.request_repaint();
    });
    Ok(LoginJob { url, cancel, rx })
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

/// One row of `ROW` height. `add` lays out the right-hand controls first (right to left), then a left-to-right
/// layout takes what's left; `hot` says the pointer is over the row, which is when a trash icon shows.
fn row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui, bool)) {
    let w = ui.available_width();
    let hot = ui.rect_contains_pointer(egui::Rect::from_min_size(ui.cursor().min, vec2(w, ROW)));
    ui.allocate_ui_with_layout(vec2(w, ROW), Layout::right_to_left(Align::Center), |ui| {
        ui.set_min_size(vec2(w, ROW));
        add(ui, hot);
    });
}

/// The row's trash slot: the icon while hovered, else the same space left empty so nothing shifts.
fn trash(ui: &mut egui::Ui, hot: bool, tip: &str) -> bool {
    if hot {
        icon_btn(ui, Icon::Delete).on_hover_text(tip).clicked()
    } else {
        ui.allocate_space(vec2(28.0, 28.0));
        false
    }
}

/// The row's reload slot, left of the trash: while hovered (or busy), else the same space left empty.
fn reload(ui: &mut egui::Ui, hot: bool, busy: bool, tip: &str) -> bool {
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
fn badge(ui: &mut egui::Ui, text: &str, on: bool) -> Option<egui::Response> {
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

/// A line of small text under a row, lined up with the names.
fn note(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.add_space(INDENT);
        ui.add(egui::Label::new(RichText::new(text).size(11.5).color(color)).wrap());
    });
}

/// `label` and a text field; a secret shows as dots unless it has the keyboard.
fn field(ui: &mut egui::Ui, label: &str, text: &mut String, id: egui::Id, secret: bool, hint: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        ui.add_space(INDENT);
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

    /// Whether the ChatGPT account must sign in again (looked up at most every 3 seconds).
    fn gpt_needs(&mut self, key: &str) -> bool {
        let fresh = self.gpt_state.get(key).filter(|(at, _)| at.elapsed() < Duration::from_secs(3));
        match fresh {
            Some((_, v)) => *v,
            None => {
                let v = chatgpt::needs_signin(key);
                self.gpt_state.insert(key.to_string(), (Instant::now(), v));
                v
            }
        }
    }

    /// Takes in finished model lists and sign-ins; call it every frame, whether or not the view is showing.
    pub fn poll(&mut self, cfg: &mut Config) {
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
                        p.chatgpt_models = meta;
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
                        // Signing in again refreshes that account (and keeps its switch) instead of duplicating it.
                        match p.chatgpt_accounts.iter_mut().find(|a| a.key == acc.key) {
                            Some(a) => *a = chatgpt::ChatgptAccount { disabled: a.disabled, ..acc },
                            None => p.chatgpt_accounts.push(acc),
                        }
                    }
                    self.gpt_state.clear();
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
                        if let Some(ctx) = &self.ctx {
                            if let Ok(rx) = start_fetch(ctx, p) {
                                self.fetches.push((p.name.clone(), rx));
                                self.fetch_errs.remove(&p.name);
                            }
                        }
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

    /// The view, scrolling past `max_h`.
    pub fn show(&mut self, ui: &mut egui::Ui, cfg: &mut Config, max_h: f32) {
        self.ctx = Some(ui.ctx().clone());
        egui::ScrollArea::vertical().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            self.body(ui, cfg);
        });
    }

    /// The fetch button of a provider's row, or a spinner while its list is coming.
    fn fetch_btn(&mut self, ui: &mut egui::Ui, p: &Provider) {
        if self.fetches.iter().any(|(n, _)| *n == p.name) {
            ui.add_sized([28.0, 28.0], egui::Spinner::new().size(14.0));
        } else if icon_btn(ui, Icon::Fetch).on_hover_text("Fetch model list").clicked() {
            match start_fetch(ui.ctx(), p) {
                Ok(rx) => {
                    self.fetches.push((p.name.clone(), rx));
                    self.fetch_errs.remove(&p.name);
                }
                Err(e) => {
                    self.fetch_errs.insert(p.name.clone(), e);
                }
            }
        }
    }

    fn body(&mut self, ui: &mut egui::Ui, cfg: &mut Config) {
        let p = pal(ui);
        let name_text = |t: &str, on: bool| RichText::new(t).size(13.0).color(if on { p.fg } else { p.muted });

        // Claude Code itself: its login can't be turned off, only offered or not as a choice.
        row(ui, |ui, _| {
            let mut on = !cfg.claude_off;
            if switch(ui, &mut on, "")
                .on_hover_text("Off: Claude models leave the model pickers (Default stays) and are skipped as fallbacks. A tier already on Claude keeps working.")
                .changed()
            {
                cfg.claude_off = !on;
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.add(egui::Label::new(name_text("Claude Code (Original)", on)).truncate());
            });
        });

        // Antigravity: its Google accounts hang under it.
        if let Some(a) = cfg.providers.iter().position(|p| p.antigravity) {
            let pr = &mut cfg.providers[a];
            let title = if model_count(pr) > 0 { format!("{} ({})", pr.name, model_count(pr)) } else { pr.name.clone() };
            row(ui, |ui, _| {
                let mut on = !pr.disabled;
                if switch(ui, &mut on, "").on_hover_text("Off: Antigravity is not offered in the pickers and not used for routing.").changed() {
                    pr.disabled = !on;
                }
                ui.allocate_space(vec2(28.0, 28.0));
                self.fetch_btn(ui, pr);
                let free = self.login.is_none();
                if ui.add_enabled_ui(free, |ui| icon_btn(ui, Icon::New).on_hover_text("Add a Google account").clicked()).inner {
                    match start_login(ui.ctx()) {
                        Ok(job) => (self.login, self.login_err) = (Some(job), String::new()),
                        Err(e) => self.login_err = e,
                    }
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(name_text(&title, on)).truncate());
                });
            });
            if let Some(job) = &self.login {
                ui.horizontal(|ui| {
                    ui.add_space(INDENT);
                    ui.spinner();
                    ui.label(RichText::new("Finish signing in in your browser…").size(11.5).color(p.subtle));
                    if ui.button("Copy link").clicked() {
                        ui.ctx().copy_text(job.url.clone());
                    }
                    if ui.button("Cancel").clicked() {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
            if !self.login_err.is_empty() {
                note(ui, &self.login_err, p.danger);
            }
            if let Some(e) = self.fetch_errs.get(&pr.name) {
                note(ui, e, p.danger);
            }
            let mut remove = None;
            for (n, acc) in pr.accounts.iter_mut().enumerate() {
                row(ui, |ui, hot| {
                    let mut on = !acc.disabled;
                    if switch(ui, &mut on, "").on_hover_text("Off: the proxy doesn't use this account.").changed() {
                        acc.disabled = !on;
                    }
                    if trash(ui, hot, "Remove this account") {
                        remove = Some(n);
                    }
                    if reload(ui, hot, self.checking.contains_key(&acc.email), "Look this account up again: its plan and its quota") {
                        let (tx, rx) = mpsc::channel();
                        let (a, ctx) = (acc.clone(), ui.ctx().clone());
                        std::thread::spawn(move || {
                            let _ = tx.send(antigravity::plan_of(&a));
                            crate::usage::refresh(false, true, false, false, false);
                            ctx.request_repaint();
                        });
                        self.checking.insert(acc.email.clone(), rx);
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add_space(INDENT);
                        let alias = acc.email.split('@').next().unwrap_or(&acc.email);
                        ui.label(RichText::new(alias).size(12.5).color(if on { p.fg } else { p.muted })).on_hover_text(&acc.email);
                        badge(ui, antigravity::plan_label(&acc.plan), on);
                    });
                });
            }
            if let Some(n) = remove {
                pr.accounts.remove(n);
            }
            if pr.accounts.is_empty() && self.login.is_none() {
                note(ui, "No Google accounts yet. Use + to add one.", p.subtle);
            }
        }

        // ChatGPT: the plan's accounts hang under it.
        if let Some(g) = cfg.providers.iter().position(|p| p.chatgpt) {
            let pr = &mut cfg.providers[g];
            let title = if model_count(pr) > 0 { format!("{} ({})", pr.name, model_count(pr)) } else { pr.name.clone() };
            row(ui, |ui, _| {
                let mut on = !pr.disabled;
                if switch(ui, &mut on, "").on_hover_text("Off: ChatGPT is not offered in the pickers and not used for routing.").changed() {
                    pr.disabled = !on;
                }
                ui.allocate_space(vec2(28.0, 28.0));
                self.fetch_btn(ui, pr);
                let free = self.gpt_login.is_none();
                // One way in, and it is Codex's: a sign-in of ccat's own with Codex's client would make OpenAI revoke the
                // earlier ones (Codex CLI and the other devices). `codex login` keeps a single session that ccat shares.
                let tip = "Sign in with `codex login` (opens a terminal). ccat shares Codex CLI's login instead of creating its own, which would sign your other devices out.";
                if ui.add_enabled_ui(free, |ui| icon_btn(ui, Icon::New).on_hover_text(tip)).inner.clicked() {
                    if let Err(e) = std::process::Command::new("cmd").args(["/c", "start", "Codex login", "cmd", "/k", "codex login"]).spawn() {
                        self.gpt_err = format!("Couldn't start `codex login`: {e}");
                    }
                }
                // Pick up a `codex login` that just finished (or a sign-out), without reopening ccat.
                if ui.ctx().cumulative_frame_nr() % 60 == 0 {
                    chatgpt::sync_codex_cli(&mut pr.chatgpt_accounts);
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(name_text(&title, on)).truncate());
                });
            });
            if let Some(job) = &self.gpt_login {
                ui.horizontal(|ui| {
                    ui.add_space(INDENT);
                    ui.spinner();
                    ui.label(RichText::new("Finish signing in in your browser…").size(11.5).color(p.subtle));
                    if ui.button("Copy link").clicked() {
                        ui.ctx().copy_text(job.url.clone());
                    }
                    if ui.button("Cancel").clicked() {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
            if !self.gpt_err.is_empty() {
                note(ui, &self.gpt_err, p.danger);
            }
            if let Some(e) = self.fetch_errs.get(&pr.name) {
                note(ui, e, p.danger);
            }
            let mut remove = None;
            let mut resync = false;
            for n in 0..pr.chatgpt_accounts.len() {
                let needs = self.gpt_needs(&pr.chatgpt_accounts[n].key);
                let acc = &mut pr.chatgpt_accounts[n];
                row(ui, |ui, hot| {
                    let mut on = !acc.disabled;
                    if switch(ui, &mut on, "").on_hover_text("Off: the proxy doesn't use this account.").changed() {
                        acc.disabled = !on;
                    }
                    if !acc.cli() && trash(ui, hot, "Remove this account from ccat") {
                        remove = Some(n);
                    }
                    let busy = self.checking.contains_key(&acc.key);
                    if reload(ui, hot, busy, "Look this account up again: its plan and its usage") {
                        let (tx, rx) = mpsc::channel();
                        let ctx = ui.ctx().clone();
                        std::thread::spawn(move || {
                            crate::usage::refresh(false, false, true, false, false);
                            let _ = tx.send(Ok(String::new()));
                            ctx.request_repaint();
                        });
                        self.checking.insert(acc.key.clone(), rx);
                        resync = true;
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add_space(INDENT);
                        let alias = acc.email.split('@').next().unwrap_or(&acc.email);
                        ui.label(RichText::new(alias).size(12.5).color(if on { p.fg } else { p.muted })).on_hover_text(&acc.email);
                        if acc.cli() {
                            ui.label(RichText::new("Codex CLI").size(10.5).color(p.subtle)).on_hover_text("Codex CLI's login, shared: ccat never refreshes or signs it out. Manage it with `codex login` / `codex logout`.");
                        }
                        if acc.codex() {
                            let plan = acc.plan.chars().next().map(|c| c.to_uppercase().chain(acc.plan.chars().skip(1)).collect::<String>()).unwrap_or_default();
                            badge(ui, &plan, on);
                        }
                        let why = if !acc.granted { "plan not granted" } else if needs && acc.cli() { "run codex login" } else if needs { "signed out" } else { "" };
                        let _ = badge(ui, why, on).map(|b| b.on_hover_text(if acc.cli() { "Codex CLI isn't signed in: + runs `codex login`" } else { "Revoked at OpenAI; remove it and use Codex CLI's login" }));
                    });
                });
            }
            if resync {
                chatgpt::sync_codex_cli(&mut pr.chatgpt_accounts);
            }
            if let Some(n) = remove {
                let gone = pr.chatgpt_accounts.remove(n);
                // Off the UI thread: an official-route sign-in is revoked first (a network call); a Codex-route one is only forgotten.
                std::thread::spawn(move || chatgpt::sign_out(&gone.key));
            }
            if pr.chatgpt_accounts.is_empty() && self.gpt_login.is_none() {
                note(ui, "No ChatGPT account yet. + runs `codex login`; ccat then shares Codex's login.", p.subtle);
            } else if !pr.chatgpt_accounts.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(INDENT);
                    let link = egui::Label::new(RichText::new("Manage usage").size(11.5).color(p.subtle).underline()).sense(Sense::click());
                    if ui.add(link).on_hover_text(chatgpt::USAGE_PAGE).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        open_browser(chatgpt::USAGE_PAGE);
                    }
                });
            }
            if cfg.chatgpt_note == 1 {
                note(ui, "You're using your ChatGPT plan — eligible requests count toward your ChatGPT usage.", p.fg);
                ui.horizontal(|ui| {
                    ui.add_space(INDENT);
                    if ui.button("Got it").clicked() {
                        cfg.chatgpt_note = 2;
                    }
                });
            }
        }

        // Ollama: local model runtime.
        if let Some(ol) = cfg.providers.iter().position(|p| p.ollama) {
            let pr = &mut cfg.providers[ol];
            let title = if model_count(pr) > 0 { format!("{} ({})", pr.name, model_count(pr)) } else { pr.name.clone() };
            row(ui, |ui, _| {
                let mut on = !pr.disabled;
                if switch(ui, &mut on, "").on_hover_text("Off: Ollama is not offered in the pickers and not used for routing.").changed() {
                    pr.disabled = !on;
                }
                ui.allocate_space(vec2(28.0, 28.0));
                self.fetch_btn(ui, pr);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(name_text(&title, on)).truncate());
                });
            });
            if let Some(e) = self.fetch_errs.get(&pr.name) {
                note(ui, e, p.danger);
            }
            ui.horizontal(|ui| {
                ui.add_space(INDENT);
                ui.label(RichText::new("URL:").size(11.5).color(p.muted));
                let mut u = pr.url.clone();
                let te = ui.add(TextEdit::singleline(&mut u).desired_width(180.0));
                if te.changed() {
                    pr.url = u;
                }
                if pr.url.is_empty() {
                    pr.url = crate::ollama::DEFAULT_URL.into();
                }
            });
            ui.horizontal(|ui| {
                ui.add_space(INDENT);
                ui.label(RichText::new("Key:").size(11.5).color(p.muted));
                let mut k = pr.key.clone();
                let te = ui.add(TextEdit::singleline(&mut k).password(true).desired_width(180.0));
                if te.changed() {
                    pr.key = k;
                }
                let link = egui::Label::new(RichText::new("Get API key").size(11.0).color(p.subtle).underline()).sense(Sense::click());
                if ui.add(link).on_hover_text("https://ollama.com/settings/keys").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    open_browser("https://ollama.com/settings/keys");
                }
            });
            if pr.models.is_empty() && pr.custom.is_empty() {
                note(ui, "Click Fetch (↻) to retrieve models from Ollama Cloud or local instance.", p.subtle);
            }
        }

        // MuseAI: Meta Hatch Gateway VM via local sidecar bridge.
        if let Some(mu_idx) = cfg.providers.iter().position(|p| p.muse) {
            let pr = &mut cfg.providers[mu_idx];
            let title = if model_count(pr) > 0 { format!("{} ({})", pr.name, model_count(pr)) } else { pr.name.clone() };
            if pr.muse_account.is_none() {
                pr.muse_account = crate::museai::active_account();
            }
            let has_account = pr.muse_account.is_some();
            row(ui, |ui, _| {
                let mut on = !pr.disabled;
                if switch(ui, &mut on, "").on_hover_text("Off: MuseAI is not offered in the pickers and not used for routing.").changed() {
                    pr.disabled = !on;
                }
                ui.allocate_space(vec2(28.0, 28.0));
                self.fetch_btn(ui, pr);
                let free = self.muse_login.is_none();
                let tip = if has_account {
                    "Sign in with MuseAI in Chrome (replaces existing session)"
                } else {
                    "Add a MuseAI account (opens Chrome)"
                };
                if ui.add_enabled_ui(free, |ui| icon_btn(ui, Icon::New).on_hover_text(tip).clicked()).inner {
                    match start_muse_login(ui.ctx()) {
                        Ok(job) => (self.muse_login, self.muse_err) = (Some(job), String::new()),
                        Err(e) => self.muse_err = e,
                    }
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(name_text(&title, on)).truncate());
                });
            });
            if let Some(job) = &self.muse_login {
                ui.horizontal(|ui| {
                    ui.add_space(INDENT);
                    ui.spinner();
                    ui.label(RichText::new("Finish signing in in Chrome…").size(11.5).color(p.subtle));
                    if ui.button("Copy link").clicked() {
                        ui.ctx().copy_text(job.url.clone());
                    }
                    if ui.button("Cancel").clicked() {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
            if !self.muse_err.is_empty() {
                note(ui, &self.muse_err, p.danger);
            }
            if let Some(e) = self.fetch_errs.get(&pr.name) {
                note(ui, e, p.danger);
            }

            let mut clear_account = false;
            if let Some(acc) = &mut pr.muse_account {
                row(ui, |ui, hot| {
                    let mut on = !acc.disabled;
                    if switch(ui, &mut on, "").on_hover_text("Off: the proxy doesn't use this account.").changed() {
                        acc.disabled = !on;
                    }
                    if trash(ui, hot, "Remove this account (clears login)") {
                        clear_account = true;
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add_space(INDENT);
                        let alias = if acc.viewer_id.is_empty() || acc.viewer_id == "Active session" {
                            "Active session".to_string()
                        } else {
                            format!("Account {}", acc.viewer_id)
                        };
                        ui.label(RichText::new(&alias).size(12.5).color(if on { p.fg } else { p.muted }));
                        let tier_badge = if acc.tier.is_empty() { "Free plan" } else { &acc.tier };
                        badge(ui, tier_badge, on);
                    });
                });
            } else if self.muse_login.is_none() {
                note(ui, "No MuseAI account yet. Use + to sign in.", p.subtle);
            }

            if clear_account {
                let _ = crate::museai::remove_account();
                pr.muse_account = None;
            }
        }

        // The endpoints.
        let mut act = None;
        for i in 0..cfg.providers.len() {
            if cfg.providers[i].antigravity || cfg.providers[i].chatgpt || cfg.providers[i].ollama || cfg.providers[i].muse {
                continue;
            }
            ui.add_space(4.0);
            if self.rename.as_ref().is_some_and(|(r, _)| *r == i) && ui.input(|inp| inp.key_pressed(Key::Escape)) {
                (self.rename, self.rename_err) = (None, String::new());
            }
            let renaming = self.rename.as_ref().is_some_and(|(r, _)| *r == i);
            let n = model_count(&cfg.providers[i]);
            let pr = &mut cfg.providers[i];
            let title = if n > 0 { format!("{} ({n})", pr.name) } else { pr.name.clone() };
            let mut commit = false;
            row(ui, |ui, hot| {
                let mut on = !pr.disabled;
                if switch(ui, &mut on, "").on_hover_text("Off: not offered in the pickers and not used for routing.").changed() {
                    pr.disabled = !on;
                }
                if trash(ui, hot && !renaming, "Delete this provider (its tiers go back to Claude)") {
                    act = Some(Act::Delete(i));
                }
                self.fetch_btn(ui, pr);
                if renaming {
                    commit |= icon_btn(ui, Icon::Save).on_hover_text("Save the name").clicked();
                } else if icon_btn(ui, Icon::Rename).on_hover_text("Rename").clicked() {
                    (self.rename, self.focus, self.rename_err) = (Some((i, pr.name.clone())), true, String::new());
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    match &mut self.rename {
                        Some((r, text)) if *r == i => {
                            let te = ui.add(TextEdit::singleline(text).desired_width(ui.available_width() - 8.0));
                            if self.focus {
                                te.request_focus();
                                self.focus = false;
                            }
                            commit |= te.lost_focus() && ui.input(|inp| inp.key_pressed(Key::Enter));
                        }
                        _ => {
                            ui.add(egui::Label::new(name_text(&title, on)).truncate());
                        }
                    }
                });
            });
            if commit {
                if let Some((_, text)) = &self.rename {
                    match check_rename(&cfg.providers, i, text) {
                        Ok(new) => act = Some(Act::Rename(i, new)),
                        Err(e) => self.rename_err = e,
                    }
                }
            }
            if renaming && !self.rename_err.is_empty() {
                note(ui, &self.rename_err, p.danger);
            }
            if let Some(e) = self.fetch_errs.get(&cfg.providers[i].name) {
                note(ui, e, p.danger);
            }
            let pr = &mut cfg.providers[i];
            field(ui, "URL", &mut pr.url, egui::Id::new(("ccat-url", i)), false, "https://api.example.com");
            field(ui, "API key", &mut pr.key, egui::Id::new(("ccat-key", i)), true, "");
        }

        ui.add_space(4.0);
        ui.separator();
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let add = egui::Label::new(RichText::new("+ Add new providers").size(12.0).color(p.subtle)).sense(Sense::click());
            if ui.add(add).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                act = Some(Act::Add);
            }
        });
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new("Switches reach running sessions within seconds. A Google account added while a session runs joins it on the next launch.").size(11.0).color(p.subtle)).wrap());

        match act {
            Some(Act::Add) => {
                let names: Vec<String> = cfg.providers.iter().map(|p| p.name.clone()).collect();
                let name = profiles::unique(&names, "Provider");
                cfg.providers.push(Provider { name: name.clone(), ..Default::default() });
                (self.rename, self.focus, self.rename_err) = (Some((cfg.providers.len() - 1, name)), true, String::new());
            }
            Some(Act::Delete(i)) => {
                self.fetch_errs.remove(&cfg.providers[i].name);
                delete_provider(cfg, &profiles::dir(), i);
                (self.rename, self.rename_err) = (None, String::new());
            }
            Some(Act::Rename(i, new)) => {
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
            None => {}
        }
    }
}
