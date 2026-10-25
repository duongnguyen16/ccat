//! Settings → Antigravity: its on/off switch, the Google accounts that hang under it, and the browser sign-in.

use super::{plan_label, plan_of, Account, Login};
use crate::settings::providers::{badge, count, head, name, note, reload, row, trash, LoginJob, ProvidersEditor, INDENT};
use crate::{icon_btn, model_count, open_browser, pal, switch, Config, Icon};
use eframe::egui::{self, Layout, RichText};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};

fn start_login(ctx: &egui::Context) -> Result<LoginJob<Account>, String> {
    let login = Login::start()?;
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

pub(crate) fn page(ed: &mut ProvidersEditor, ui: &mut egui::Ui, cfg: &mut Config, feed: &crate::usage::Feed) {
    let p = pal(ui);
    let Some(a) = cfg.providers.iter().position(|p| p.antigravity) else { return };
    let pr = &mut cfg.providers[a];
    let title = pr.name.clone();
    head(ui, |ui| {
        let mut on = !pr.disabled;
        if switch(ui, &mut on, "").on_hover_text("Off: Antigravity is not offered in the pickers and not used for routing.").changed() {
            pr.disabled = !on;
        }
        name(ui, &title, on);
    });
    let n = model_count(pr);
    row(ui, |ui, _| {
        ed.fetch_btn(ui, pr);
        let free = ed.login.is_none();
        if ui.add_enabled_ui(free, |ui| icon_btn(ui, Icon::New).on_hover_text("Add a Google account").clicked()).inner {
            match start_login(ui.ctx()) {
                Ok(job) => (ed.login, ed.login_err) = (Some(job), String::new()),
                Err(e) => ed.login_err = e,
            }
        }
        count(ui, n);
    });
    if let Some(job) = &ed.login {
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
    if !ed.login_err.is_empty() {
        note(ui, &ed.login_err, p.danger);
    }
    if let Some(e) = ed.fetch_errs.get(&pr.name) {
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
            if reload(ui, hot, ed.checking.contains_key(&acc.email), "Look this account up again: its plan and its quota") {
                let (tx, rx) = mpsc::channel();
                let (a, ctx) = (acc.clone(), ui.ctx().clone());
                std::thread::spawn(move || {
                    let _ = tx.send(plan_of(&a));
                    crate::usage::refresh(false, true, false, false, false);
                    ctx.request_repaint();
                });
                ed.checking.insert(acc.email.clone(), rx);
            }
            ui.with_layout(Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add_space(INDENT);
                let alias = acc.email.split('@').next().unwrap_or(&acc.email);
                ui.label(RichText::new(alias).size(12.5).color(if on { p.fg } else { p.muted })).on_hover_text(&acc.email);
                badge(ui, plan_label(&acc.plan), on);
            });
        });
        // This account's own quota, every model group.
        let source = feed.usage.antigravity.get(&acc.email);
        let bars = source.map_or_else(Vec::new, |s| crate::usage::ag_bars(&[(&*acc, s)], ""));
        crate::usage::account_limits(ui, &bars, source.map_or("", |s| &s.error), acc.disabled);
    }
    if let Some(n) = remove {
        pr.accounts.remove(n);
    }
    if pr.accounts.is_empty() && ed.login.is_none() {
        note(ui, "No Google accounts yet. Use + to add one.", p.subtle);
    }
    ui.add_space(6.0);
    note(ui, "A Google account added while a session runs joins it, after the accounts it already uses.", p.subtle);
}
