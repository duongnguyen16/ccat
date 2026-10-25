//! Settings → MuseAI: its on/off switch and the signed-in account (a cookie login in Chrome).

use super::{active_account, remove_account, Account, Login};
use crate::settings::providers::{badge, count, head, name, note, row, trash, LoginJob, ProvidersEditor, INDENT};
use crate::{icon_btn, model_count, pal, switch, Config, Icon};
use eframe::egui::{self, Layout, RichText};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};

fn start_login(ctx: &egui::Context) -> Result<LoginJob<Account>, String> {
    let login = Login::start()?;
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
    let Some(mu_idx) = cfg.providers.iter().position(|p| p.muse) else { return };
    let pr = &mut cfg.providers[mu_idx];
    let title = pr.name.clone();
    if pr.muse_account.is_none() {
        pr.muse_account = active_account();
    }
    let has_account = pr.muse_account.is_some();
    head(ui, |ui| {
        let mut on = !pr.disabled;
        if switch(ui, &mut on, "").on_hover_text("Off: MuseAI is not offered in the pickers and not used for routing.").changed() {
            pr.disabled = !on;
        }
        name(ui, &title, on);
    });
    let n = model_count(pr);
    row(ui, |ui, _| {
        ed.fetch_btn(ui, pr);
        let free = ed.muse_login.is_none();
        let tip = if has_account {
            "Sign in with MuseAI in Chrome (replaces existing session)"
        } else {
            "Add a MuseAI account (opens Chrome)"
        };
        if ui.add_enabled_ui(free, |ui| icon_btn(ui, Icon::New).on_hover_text(tip).clicked()).inner {
            match start_login(ui.ctx()) {
                Ok(job) => (ed.muse_login, ed.muse_err) = (Some(job), String::new()),
                Err(e) => ed.muse_err = e,
            }
        }
        count(ui, n);
    });
    if let Some(job) = &ed.muse_login {
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
    if !ed.muse_err.is_empty() {
        note(ui, &ed.muse_err, p.danger);
    }
    if let Some(e) = ed.fetch_errs.get(&pr.name) {
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
            ui.with_layout(Layout::left_to_right(egui::Align::Center), |ui| {
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
    } else if ed.muse_login.is_none() {
        note(ui, "No MuseAI account yet. Use + to sign in.", p.subtle);
    }

    if clear_account {
        let _ = remove_account();
        pr.muse_account = None;
    }
    // The plan's allowance, as muse-cli cached it.
    let u = &feed.usage;
    if pr.muse_account.is_some() || !u.muse.bars.is_empty() || !u.muse.error.is_empty() {
        feed.limits(ui, 4, &u.muse_plan, &crate::usage::muse_bars(u), u.muse.fetched, &u.muse.error);
    }
}
