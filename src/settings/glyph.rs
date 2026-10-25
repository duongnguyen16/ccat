//! The small icons in the Settings sidebar: vectors drawn with painter strokes (no icon font), and the providers' logos
//! from `media/png` (`art`, `Logos`). Each stroked glyph is
//! laid out for a 16 px square centred on `c`; the colour is the item's text colour.

use eframe::egui::{self, vec2, Color32, Pos2, Shape, Stroke};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Glyph {
    Sliders,
    Route,
    Leaf,
    Monitor,
    Info,
    Dashboard,
    Bars,
    Clock,
    Claude,
    Antigravity,
    ChatGpt,
    Ollama,
    Muse,
    Link,
    Plus,
}

/// Draws `glyph` centred on `c`.
pub(super) fn paint(pt: &egui::Painter, c: Pos2, glyph: Glyph, color: Color32) {
    let s = Stroke::new(1.3, color);
    let at = |x: f32, y: f32| c + vec2(x, y);
    let line = |a: (f32, f32), b: (f32, f32)| {
        pt.line_segment([at(a.0, a.1), at(b.0, b.1)], s);
    };
    let poly = |pts: &[(f32, f32)]| {
        pt.add(Shape::closed_line(pts.iter().map(|&(x, y)| at(x, y)).collect(), s));
    };
    let open = |pts: &[(f32, f32)]| {
        pt.add(Shape::line(pts.iter().map(|&(x, y)| at(x, y)).collect(), s));
    };
    match glyph {
        Glyph::Sliders => {
            for (y, knob) in [(-5.0, -3.0), (0.0, 3.0), (5.0, -1.0)] {
                line((-7.0, y), (7.0, y));
                pt.circle_filled(at(knob, y), 2.2, color);
            }
        }
        Glyph::Route => {
            // One line that forks in two, each branch ending in an arrowhead.
            open(&[(-7.0, 0.0), (-2.0, 0.0), (1.0, -4.5), (6.0, -4.5)]);
            open(&[(-2.0, 0.0), (1.0, 4.5), (6.0, 4.5)]);
            open(&[(3.5, -7.0), (6.5, -4.5), (3.5, -2.0)]);
            open(&[(3.5, 2.0), (6.5, 4.5), (3.5, 7.0)]);
        }
        Glyph::Leaf => {
            // A lens from corner to corner, with its midrib and a short stem.
            let (a, b) = (vec2(-5.0, 5.0), vec2(6.0, -6.0));
            let n = vec2(1.0, 1.0).normalized() * 4.2;
            let side = |k: f32| (0..=12).map(move |i| {
                let t = i as f32 / 12.0;
                c + a + (b - a) * t + n * k * (t * std::f32::consts::PI).sin()
            });
            let mut ring: Vec<Pos2> = side(1.0).collect();
            ring.extend(side(-1.0).collect::<Vec<_>>().into_iter().rev().skip(1));
            pt.add(Shape::closed_line(ring, s));
            line((-6.5, 6.5), (3.0, -3.0));
        }
        Glyph::Monitor => {
            pt.rect_stroke(egui::Rect::from_min_max(at(-7.0, -6.0), at(7.0, 3.0)), 1.5, s, egui::StrokeKind::Middle);
            line((0.0, 3.0), (0.0, 6.5));
            line((-3.5, 6.5), (3.5, 6.5));
        }
        Glyph::Info => {
            pt.circle_stroke(c, 7.0, s);
            pt.circle_filled(at(0.0, -3.0), 1.0, color);
            line((0.0, -0.5), (0.0, 4.0));
        }
        Glyph::Dashboard => {
            // Four tiles of different sizes.
            for (a, b) in [((-7.0, -7.0), (-1.0, 0.0)), ((1.0, -7.0), (7.0, -2.0)), ((-7.0, 2.0), (-1.0, 7.0)), ((1.0, 0.0), (7.0, 7.0))] {
                pt.rect_stroke(egui::Rect::from_min_max(at(a.0, a.1), at(b.0, b.1)), 1.5, s, egui::StrokeKind::Middle);
            }
        }
        Glyph::Bars => {
            for (x, top) in [(-6.5, 0.0), (-1.5, -7.0), (3.5, -3.5)] {
                pt.rect_filled(egui::Rect::from_min_max(at(x, top), at(x + 3.0, 7.0)), 1.0, color);
            }
        }
        Glyph::Clock => {
            pt.circle_stroke(c, 7.0, s);
            open(&[(0.0, -4.0), (0.0, 0.0), (3.2, 1.8)]);
        }
        Glyph::Claude => {
            // A burst: eight rays, the diagonal ones shorter.
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                let (dir, long) = (vec2(a.cos(), a.sin()), i % 2 == 0);
                pt.line_segment([c + dir * 1.6, c + dir * if long { 7.5 } else { 5.5 }], Stroke::new(1.6, color));
            }
        }
        Glyph::Antigravity => poly(&[(0.0, -7.5), (7.0, 6.5), (0.0, 3.0), (-7.0, 6.5)]),
        Glyph::ChatGpt => {
            let hex: Vec<(f32, f32)> = (0..6).map(|i| {
                let a = (i as f32 * 60.0 - 90.0).to_radians();
                (a.cos() * 7.2, a.sin() * 7.2)
            }).collect();
            poly(&hex);
            for i in [0, 2, 4] {
                line((0.0, 0.0), hex[i]);
            }
        }
        Glyph::Ollama => {
            for y in [-7.0, 1.0] {
                pt.rect_stroke(egui::Rect::from_min_max(at(-7.0, y), at(7.0, y + 6.0)), 1.5, s, egui::StrokeKind::Middle);
                pt.circle_filled(at(3.5, y + 3.0), 1.0, color);
            }
        }
        Glyph::Muse => poly(&[(1.5, -8.0), (-5.0, 1.0), (-0.5, 1.0), (-1.5, 8.0), (5.0, -1.5), (0.5, -1.5)]),
        Glyph::Link => {
            pt.circle_stroke(c, 7.0, s);
            line((-7.0, 0.0), (7.0, 0.0));
            let ellipse: Vec<Pos2> = (0..24).map(|i| {
                let a = i as f32 * std::f32::consts::TAU / 24.0;
                c + vec2(a.cos() * 3.4, a.sin() * 7.0)
            }).collect();
            pt.add(Shape::closed_line(ellipse, s));
        }
        Glyph::Plus => {
            line((-6.0, 0.0), (6.0, 0.0));
            line((0.0, -6.0), (0.0, 6.0));
        }
    }
}

/// A provider's logo, from `media/png` (96 px, transparent): the sidebar paints it in place of the stroked glyph.
#[derive(Clone, Copy, Debug)]
pub(super) struct Art {
    pub file: &'static str,
    pub png: &'static [u8],
    /// A white mark: tinted to the item's text colour, so it follows the theme and an off provider's dimming. A brand
    /// logo keeps its own colours and is only faded.
    pub mono: bool,
}

/// The logo of `glyph`'s provider, if it has one; the rest stay painter-drawn.
pub(super) fn art(glyph: Glyph) -> Option<Art> {
    macro_rules! art {
        ($file:literal, $mono:literal) => {
            Some(Art { file: $file, png: include_bytes!(concat!("../../media/png/", $file)), mono: $mono })
        };
    }
    match glyph {
        Glyph::Claude => art!("claude-code.png", false),
        Glyph::Antigravity => art!("google-antigravity.png", false),
        Glyph::ChatGpt => art!("openai-chatgpt.png", true),
        Glyph::Ollama => art!("ollama-dark.png", true),
        Glyph::Muse => art!("Muse-logo.png", false),
        _ => None,
    }
}

/// The logos' textures, each decoded the first time it is painted.
#[derive(Default)]
pub(super) struct Logos(HashMap<&'static str, Option<egui::TextureHandle>>);

impl Logos {
    /// Paints `glyph`'s logo, `size` points square, centred on `c`: tinted `color` when it is a white mark, faded to
    /// `fade` (0 to 1) otherwise. `false` when it has none (or it cannot be decoded): paint the glyph instead.
    pub(super) fn paint(&mut self, ui: &egui::Ui, c: Pos2, size: f32, glyph: Glyph, color: Color32, fade: f32) -> bool {
        let Some(art) = art(glyph) else { return false };
        let tex = self.0.entry(art.file).or_insert_with(|| {
            let icon = eframe::icon_data::from_png_bytes(art.png).ok()?;
            // Averaged down first: the GPU's plain minify of 96 px to 18 would alias.
            let image = crate::usage::shrink(&icon.rgba, icon.width as usize, icon.height as usize, 48);
            Some(ui.ctx().load_texture(art.file, image, egui::TextureOptions::LINEAR))
        });
        let Some(tex) = tex else { return false };
        let tint = if art.mono { color } else { Color32::WHITE.gamma_multiply(fade) };
        egui::Image::new(&*tex).fit_to_exact_size(vec2(size, size)).tint(tint).paint_at(ui, egui::Rect::from_center_size(c, vec2(size, size)));
        true
    }
}
