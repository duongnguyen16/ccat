//! Configurations: named sets of the four tier routes, one file each in `profiles/` next to the config.
//! `Config::tiers` always holds the applied set (running sessions follow it); `Config::profile` names the file it
//! came from, and tier edits are written to both. The logic takes the folder as a parameter so it can be tested.

use crate::{pal, Config, Route};
use eframe::egui::{self, vec2, Align, Align2, Color32, FontId, Layout, RichText, Sense, Stroke, StrokeKind, TextEdit};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub fn dir() -> PathBuf {
    crate::config_path().with_file_name("profiles")
}

fn file(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.json"))
}

fn same(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// The profile names on disk, alphabetical (the file stems; temp files end in `.tmp` and don't count).
pub fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    v.sort_by_key(|n| n.to_lowercase());
    v
}

/// `{ "tiers": [four routes] }`, normalised like the config on load. Strict: an import must really be one.
fn parse(text: &str) -> Result<[Route; 4], String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON ({e})"))?;
    let tiers = v.get("tiers").ok_or("it has no “tiers”")?;
    let mut tiers: [Route; 4] = serde_json::from_value(tiers.clone()).map_err(|e| format!("“tiers” should be four routes ({e})"))?;
    for r in &mut tiers {
        r.model = crate::bare_id(&r.model);
        r.fallbacks.extend(r.fallback.take());
    }
    Ok(tiers)
}

pub fn load(dir: &Path, name: &str) -> Option<[Route; 4]> {
    fs::read_to_string(file(dir, name)).ok().and_then(|t| parse(&t).ok())
}

/// Temp file + rename, like the config: another ccat window never reads half a profile.
pub fn write(dir: &Path, name: &str, tiers: &[Route; 4]) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let p = file(dir, name);
    let tmp = p.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(&serde_json::json!({ "tiers": tiers })).unwrap()).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// Writes the applied tiers to the applied profile's file when it differs.
pub fn sync(dir: &Path, cfg: &Config) {
    if !cfg.profile.is_empty() && load(dir, &cfg.profile).as_ref() != Some(&cfg.tiers) {
        let _ = write(dir, &cfg.profile, &cfg.tiers);
    }
}

/// Runs `edit` over every profile file and rewrites those it changed: a provider renamed or deleted must reach the
/// configurations that aren't applied too, not only the applied one.
pub fn edit_all(dir: &Path, edit: impl Fn(&mut [Route; 4])) {
    for n in names(dir) {
        if let Some(mut tiers) = load(dir, &n) {
            let old = tiers.clone();
            edit(&mut tiers);
            if tiers != old {
                let _ = write(dir, &n, &tiers);
            }
        }
    }
}

/// Makes `name` the applied profile: its tiers become `cfg.tiers`.
pub fn apply(dir: &Path, cfg: &mut Config, name: &str) -> Result<(), String> {
    cfg.tiers = load(dir, name).ok_or_else(|| format!("“{name}” can't be read."))?;
    cfg.profile = name.into();
    Ok(())
}

/// First run (no profiles yet): the current tiers become `Default`. A config naming a profile that isn't on disk
/// (reset, or the file deleted by hand) falls back to the first readable one. Returns whether `cfg` changed.
pub fn ensure(dir: &Path, cfg: &mut Config) -> bool {
    let all = names(dir);
    if all.is_empty() {
        let _ = write(dir, "Default", &cfg.tiers);
        cfg.profile = "Default".into();
        return true;
    }
    if let Some(n) = all.iter().find(|n| same(n, &cfg.profile)) {
        let changed = *n != cfg.profile;
        cfg.profile = n.clone();
        return changed;
    }
    all.iter().any(|n| apply(dir, cfg, n).is_ok())
}

/// A valid, unused name (trimmed) for a profile; `own` is the one being renamed, which may keep its name.
pub fn check_name(all: &[String], name: &str, own: &str) -> Result<String, String> {
    let name = name.trim();
    let stem = name.split('.').next().unwrap_or("").to_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str()) || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.ends_with(|c: char| c.is_ascii_digit()));
    if name.is_empty() {
        Err("Give it a name.".into())
    } else if name.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)) {
        Err("A name can't contain < > : \" / \\ | ? *".into())
    } else if name.ends_with('.') || reserved {
        Err("Windows can't use that name for a file.".into())
    } else if all.iter().any(|n| same(n, name) && !same(n, own)) {
        Err("Another configuration already has that name.".into())
    } else {
        Ok(name.into())
    }
}

/// `base`, or `base 2`, `base 3`… — the first not taken.
pub fn unique(all: &[String], base: &str) -> String {
    let taken = |n: &str| all.iter().any(|x| same(x, n));
    std::iter::once(base.to_string()).chain((2..).map(|i| format!("{base} {i}"))).find(|n| !taken(n)).unwrap()
}

/// The actions below all write the file first and change `cfg` only when that worked, then apply the result.
pub fn create(dir: &Path, cfg: &mut Config, name: &str, tiers: [Route; 4]) -> Result<(), String> {
    write(dir, name, &tiers)?;
    (cfg.tiers, cfg.profile) = (tiers, name.into());
    Ok(())
}

/// A copy of the applied profile under `name`, applied.
pub fn duplicate(dir: &Path, cfg: &mut Config, name: &str) -> Result<(), String> {
    create(dir, cfg, name, cfg.tiers.clone())
}

/// Renames the applied profile's file (a case-only change works too).
pub fn rename(dir: &Path, cfg: &mut Config, name: &str) -> Result<(), String> {
    if name != cfg.profile {
        fs::rename(file(dir, &cfg.profile), file(dir, name)).map_err(|e| format!("Couldn't rename it ({e})."))?;
        cfg.profile = name.into();
    }
    Ok(())
}

/// Deletes the applied profile's file and applies the first one left.
pub fn delete(dir: &Path, cfg: &mut Config) -> Result<(), String> {
    fs::remove_file(file(dir, &cfg.profile)).map_err(|e| format!("Couldn't delete it ({e})."))?;
    cfg.profile.clear();
    ensure(dir, cfg);
    Ok(())
}

/// Adds the profile in `text` (a file's content) under `stem`, made unique, and applies it.
pub fn import(dir: &Path, cfg: &mut Config, stem: &str, text: &str) -> Result<(), String> {
    let tiers = parse(text).map_err(|e| format!("That isn't a ccat configuration: {e}."))?;
    let all = names(dir);
    let name = unique(&all, stem);
    let name = check_name(&all, &name, "").unwrap_or_else(|_| unique(&all, "Imported configuration"));
    create(dir, cfg, &name, tiers)
}

// ---------------------------------------------------------------- the bar and its menu

enum Act {
    Switch(String),
    New,
    Duplicate,
    Rename,
    Import,
    Reveal,
    Delete,
}

enum Dialog {
    Duplicate,
    Rename,
    Delete,
    Error(String),
}

/// The "● name ⌄" button above the tiers, its menu, and the small dialogs the menu opens.
#[derive(Default)]
pub struct Bar {
    menu: bool,
    dialog: Option<Dialog>,
    /// The name being typed in the Duplicate / Rename dialog.
    text: String,
    focus: bool,
    /// When our own file dialog closed: the window lost and regained focus around it.
    dialog_at: Option<Instant>,
}

/// Opens `folder` in the file manager.
fn reveal(folder: &Path) {
    let _ = fs::create_dir_all(folder);
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(folder).spawn();
    #[cfg(not(windows))]
    crate::open_browser(&folder.to_string_lossy());
}

pub fn green(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode { crate::hex(0x4cc38a) } else { crate::hex(0x1f8f5f) }
}

/// A menu entry: text on the left, a fill on hover.
fn item(ui: &mut egui::Ui, text: &str, enabled: bool, danger: bool) -> bool {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), if enabled { Sense::click() } else { Sense::hover() });
    if enabled && resp.hovered() {
        ui.painter().rect_filled(rect, 5.0, p.hover);
    }
    let color = match (enabled, danger) {
        (false, _) => p.subtle.gamma_multiply(0.6),
        (_, true) => p.danger,
        _ => p.fg,
    };
    ui.painter().text(rect.left_center() + vec2(26.0, 0.0), Align2::LEFT_CENTER, text, FontId::proportional(13.0), color);
    enabled && resp.clicked()
}

fn sep(ui: &mut egui::Ui) {
    ui.add_space(3.0);
    ui.separator();
    ui.add_space(3.0);
}

impl Bar {
    /// A menu, dialog or error is up: Esc and Enter belong to it, and the popup needs room for it.
    pub fn active(&self) -> bool {
        self.menu || self.dialog.is_some()
    }

    /// Our file dialog closed a moment ago, so a focus loss isn't the user clicking away.
    pub fn recent_dialog(&self) -> bool {
        self.dialog_at.is_some_and(|t| t.elapsed() < Duration::from_secs(1))
    }

    /// `title` on the left, the profile button on the right; applying, duplicating, renaming, importing, deleting
    /// act on `cfg` (and the files) right away.
    pub fn show(&mut self, ui: &mut egui::Ui, cfg: &mut Config, frame: &eframe::Frame, title: impl FnOnce(&mut egui::Ui)) {
        let p = pal(ui);
        let (folder, ctx) = (dir(), ui.ctx().clone());
        let mut anchor = egui::Rect::NOTHING;
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::left_to_right(Align::Center), |ui| {
            ui.set_min_size(vec2(ui.available_width(), 26.0));
            title(ui);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let galley = ui.painter().layout_no_wrap(cfg.profile.clone(), FontId::proportional(13.0), p.fg);
                let w = (galley.size().x + 52.0).min(ui.available_width()).min(280.0);
                let (rect, resp) = ui.allocate_exact_size(vec2(w, 26.0), Sense::click());
                let st = ui.style().interact(&resp);
                ui.painter().rect(rect, 6.0, p.sunken, if self.menu { ui.visuals().widgets.active.bg_stroke } else { st.bg_stroke }, StrokeKind::Inside);
                ui.painter().circle_filled(rect.left_center() + vec2(12.0, 0.0), 3.5, green(ui));
                let text = rect.shrink2(vec2(10.0, 0.0));
                ui.painter().with_clip_rect(text.with_min_x(text.min.x + 14.0).with_max_x(text.max.x - 14.0)).text(text.left_center() + vec2(14.0, 0.0), Align2::LEFT_CENTER, &cfg.profile, FontId::proportional(13.0), p.fg);
                ui.painter().text(text.right_center(), Align2::RIGHT_CENTER, "⏷", FontId::proportional(10.0), p.subtle);
                if resp.on_hover_text("Configurations: named sets of the four tiers").clicked() {
                    self.menu = !self.menu;
                }
                anchor = rect;
            });
        });

        let mut act = None;
        if self.menu {
            let all = names(&folder);
            let area = egui::Area::new(ui.id().with("profile-menu"))
                .order(egui::Order::Foreground)
                .fixed_pos(anchor.right_bottom() + vec2(0.0, 4.0))
                .pivot(Align2::RIGHT_TOP)
                .constrain(true)
                .show(&ctx, |ui| {
                    egui::Frame::popup(ui.style()).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
                        ui.set_width(250.0);
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.add_space(2.0);
                        ui.label(RichText::new("Configurations").size(11.0).color(p.subtle));
                        ui.add_space(3.0);
                        egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                            for n in &all {
                                let applied = *n == cfg.profile;
                                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
                                if resp.hovered() {
                                    ui.painter().rect_filled(rect, 5.0, p.hover);
                                }
                                let r = rect.shrink2(vec2(8.0, 0.0));
                                let mut right = r.max.x;
                                if applied {
                                    ui.painter().text(r.left_center(), Align2::LEFT_CENTER, "✔", FontId::proportional(11.0), green(ui));
                                    let badge = ui.painter().layout_no_wrap("applied".into(), FontId::proportional(10.5), green(ui));
                                    let b = egui::Rect::from_center_size(egui::pos2(r.max.x - badge.size().x / 2.0 - 6.0, r.center().y), badge.size() + vec2(12.0, 4.0));
                                    ui.painter().rect_filled(b, 8.0, green(ui).gamma_multiply(0.18));
                                    ui.painter().galley(b.center() - badge.size() / 2.0, badge, green(ui));
                                    right = b.min.x - 6.0;
                                }
                                ui.painter().with_clip_rect(r.with_max_x(right)).text(r.left_center() + vec2(18.0, 0.0), Align2::LEFT_CENTER, n, FontId::proportional(13.0), p.fg);
                                if resp.clicked() {
                                    act = Some(Act::Switch(n.clone()));
                                }
                            }
                        });
                        sep(ui);
                        for (text, a) in [("Duplicate…", Act::Duplicate), ("Rename…", Act::Rename), ("New configuration", Act::New), ("Import configuration…", Act::Import)] {
                            if item(ui, text, true, false) {
                                act = Some(a);
                            }
                        }
                        sep(ui);
                        if item(ui, "Show in Explorer", true, false) {
                            act = Some(Act::Reveal);
                        }
                        if item(ui, "Delete", all.len() > 1, true) {
                            act = Some(Act::Delete);
                        }
                    });
                });
            let away = ui.input(|i| i.pointer.any_click() && i.pointer.interact_pos().is_some_and(|at| !area.response.rect.contains(at) && !anchor.contains(at)));
            if away || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.menu = false;
            }
        }
        if let Some(a) = act {
            self.run(a, cfg, frame, &folder);
        }
        self.dialogs(&ctx, p, cfg, &folder);
    }

    /// Edits so far are written first, so switching away never loses them.
    fn run(&mut self, act: Act, cfg: &mut Config, frame: &eframe::Frame, folder: &Path) {
        sync(folder, cfg);
        let all = names(folder);
        self.menu = false;
        let result = match act {
            Act::Switch(n) if n == cfg.profile => Ok(()),
            Act::Switch(n) => apply(folder, cfg, &n),
            Act::New => create(folder, cfg, &unique(&all, "New configuration"), Default::default()),
            Act::Duplicate | Act::Rename => {
                self.text = if matches!(act, Act::Rename) { cfg.profile.clone() } else { unique(&all, &format!("{} copy", cfg.profile)) };
                (self.dialog, self.focus) = (Some(if matches!(act, Act::Rename) { Dialog::Rename } else { Dialog::Duplicate }), true);
                Ok(())
            }
            Act::Delete => {
                self.dialog = Some(Dialog::Delete);
                Ok(())
            }
            Act::Reveal => Ok(reveal(folder)),
            Act::Import => {
                let picked = rfd::FileDialog::new().set_parent(frame).add_filter("ccat configuration", &["json"]).pick_file();
                self.dialog_at = Some(Instant::now());
                match picked {
                    None => Ok(()),
                    Some(path) => {
                        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        fs::read_to_string(&path).map_err(|e| format!("Couldn't read the file ({e}).")).and_then(|t| import(folder, cfg, &stem, &t))
                    }
                }
            }
        };
        if let Err(e) = result {
            self.dialog = Some(Dialog::Error(e));
        }
    }

    /// The name / confirm / error dialog, if one is open.
    fn dialogs(&mut self, ctx: &egui::Context, p: &crate::Pal, cfg: &mut Config, folder: &Path) {
        let Some(dialog) = self.dialog.take() else { return };
        let all = names(folder);
        let (mut ok, mut cancel) = (false, false);
        let check = match dialog {
            Dialog::Duplicate => check_name(&all, &self.text, ""),
            Dialog::Rename => check_name(&all, &self.text, &cfg.profile),
            _ => Ok(String::new()),
        };
        let m = egui::Modal::new(egui::Id::new("profile-dialog")).show(ctx, |ui| {
            ui.set_width(280.0);
            let title = match &dialog {
                Dialog::Duplicate => "Duplicate configuration".to_string(),
                Dialog::Rename => "Rename configuration".to_string(),
                Dialog::Delete => format!("Delete “{}”?", cfg.profile),
                Dialog::Error(_) => "Configuration".to_string(),
            };
            ui.label(RichText::new(title).family(crate::serif()).size(17.0));
            ui.add_space(6.0);
            match &dialog {
                Dialog::Duplicate | Dialog::Rename => {
                    let te = ui.add(TextEdit::singleline(&mut self.text).hint_text("Name").desired_width(f32::INFINITY));
                    if self.focus {
                        te.request_focus();
                        self.focus = false;
                    }
                    ok = te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    match &check {
                        Err(e) if !self.text.trim().is_empty() => ui.label(RichText::new(e).size(11.5).color(p.danger)),
                        _ => ui.label(RichText::new(" ").size(11.5)),
                    };
                }
                Dialog::Delete => {
                    ui.add(egui::Label::new(RichText::new("Its file is removed for good. The first of the others is applied.").size(12.0).color(p.muted)).wrap());
                }
                Dialog::Error(e) => {
                    ui.add(egui::Label::new(RichText::new(e).size(12.0).color(p.danger)).wrap());
                }
            }
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (label, fill, enabled) = match &dialog {
                    Dialog::Delete => ("Delete", p.danger, true),
                    Dialog::Error(_) => ("OK", p.fg, true),
                    _ => ("OK", p.fg, check.is_ok()),
                };
                let go = egui::Button::new(RichText::new(label).color(if matches!(dialog, Dialog::Delete) { Color32::WHITE } else { p.bg }).strong()).fill(fill).stroke(Stroke::NONE).min_size(vec2(72.0, 28.0));
                ok |= ui.add_enabled(enabled, go).clicked();
                if !matches!(dialog, Dialog::Error(_)) {
                    cancel |= ui.add(egui::Button::new(RichText::new("Cancel").color(p.muted)).frame(false)).clicked();
                }
            });
        });
        if cancel || m.should_close() {
            return;
        }
        if !ok {
            self.dialog = Some(dialog);
            return;
        }
        let result = match (&dialog, check) {
            (Dialog::Error(_), _) => Ok(()),
            (Dialog::Delete, _) => delete(folder, cfg),
            (_, Err(_)) => {
                self.dialog = Some(dialog);
                return;
            }
            (Dialog::Rename, Ok(name)) => rename(folder, cfg, &name),
            (_, Ok(name)) => duplicate(folder, cfg, &name),
        };
        if let Err(e) = result {
            self.dialog = Some(Dialog::Error(e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ccat-profiles-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn route(provider: &str, model: &str) -> Route {
        Route { provider: provider.into(), model: model.into(), ..Default::default() }
    }

    fn tiers(model: &str) -> [Route; 4] {
        std::array::from_fn(|_| route("OR", model))
    }

    #[test]
    fn first_run_makes_default_from_the_current_tiers() {
        let d = tmp("migrate");
        let mut cfg = Config { tiers: tiers("m"), ..Default::default() };
        assert!(ensure(&d, &mut cfg));
        assert_eq!((cfg.profile.as_str(), names(&d)), ("Default", vec!["Default".to_string()]));
        assert_eq!(load(&d, "Default"), Some(tiers("m")));
        assert!(!ensure(&d, &mut cfg), "nothing to do the second time");

        // The applied profile's file was deleted by hand: the first readable one is applied.
        write(&d, "Other", &tiers("o")).unwrap();
        fs::remove_file(file(&d, "Default")).unwrap();
        assert!(ensure(&d, &mut cfg));
        assert_eq!((cfg.profile.as_str(), cfg.tiers.clone()), ("Other", tiers("o")));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn edits_sync_and_switching_applies() {
        let d = tmp("sync");
        let mut cfg = Config { tiers: tiers("a"), profile: "A".into(), ..Default::default() };
        write(&d, "A", &tiers("a")).unwrap();
        write(&d, "B", &tiers("b")).unwrap();
        cfg.tiers = tiers("a2"); // edited in an editor
        sync(&d, &cfg);
        assert_eq!(load(&d, "A"), Some(tiers("a2")));
        apply(&d, &mut cfg, "B").unwrap();
        assert_eq!((cfg.profile.as_str(), cfg.tiers.clone()), ("B", tiers("b")));
        assert!(apply(&d, &mut cfg, "Nope").is_err());
        assert_eq!(cfg.profile, "B", "a failed apply changes nothing");
        assert_eq!(names(&d), ["A", "B"]);
        assert!(!fs::read_dir(&d).unwrap().flatten().any(|e| e.path().extension().is_some_and(|x| x == "tmp")), "no temp file left");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn names_are_checked_and_made_unique() {
        let all = vec!["Work".to_string(), "Home".to_string()];
        assert_eq!(check_name(&all, "  Play ", ""), Ok("Play".into()));
        assert!(check_name(&all, "", "").is_err() && check_name(&all, "  ", "").is_err());
        assert!(["a/b", "a\\b", "a:b", "a*", "a?", "a\"", "a<", "a>", "a|", "x.", "CON", "com1"].iter().all(|n| check_name(&all, n, "").is_err()));
        assert!(check_name(&all, "WORK", "").is_err(), "case-insensitive");
        assert_eq!(check_name(&all, "WORK", "Work"), Ok("WORK".into()), "a rename may change its own case");
        assert_eq!(unique(&all, "Work"), "Work 2");
        assert_eq!(unique(&all, "Play"), "Play");
        assert_eq!(unique(&["Work".into(), "work 2".into()], "Work"), "Work 3");
    }

    #[test]
    fn import_is_strict_and_normalises() {
        assert!(parse("{}").is_err() && parse("nope").is_err() && parse(r#"{"tiers":[{}]}"#).is_err());
        let ok = parse(r#"{"tiers":[{"provider":"OR","model":"anthropic/m","fallback":{"provider":"X","model":"y"}},{},{},{}]}"#).unwrap();
        assert_eq!(ok[0].model, "m");
        assert_eq!(ok[0].fallbacks.len(), 1, "a legacy single fallback joins the chain");
        let d = tmp("round");
        write(&d, "X", &ok).unwrap();
        assert_eq!(load(&d, "X"), Some(ok));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn menu_actions_keep_files_and_config_in_step() {
        let d = tmp("actions");
        let mut cfg = Config { tiers: tiers("a"), profile: "A".into(), ..Default::default() };
        write(&d, "A", &tiers("a")).unwrap();

        duplicate(&d, &mut cfg, "A copy").unwrap();
        assert_eq!((cfg.profile.as_str(), cfg.tiers.clone(), names(&d)), ("A copy", tiers("a"), vec!["A".to_string(), "A copy".into()]));
        rename(&d, &mut cfg, "Work").unwrap();
        assert_eq!((cfg.profile.as_str(), names(&d)), ("Work", vec!["A".to_string(), "Work".into()]));
        rename(&d, &mut cfg, "WORK").unwrap();
        assert_eq!(names(&d), ["A", "WORK"], "case-only rename");
        create(&d, &mut cfg, "Blank", Default::default()).unwrap();
        assert_eq!((cfg.tiers.clone(), load(&d, "Blank")), (<[Route; 4]>::default(), Some(<[Route; 4]>::default())));

        // Deleting the applied one applies the first of the rest.
        delete(&d, &mut cfg).unwrap();
        assert_eq!((cfg.profile.as_str(), cfg.tiers.clone(), names(&d)), ("A", tiers("a"), vec!["A".to_string(), "WORK".into()]));

        let file_text = serde_json::json!({ "tiers": tiers("imp") }).to_string();
        import(&d, &mut cfg, "A", &file_text).unwrap();
        assert_eq!((cfg.profile.as_str(), cfg.tiers.clone()), ("A 2", tiers("imp")), "an imported name that's taken is made unique");
        assert!(import(&d, &mut cfg, "Bad", "{\"tiers\": 3}").is_err());
        assert_eq!(cfg.profile, "A 2", "a rejected import changes nothing");
        assert_eq!(names(&d).len(), 3);
        let _ = fs::remove_dir_all(&d);
    }
}
