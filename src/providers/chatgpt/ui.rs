//! Settings → ChatGPT: its on/off switch and the plan's accounts, which sign in through Codex CLI.

use super::{sign_out, sync_codex_cli, USAGE_PAGE};
use crate::settings::providers::{badge, count, head, name, note, reload, row, trash, ProvidersEditor, INDENT};
use crate::{icon_btn, model_count, open_browser, pal, switch, Config, Icon};
use eframe::egui::{self, Layout, RichText, Sense};
use std::{
    sync::{atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

pub(crate) fn page(ed: &mut ProvidersEditor, ui: &mut egui::Ui, cfg: &mut Config, feed: &crate::usage::Feed) {
    let p = pal(ui);
    let Some(g) = cfg.providers.iter().position(|p| p.chatgpt) else { return };
    let pr = &mut cfg.providers[g];
    let title = pr.name.clone();
    head(ui, |ui| {
        let mut on = !pr.disabled;
        if switch(ui, &mut on, "").on_hover_text("Off: ChatGPT is not offered in the pickers and not used for routing.").changed() {
            pr.disabled = !on;
        }
        name(ui, &title, on);
    });
    let n = model_count(pr);
    row(ui, |ui, _| {
        ed.fetch_btn(ui, pr);
        let free = ed.gpt_login.is_none();
        // One way in, and it is Codex's: a sign-in of ccat's own with Codex's client would make OpenAI revoke the
        // earlier ones (Codex CLI and the other devices). `codex login` keeps a single session that ccat shares.
        let tip = "Sign in with `codex login` (opens a terminal). ccat shares Codex CLI's login instead of creating its own, which would sign your other devices out.";
        if ui.add_enabled_ui(free, |ui| icon_btn(ui, Icon::New).on_hover_text(tip)).inner.clicked() {
            if let Err(e) = std::process::Command::new("cmd").args(["/c", "start", "Codex login", "cmd", "/k", "codex login"]).spawn() {
                ed.gpt_err = format!("Couldn't start `codex login`: {e}");
            }
        }
        // Pick up a `codex login` that just finished (or a sign-out), without reopening ccat.
        if ed.codex_at.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            ed.codex_at = Some(Instant::now());
            sync_codex_cli(&mut pr.chatgpt_accounts);
        }
        count(ui, n);
    });
    if let Some(job) = &ed.gpt_login {
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
    if !ed.gpt_err.is_empty() {
        note(ui, &ed.gpt_err, p.danger);
    }
    if let Some(e) = ed.fetch_errs.get(&pr.name) {
        note(ui, e, p.danger);
    }
    // One allowance, not one per account: usage is asked with the first Codex login that answers (see `fetch_chatgpt`).
    let u = &feed.usage;
    if u.chatgpt_on {
        feed.limits(ui, 2, &u.chatgpt_plan, &crate::usage::tidy(&u.chatgpt.bars, false), u.chatgpt.fetched, &u.chatgpt.error);
        note(ui, "One allowance for the Codex login ccat asks with, not one per account.", p.subtle);
        ui.add_space(8.0);
    }
    let mut remove = None;
    let mut resync = false;
    for n in 0..pr.chatgpt_accounts.len() {
        let needs = ed.gpt_needs(&pr.chatgpt_accounts[n].key);
        let acc = &mut pr.chatgpt_accounts[n];
        row(ui, |ui, hot| {
            let mut on = !acc.disabled;
            if switch(ui, &mut on, "").on_hover_text("Off: the proxy doesn't use this account.").changed() {
                acc.disabled = !on;
            }
            if !acc.cli() && trash(ui, hot, "Remove this account from ccat") {
                remove = Some(n);
            }
            let busy = ed.checking.contains_key(&acc.key);
            if reload(ui, hot, busy, "Look this account up again: its plan and its usage") {
                let (tx, rx) = mpsc::channel();
                let ctx = ui.ctx().clone();
                std::thread::spawn(move || {
                    crate::usage::refresh(false, false, true, false, false);
                    let _ = tx.send(Ok(String::new()));
                    ctx.request_repaint();
                });
                ed.checking.insert(acc.key.clone(), rx);
                resync = true;
            }
            ui.with_layout(Layout::left_to_right(egui::Align::Center), |ui| {
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
        sync_codex_cli(&mut pr.chatgpt_accounts);
    }
    if let Some(n) = remove {
        let gone = pr.chatgpt_accounts.remove(n);
        // Off the UI thread: an official-route sign-in is revoked first (a network call); a Codex-route one is only forgotten.
        std::thread::spawn(move || sign_out(&gone.key));
    }
    if pr.chatgpt_accounts.is_empty() && ed.gpt_login.is_none() {
        note(ui, "No ChatGPT account yet. + runs `codex login`; ccat then shares Codex's login.", p.subtle);
    } else if !pr.chatgpt_accounts.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(INDENT);
            let link = egui::Label::new(RichText::new("Manage usage").size(11.5).color(p.subtle).underline()).sense(Sense::click());
            if ui.add(link).on_hover_text(USAGE_PAGE).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                open_browser(USAGE_PAGE);
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
