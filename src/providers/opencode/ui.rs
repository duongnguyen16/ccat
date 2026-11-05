//! Pool controls show configured outbound addresses, never upstream server IPs.
use super::egress::{self, Config, Kind, NodeConfig};
use crate::settings::{button, group, providers::{field, note}};
use eframe::egui::{self, DragValue, Id, TextEdit};

pub(crate) fn pools(ui: &mut egui::Ui, config: &mut Config, provider: usize) {
    let p = crate::pal(ui);
    ui.label(egui::RichText::new("Outbound pools").size(14.5).color(p.fg));
    note(ui, "Sticky per conversation. Refusals never rotate IPs. Addresses below are your configured routes, not OpenCode server IPs.", p.subtle);
    let kinds = [("Direct", Kind::Direct), ("HTTP proxy", Kind::HttpProxy), ("HTTPS proxy", Kind::HttpsProxy), ("Bind IP (unsupported)", Kind::BindIp), ("SOCKS5 (unsupported)", Kind::Socks5)]
        .into_iter().map(|(name, kind)| (name.to_string(), kind)).collect::<Vec<_>>();
    let mut remove = None;
    for (index, node) in config.nodes.iter_mut().enumerate() {
        group(ui, |ui, g| {
            g.item(ui, |ui| {
                ui.horizontal(|ui| {
                    crate::switch(ui, &mut node.enabled, "");
                    ui.add(TextEdit::singleline(&mut node.name).desired_width(180.0).hint_text("Pool name"));
                    if crate::icon_btn(ui, crate::Icon::Delete).on_hover_text("Remove pool; active requests finish on their existing connection").clicked() { remove = Some(index); }
                });
                ui.horizontal(|ui| {
                    let salt = format!("opencode-egress-{provider}-{}", node.id);
                    if let Some(kind) = crate::settings::select(ui, &salt, 160.0, &kinds, &node.kind) { node.kind = kind; }
                    ui.label("Weight");
                    ui.add(DragValue::new(&mut node.weight).range(1..=10_000));
                    crate::switch(ui, &mut node.draining, "Drain").on_hover_text("No new requests; existing streams finish.");
                });
                if node.kind == Kind::Direct {
                    note(ui, "System route · no external IP probe", p.subtle);
                } else {
                    let (label, hint) = if node.kind == Kind::BindIp { ("IP", "192.168.1.20") } else { ("Proxy", "http://127.0.0.1:3128") };
                    field(ui, label, &mut node.address, Id::new(("egress-address", provider, &node.id)), false, hint);
                    field(ui, "User env", &mut node.username_env, Id::new(("egress-user", provider, &node.id)), false, "PROXY_USERNAME (optional)");
                    field(ui, "Pass env", &mut node.password_env, Id::new(("egress-pass", provider, &node.id)), false, "PROXY_PASSWORD (optional)");
                }
                let state = if !node.enabled { "Disabled" } else if node.draining { "Draining" } else { "Configured (not a health check)" };
                note(ui, state, p.subtle);
            });
        });
    }
    if let Some(index) = remove { config.nodes.remove(index); }
    if button(ui, "+ Add pool").clicked() {
        let id = format!("pool-{}", crate::common::random_hex(4));
        config.nodes.push(NodeConfig { id, name: "New pool".into(), ..Default::default() });
    }
    if let Err(error) = egress::validate(config) { note(ui, &error, p.danger); }
}
