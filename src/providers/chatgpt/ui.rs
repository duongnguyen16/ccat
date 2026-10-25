//! Settings → ChatGPT: its on/off switch, the Official / Codex (Legacy) mode, the accounts, and the browser sign-ins.

use super::{sign_out, ChatgptAccount, Login, USAGE_PAGE};
use crate::settings::providers::{badge, count, head, name, note, reload, row, trash, LoginJob, ProvidersEditor};
use crate::settings::card;
use crate::usage::segmented;
use crate::{icon_btn, model_count, open_browser, pal, switch, Config, Icon};
use eframe::egui::{self, Layout, RichText, Sense};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};

/// The Codex (Legacy) sign-in uses Codex's own OAuth client, and OpenAI may then end the Codex CLI's session elsewhere.
const CODEX_WARN: &str = "Signs in with Codex's own client, as Codex CLI does. This may sign Codex CLI out on your other devices.";

/// `again`: an account to sign in again (its client id and hints), else a new one. `codex`: Codex's client instead.
fn start_login(ctx: &egui::Context, again: Option<&ChatgptAccount>, codex: bool) -> Result<LoginJob<ChatgptAccount>, String> {
    let login = Login::start(again, codex)?;
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

/// A small underlined link under the rows.
fn link(ui: &mut egui::Ui, text: &str, tip: &str, url: &str) {
    let p = pal(ui);
    let label = egui::Label::new(RichText::new(text).size(11.5).color(p.subtle).underline()).sense(Sense::click());
    if ui.add(label).on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        open_browser(url);
    }
}

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
    feed.analytics(ui, &title);
    let n = model_count(pr);
    let mut start: Option<(Option<ChatgptAccount>, bool)> = None;
    card(ui, |ui| {
        row(ui, |ui, _| {
            ed.fetch_btn(ui, pr);
            let free = ed.gpt_login.is_none();
            if ui.add_enabled_ui(free, |ui| icon_btn(ui, Icon::New).on_hover_text("Sign in with ChatGPT (opens your browser)")).inner.clicked() {
                start = Some((None, false));
            }
            count(ui, n);
        });
        if let Some(job) = &ed.gpt_login {
            ui.horizontal(|ui| {
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
    });
    // Official or Codex (Legacy): followed live by the running sessions (see `Pool::set_legacy`).
    let mut legacy = pr.gpt_legacy();
    card(ui, |ui| {
        if segmented(ui, &mut legacy, &[(false, "Official"), (true, "Codex (Legacy)")]) {
            pr.chatgpt_legacy = Some(legacy);
        }
        ui.add_space(4.0);
        if legacy {
            note(ui, "Codex (Legacy): the ChatGPT backend Codex CLI uses. Not documented by OpenAI, so it may change; shows your usage numbers.", p.subtle);
            ui.add_space(4.0);
            let free = ed.gpt_login.is_none();
            if ui.add_enabled(free, egui::Button::new("Sign in for Codex (Legacy)")).on_hover_text(CODEX_WARN).clicked() {
                start = Some((None, true));
            }
        } else {
            note(ui, "Official: api.openai.com, documented by OpenAI. Counts toward the cap you set for ccat in ChatGPT settings; no usage numbers here.", p.subtle);
        }
    });
    // One allowance, not one per account: usage is asked with the first stored token that answers (see `fetch_chatgpt`).
    let u = &feed.usage;
    if u.chatgpt_on {
        card(ui, |ui| {
            if !legacy && u.chatgpt.fetched > 0 && u.chatgpt.bars.is_empty() && u.chatgpt.error.is_empty() {
                // Not a failure: the official sign-in's token doesn't read the numbers.
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Usage numbers aren't available for the official sign-in; see").size(11.5).color(p.subtle));
                    link(ui, "chatgpt.com Settings → Usage", USAGE_PAGE, USAGE_PAGE);
                });
            } else {
                feed.limits(ui, 2, &u.chatgpt_plan, &crate::usage::tidy(&u.chatgpt.bars, false), u.chatgpt.fetched, &u.chatgpt.error);
                ui.add_space(4.0);
                note(ui, "One allowance for the login ccat asks with, not one per account.", p.subtle);
            }
        });
    }
    let mut remove = None;
    for n in 0..pr.chatgpt_accounts.len() {
        let (needs, refused) = ed.gpt_needs(&pr.chatgpt_accounts[n].key);
        let acc = &mut pr.chatgpt_accounts[n];
        let usable_here = acc.runs(legacy);
        card(ui, |ui| {
            row(ui, |ui, hot| {
                let mut on = !acc.disabled;
                if switch(ui, &mut on, "").on_hover_text("Off: the proxy doesn't use this account.").changed() {
                    acc.disabled = !on;
                }
                if trash(ui, hot, "Remove this account from ccat (signs it out at OpenAI)") {
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
                }
                ui.with_layout(Layout::left_to_right(egui::Align::Center), |ui| {
                    let alias = acc.email.split('@').next().unwrap_or(&acc.email);
                    ui.label(RichText::new(alias).size(12.5).color(if on && usable_here { p.fg } else { p.muted })).on_hover_text(&acc.email);
                    if acc.cli() {
                        ui.label(RichText::new("Codex CLI (legacy import)").size(10.5).color(p.subtle))
                            .on_hover_text("An older ccat copied this login from Codex CLI. ccat no longer reads Codex's files, so it has no token: sign in with ChatGPT to replace it.");
                    } else if acc.own_codex() {
                        ui.label(RichText::new("Codex").size(10.5).color(p.subtle)).on_hover_text(if legacy { "Signed in with Codex's client: used in Codex (Legacy) mode." } else { "Signed in with Codex's client: only used in Codex (Legacy) mode." });
                    }
                    let plan = acc.plan.chars().next().map(|c| c.to_uppercase().chain(acc.plan.chars().skip(1)).collect::<String>()).unwrap_or_default();
                    badge(ui, &plan, on);
                    let (why, tip) = if !acc.granted {
                        ("plan not granted", "Plan usage wasn't granted at sign-in. Click to sign in again.")
                    } else if acc.cli() {
                        ("sign in again", "Click to sign in with ChatGPT (this row is replaced).")
                    } else if needs {
                        ("sign in again", "The sign-in ended (revoked, expired or reused). Click to sign in again.")
                    } else if refused && legacy && acc.route.is_empty() {
                        ("Codex (Legacy) needs its own sign-in", "The Codex backend refused this ChatGPT sign-in (Official mode still works). Click to sign in with Codex's client. This may sign Codex CLI out on your other devices.")
                    } else {
                        ("", "")
                    };
                    if badge(ui, why, on).is_some_and(|b| b.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()) && ed.gpt_login.is_none() {
                        start = match (acc.cli(), refused && legacy && acc.route.is_empty() && !needs) {
                            (true, _) => Some((None, false)),
                            (_, true) => Some((None, true)),
                            _ => Some((Some(acc.clone()), acc.own_codex())),
                        };
                    }
                });
            });
        });
    }
    if let Some((again, codex)) = start.filter(|_| ed.gpt_login.is_none()) {
        match start_login(ui.ctx(), again.as_ref(), codex) {
            Ok(job) => (ed.gpt_login, ed.gpt_err) = (Some(job), String::new()),
            Err(e) => ed.gpt_err = e,
        }
    }
    if let Some(n) = remove {
        let gone = pr.chatgpt_accounts.remove(n);
        // Off the UI thread: a Sign in with ChatGPT token is revoked first (a network call); a Codex-client one is only forgotten.
        std::thread::spawn(move || sign_out(&gone.key));
    }
    if pr.chatgpt_accounts.is_empty() && ed.gpt_login.is_none() {
        note(ui, "No ChatGPT account yet. + signs in with ChatGPT in your browser, once, for both modes.", p.subtle);
    } else if !pr.chatgpt_accounts.is_empty() {
        link(ui, "Manage usage", USAGE_PAGE, USAGE_PAGE);
    }
    if cfg.chatgpt_note == 1 {
        ui.add_space(10.0);
        card(ui, |ui| {
            note(ui, "You're using your ChatGPT plan — eligible requests count toward your ChatGPT usage.", p.fg);
            ui.add_space(4.0);
            if ui.button("Got it").clicked() {
                cfg.chatgpt_note = 2;
            }
        });
    }
}
