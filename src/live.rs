//! What each running session's proxy is doing, per tier: the route that served its last request, and requests held at
//! a usage limit with what they wait for. Each session writes `live/<pid>.json` next to the config; the usage window
//! reads them all to light the route in use and say what a held request waits for.

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::PathBuf};

/// A route as the config names it: provider ("" = Claude) and model.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Hop {
    pub provider: String,
    pub model: String,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Tier {
    /// The route that served the last request, and when.
    pub used: Option<Hop>,
    pub at: u64,
    /// Requests waiting at a usage limit, and per route when it is expected back (unix time, 0 = unknown).
    pub held: u32,
    pub waiting: Vec<(Hop, u64)>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Session {
    pub updated: u64,
    pub tiers: HashMap<String, Tier>,
}

pub fn dir() -> PathBuf {
    crate::config_path().with_file_name("live")
}

/// This process's file; its contents are only ever replaced whole.
pub fn write(s: &Session) {
    if cfg!(test) {
        return;
    }
    let d = dir();
    let _ = fs::create_dir_all(&d);
    let path = d.join(format!("{}.json", std::process::id()));
    let tmp = path.with_extension("tmp");
    if serde_json::to_vec(s).is_ok_and(|b| fs::write(&tmp, b).is_ok()) {
        let _ = fs::rename(&tmp, &path);
    }
}

/// Every running session merged per tier: the most recent route used in the last `RECENT` seconds, held requests
/// added up, the waits of every session (each route at its soonest). A session that stopped writing (it heartbeats
/// every minute) is left out, and its file removed once it is a day old.
pub fn read(now: u64) -> HashMap<String, Tier> {
    const STALE: u64 = 180;
    const RECENT: u64 = 15 * 60;
    let mut out: HashMap<String, Tier> = HashMap::new();
    for e in fs::read_dir(dir()).into_iter().flatten().flatten() {
        let Some(s) = fs::read(e.path()).ok().and_then(|b| serde_json::from_slice::<Session>(&b).ok()) else { continue };
        if s.updated + STALE < now {
            if s.updated + 86_400 < now {
                let _ = fs::remove_file(e.path());
            }
            continue;
        }
        for (name, t) in s.tiers {
            let m = out.entry(name).or_default();
            if t.used.is_some() && t.at + RECENT >= now && t.at > m.at {
                (m.used, m.at) = (t.used, t.at);
            }
            m.held += t.held;
            for (hop, back) in t.waiting.into_iter().filter(|_| t.held > 0) {
                match m.waiting.iter_mut().find(|w| w.0 == hop) {
                    Some(w) if back != 0 && (w.1 == 0 || back < w.1) => w.1 = back,
                    Some(_) => {}
                    None => m.waiting.push((hop, back)),
                }
            }
        }
    }
    out
}

/// Whether a config row (provider, model; "" model = Claude Code's pick) is this route.
pub fn matches(hop: &Hop, provider: &str, model: &str) -> bool {
    let id = crate::bare_id(model);
    hop.provider == provider && (id.is_empty() || hop.model == id.split('[').next().unwrap_or("").trim())
}

/// "GPT 6.1 Sol back in 3h 58m · Gemini X back time unknown": what a held tier waits for.
pub fn waits(t: &Tier, now: u64) -> String {
    let when = |back: u64| match back.saturating_sub(now) {
        _ if back == 0 => "back time unknown".to_string(),
        0..60 => "back any moment".to_string(),
        s if s < 3600 => format!("back in {}m", s / 60),
        s if s < 86_400 => format!("back in {}h {}m", s / 3600, s % 3600 / 60),
        s if s < 6 * 86_400 => format!("back in {}d {}h", s / 86_400, s % 86_400 / 3600),
        _ => {
            let (_, mo, d) = crate::usage::local_date(back);
            let (_, h, m) = crate::usage::local(back);
            format!("back on {d:02}/{mo:02}, {h:02}:{m:02}")
        }
    };
    t.waiting
        .iter()
        .map(|(h, back)| {
            let name = if h.model.is_empty() { "Claude".to_string() } else { crate::usage::pretty_model(&h.model) };
            format!("{name} {}", when(*back))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// "in 3h 58m", "on 30/10, 17:51", "any moment" or "time unknown": the soonest reset time among waiting routes.
pub fn nearest_wait(t: &Tier, now: u64) -> String {
    let nearest = t.waiting.iter().map(|(_, b)| *b).filter(|b| *b > now).min();
    match nearest {
        Some(back) => match back - now {
            0..60 => "any moment".to_string(),
            s if s < 3600 => format!("in {}m", s / 60),
            s if s < 86_400 => format!("in {}h {}m", s / 3600, s % 3600 / 60),
            s if s < 6 * 86_400 => format!("in {}d {}h", s / 86_400, s % 86_400 / 3600),
            _ => {
                let (_, mo, d) = crate::usage::local_date(back);
                let (_, h, m) = crate::usage::local(back);
                format!("on {d:02}/{mo:02}, {h:02}:{m:02}")
            }
        },
        None => "time unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_match_routes_and_waits_read_plainly() {
        let hop = |p: &str, m: &str| Hop { provider: p.into(), model: m.into() };
        assert!(matches(&hop("", "claude-opus-5-5"), "", ""), "Default is Claude Code's pick, whatever it sent");
        assert!(matches(&hop("Antigravity", "gemini-x"), "Antigravity", "gemini-x[1m]"), "a context hint doesn't count");
        assert!(!matches(&hop("ChatGPT", "gemini-x"), "Antigravity", "gemini-x"), "same id, other provider");
        let t = Tier { held: 1, waiting: vec![(hop("ChatGPT", "gpt-6.1-sol"), 1000 + 3 * 3600 + 58 * 60), (hop("Antigravity", "gemini-x"), 0)], ..Default::default() };
        assert_eq!(waits(&t, 1000), "GPT 6.1 Sol back in 3h 58m · Gemini X back time unknown");
        assert_eq!(nearest_wait(&t, 1000), "in 3h 58m");
    }
}
