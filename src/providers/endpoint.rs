//! A custom Anthropic-compatible endpoint: forwarded with its own key (see `proxy::http::forward`); here its model list.

use crate::bare_id;
use crate::providers::openai::{is_free, PRESETS};
use serde_json::Value;
use std::time::Duration;

fn models_endpoint(url: &str) -> String {
    let base = url.trim().trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    format!("{base}/v1/models?limit=1000")
}

/// Works for Anthropic-style and OpenAI-style `/v1/models`: both return `{ data: [{ id }] }`.
pub(crate) fn fetch_models(url: &str, key: &str) -> Result<Vec<String>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
    let mut res = agent
        .get(&models_endpoint(url))
        .header("x-api-key", key)
        .header("authorization", format!("Bearer {key}"))
        .header("anthropic-version", "2023-06-01")
        .call()
        .map_err(|e| e.to_string())?;
    let body = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("response is not JSON: {e}"))?;
    let data = v["data"].as_array().ok_or("response has no `data` array")?;
    Ok(data.iter().filter_map(|m| m["id"].as_str().map(bare_id)).collect())
}

// ---------------------------------------------------------------- settings page

/// Settings → a custom endpoint (`cfg.providers[i]`): its name (rename, delete), the on/off switch, URL and key, and
/// the model list. Rename and delete come back as an `Act` for the editor to carry out.
pub(crate) fn page(ed: &mut crate::settings::providers::ProvidersEditor, ui: &mut eframe::egui::Ui, cfg: &mut crate::Config, feed: &crate::usage::Feed, i: usize) -> Option<crate::settings::providers::Act> {
    use crate::settings::{group, providers::{count, field, head, name, note, row, Act}};
    use crate::{icon_btn, switch, Icon};
    use eframe::egui::{Key, Layout, TextEdit};
    let p = crate::pal(ui);
    if ed.rename.as_ref().is_some_and(|(r, _)| *r == i) && ui.input(|inp| inp.key_pressed(Key::Escape)) {
        (ed.rename, ed.rename_err) = (None, String::new());
    }
    let renaming = ed.rename.as_ref().is_some_and(|(r, _)| *r == i);
    let n = crate::model_count(&cfg.providers[i]);
    let names: Vec<String> = cfg.providers.iter().map(|p| p.name.clone()).collect();
    let pr = &mut cfg.providers[i];
    let special = pr.is_opencode();
    let title = pr.name.clone();
    let (mut act, mut commit) = (None, false);
    head(ui, |ui| {
        let mut on = !pr.disabled;
        if switch(ui, &mut on, "").on_hover_text("Off: not offered in the pickers and not used for routing.").changed() {
            pr.disabled = !on;
        }
        if !special && !renaming && icon_btn(ui, Icon::Delete).on_hover_text("Delete this provider (its tiers go back to Claude)").clicked() {
            act = Some(Act::Delete(i));
        }
        if renaming {
            commit |= icon_btn(ui, Icon::Save).on_hover_text("Save the name").clicked();
        } else if !special && icon_btn(ui, Icon::Rename).on_hover_text("Rename").clicked() {
            (ed.rename, ed.focus, ed.rename_err) = (Some((i, pr.name.clone())), true, String::new());
        }
        match &mut ed.rename {
            Some((r, text)) if *r == i => {
                ui.with_layout(Layout::left_to_right(eframe::egui::Align::Center), |ui| {
                    let te = ui.add(TextEdit::singleline(text).desired_width(ui.available_width() - 8.0));
                    if ed.focus {
                        te.request_focus();
                        ed.focus = false;
                    }
                    commit |= te.lost_focus() && ui.input(|inp| inp.key_pressed(Key::Enter));
                });
            }
            _ => name(ui, &title, on),
        }
    });
    if commit {
        act = ed.commit_rename(cfg, i).or(act);
    }
    if renaming && !ed.rename_err.is_empty() {
        note(ui, &ed.rename_err, p.danger);
    }
    group(ui, |ui, g| feed.analytics(ui, g, &title));
    let pr = &mut cfg.providers[i];
    row(ui, |ui, _| {
        ed.fetch_btn(ui, pr);
        count(ui, "Models", n);
    });
    if let Some(e) = ed.fetch_errs.get(&pr.name) {
        note(ui, e, p.danger);
    }
    if special {
        note(ui, "OpenCode's anonymous free models, proxied for you. No API key or account needed; the free tier's per-IP limits apply.", p.subtle);
        group(ui, |ui, g| {
            g.item(ui, |ui| field(ui, "URL", &mut pr.url, eframe::egui::Id::new(("ccat-url", i)), false, crate::providers::opencode::URL));
        });
        crate::providers::opencode::ui::pools(ui, &mut pr.opencode_egress, i);
        return act;
    }
    group(ui, |ui, g| {
        g.item(ui, |ui| field(ui, "URL", &mut pr.url, eframe::egui::Id::new(("ccat-url", i)), false, "https://api.example.com"));
        g.item(ui, |ui| {
            field(ui, "API key", &mut pr.key, eframe::egui::Id::new(("ccat-key", i)), true, "");
            if let Some(ps) = PRESETS.iter().find(|ps| ps.url == pr.url.trim().trim_end_matches('/')) {
                let link = eframe::egui::Label::new(eframe::egui::RichText::new("Get API key").size(11.0).color(p.subtle).underline()).sense(eframe::egui::Sense::click());
                if ui.add(link).on_hover_text(ps.keys).on_hover_cursor(eframe::egui::CursorIcon::PointingHand).clicked() {
                    crate::open_browser(ps.keys);
                }
            }
        });
        g.item(ui, |ui| {
            ui.horizontal(|ui| {
                switch(ui, &mut pr.openai, "OpenAI-compatible (Chat Completions)").on_hover_text("On: the URL is the API base with its version (…/v1) and requests are translated to /chat/completions. Off: an Anthropic Messages endpoint, forwarded as is.");
            });
        });
        g.item(ui, |ui| {
            ui.horizontal(|ui| {
                if switch(ui, &mut pr.free_only, "Only free models").on_hover_text("Fetch keeps the models named free (:free, -free). Fetch again after turning it off.").changed() && pr.free_only {
                    pr.models.retain(|m| is_free(m));
                }
            });
        });
    });
    // A new endpoint: one click fills a known provider's address.
    if pr.url.trim().is_empty() {
        note(ui, "Or start from a known provider:", p.subtle);
        ui.horizontal_wrapped(|ui| {
            for ps in PRESETS {
                if crate::settings::button(ui, ps.name).on_hover_text(ps.url).clicked() {
                    (pr.url, pr.openai) = (ps.url.into(), ps.openai);
                    if !ps.key.is_empty() {
                        pr.key = ps.key.into();
                    }
                    if pr.name.starts_with("Provider") && !names.iter().any(|n| n.eq_ignore_ascii_case(ps.name)) {
                        act = Some(Act::Rename(i, ps.name.into()));
                    }
                }
            }
        });
    }
    act
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_normalizes_v1() {
        assert_eq!(models_endpoint("https://x.io/"), "https://x.io/v1/models?limit=1000");
        assert_eq!(models_endpoint("https://x.io/api/v1"), "https://x.io/api/v1/models?limit=1000");
    }
}
