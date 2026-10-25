//! The Analytics and History pages: what the proxy logged in `tokens.jsonl`, one line per answered request. The file's
//! tail is read into memory once and then only what is appended (`Log`); the pages filter and add that up
//! (`analyse`, `history`, both plain functions of the loaded calls and a query, so they can be tested without a window).

use super::{button, group, heading, select};
use crate::usage::{self, Bars, Call, Pricing};
use eframe::egui::{self, vec2, Align, Align2, Color32, FontId, Layout, RichText, Sense, TextEdit};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

/// How often the file is looked at.
const POLL: Duration = Duration::from_secs(3);
/// The most of the file's end read, and the most requests kept: an old, huge log still opens quickly.
/// ponytail: older requests are simply not shown; raise the caps (or aggregate by day on load) if that bites.
const MAX_BYTES: u64 = 32 << 20;
const MAX_CALLS: usize = 200_000;
/// History rows added per "Load more".
const PAGE: usize = 200;
/// The most models a filter lists (the busiest ones).
const MODEL_CHOICES: usize = 14;

// ---------------------------------------------------------------- the log

/// The requests of `tokens.jsonl`, oldest first as the file has them.
pub(super) struct Log {
    path: PathBuf,
    /// How much of the file has been read: always the end of a whole line.
    len: u64,
    pricing: Pricing,
    pricing_stamp: Option<SystemTime>,
    pub calls: Vec<Call>,
    /// Changes whenever `calls` does.
    pub version: u64,
    polled: Option<Instant>,
    /// Requests per provider, tier and model: what the filters list.
    counts: [HashMap<String, usize>; 3],
}

impl Log {
    pub(super) fn new() -> Self {
        Self::with(usage::tokens_path(), usage::load_pricing())
    }

    fn with(path: PathBuf, pricing: Pricing) -> Self {
        let pricing_stamp = fs::metadata(usage::pricing_path()).and_then(|m| m.modified()).ok();
        Self { path, len: 0, pricing, pricing_stamp, calls: Vec::new(), version: 0, polled: None, counts: Default::default() }
    }

    /// Takes in what was appended since the last look, at most every few seconds.
    pub(super) fn poll(&mut self) {
        if self.polled.is_some_and(|t| t.elapsed() < POLL) {
            return;
        }
        self.polled = Some(Instant::now());
        let stamp = fs::metadata(usage::pricing_path()).and_then(|m| m.modified()).ok();
        if stamp != self.pricing_stamp {
            // New prices change every cost: read it all again.
            (self.pricing_stamp, self.pricing) = (stamp, usage::load_pricing());
            self.reset();
        }
        self.refresh();
    }

    fn reset(&mut self) {
        self.calls.clear();
        self.counts = Default::default();
        self.len = 0;
        self.version += 1;
    }

    /// Reads what the file gained since `len`; a file that shrank (cleared by hand) is read from the start.
    fn refresh(&mut self) {
        let size = fs::metadata(&self.path).map_or(0, |m| m.len());
        if size < self.len {
            self.reset();
        }
        if size == self.len {
            return;
        }
        let first = self.len == 0;
        let from = if first { size.saturating_sub(MAX_BYTES) } else { self.len };
        let Some(buf) = fs::File::open(&self.path).and_then(|mut f| {
            f.seek(SeekFrom::Start(from))?;
            let mut buf = Vec::new();
            f.take(size - from).read_to_end(&mut buf).map(|_| buf)
        }).ok() else {
            return;
        };
        // Whole lines only: a request being appended right now is there next time.
        let Some(end) = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1) else { return };
        // A big file is entered mid-line: start at its first whole one.
        let start = if first && from > 0 { buf.iter().position(|&b| b == b'\n').map_or(end, |i| i + 1) } else { 0 };
        self.len = from + end as u64;
        self.ingest(&String::from_utf8_lossy(&buf[start.min(end)..end]));
    }

    fn ingest(&mut self, text: &str) {
        let before = self.calls.len();
        for v in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
            if let Some(c) = Call::parse(&v, &self.pricing) {
                for (k, name) in [&c.provider, &c.tier, &c.model].into_iter().enumerate() {
                    if !name.is_empty() {
                        *self.counts[k].entry(name.clone()).or_default() += 1;
                    }
                }
                self.calls.push(c);
            }
        }
        if self.calls.len() > MAX_CALLS {
            self.calls.drain(..self.calls.len() - MAX_CALLS);
        }
        if self.calls.len() != before {
            self.version += 1;
        }
    }

    /// The providers (0), tiers (1) or models (2) seen, the busiest first, at most `cap`.
    fn facet(&self, kind: usize, cap: usize) -> Vec<String> {
        let mut v: Vec<(&String, &usize)> = self.counts[kind].iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        v.into_iter().take(cap).map(|(k, _)| k.clone()).collect()
    }
}

// ---------------------------------------------------------------- queries

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Range {
    H24,
    D7,
    D30,
    D90,
    All,
}

impl Range {
    const ALL: [(Range, &'static str); 5] = [(Range::H24, "24h"), (Range::D7, "7d"), (Range::D30, "30d"), (Range::D90, "90d"), (Range::All, "All")];

    /// How many local days a day range covers (today included).
    fn days(self) -> i64 {
        match self {
            Range::D7 => 7,
            Range::D30 => 30,
            Range::D90 => 90,
            Range::H24 | Range::All => 0,
        }
    }

    /// The first second the range takes in at `now`: the last 24 whole hours, or local midnight of the first day.
    pub(super) fn since(self, now: u64) -> u64 {
        match self {
            Range::H24 => (now / 3600).saturating_sub(23) * 3600,
            Range::All => 0,
            r => {
                let (_, h, m) = usage::local(now);
                // ponytail: 24-hour days back from midnight; a range with a DST change starts an hour off
                (now - u64::from(h) * 3600 - u64::from(m) * 60 - now % 60).saturating_sub((r.days() as u64 - 1) * 86_400)
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Group {
    Model,
    Provider,
    Tier,
    Day,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Metric {
    Tokens,
    Input,
    Output,
    CacheRead,
    Cost,
    Requests,
}

impl Metric {
    const ALL: [(Metric, &'static str); 6] = [
        (Metric::Tokens, "Tokens in + out"),
        (Metric::Input, "Input tokens"),
        (Metric::Output, "Output tokens"),
        (Metric::CacheRead, "Cache read"),
        (Metric::Cost, "Est. cost"),
        (Metric::Requests, "Requests"),
    ];

    fn name(self) -> &'static str {
        Self::ALL.iter().find(|(m, _)| *m == self).map_or("", |(_, n)| n)
    }

    fn fmt(self, v: f64) -> String {
        match self {
            Metric::Cost => usage::fmt_cost(v),
            _ => usage::fmt_tokens(v),
        }
    }
}

/// What the Analytics page shows: a range, filters ("" for all), the grouping and the measure.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct Query {
    pub range: Range,
    pub provider: String,
    pub tier: String,
    pub model: String,
    pub group: Group,
    pub metric: Metric,
}

impl Query {
    fn matches(&self, c: &Call) -> bool {
        let is = |want: &str, got: &str| want.is_empty() || want == got;
        is(&self.provider, &c.provider) && is(&self.tier, &c.tier) && is(&self.model, &c.model)
    }
}

/// Sums over some calls.
#[derive(Clone, Default, Debug, PartialEq)]
pub(super) struct Totals {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    ms_sum: u64,
    ms_n: u64,
}

impl Totals {
    fn add(&mut self, c: &Call) {
        self.requests += 1;
        self.input += c.input;
        self.output += c.output;
        self.cache_read += c.cache_read;
        self.cache_write += c.cache_write;
        self.cost += c.cost;
        if let Some(ms) = c.ms {
            self.ms_sum += ms;
            self.ms_n += 1;
        }
    }

    /// The share of the prompt served from the cache: cache read over everything the prompts held.
    pub(super) fn cache_hit(&self) -> Option<f64> {
        let prompt = self.input + self.cache_read + self.cache_write;
        (prompt > 0).then(|| self.cache_read as f64 / prompt as f64)
    }

    fn avg_output(&self) -> f64 {
        if self.requests == 0 { 0.0 } else { self.output as f64 / self.requests as f64 }
    }

    /// Mean request time in ms, over the requests whose line says.
    pub(super) fn avg_ms(&self) -> Option<f64> {
        (self.ms_n > 0).then(|| self.ms_sum as f64 / self.ms_n as f64)
    }

    fn value(&self, m: Metric) -> f64 {
        match m {
            Metric::Tokens => (self.input + self.output) as f64,
            Metric::Input => self.input as f64,
            Metric::Output => self.output as f64,
            Metric::CacheRead => self.cache_read as f64,
            Metric::Cost => self.cost,
            Metric::Requests => self.requests as f64,
        }
    }
}

fn value(c: &Call, m: Metric) -> f64 {
    match m {
        Metric::Tokens => (c.input + c.output) as f64,
        Metric::Input => c.input as f64,
        Metric::Output => c.output as f64,
        Metric::CacheRead => c.cache_read as f64,
        Metric::Cost => c.cost,
        Metric::Requests => 1.0,
    }
}

/// One group's line of the table: its name, its day number (for ordering days) and its sums.
#[derive(Debug)]
pub(super) struct Row {
    pub name: String,
    day: i64,
    pub totals: Totals,
}

/// The Analytics page's numbers for a query.
#[derive(Debug, Default)]
pub(super) struct Analysis {
    /// Hourly bars (the 24h range), else bars of `step` local days.
    pub hourly: bool,
    /// The first bar's start: unix seconds when hourly, else a day number.
    pub first: i64,
    pub step: i64,
    pub buckets: usize,
    /// The chart's layers (one `Total` when grouped by day) and, per layer, the metric in each bar.
    pub names: Vec<String>,
    pub cells: Vec<Vec<f64>>,
    /// The groups, biggest first (newest first for days).
    pub rows: Vec<Row>,
    pub total: Totals,
}

impl Analysis {
    fn label(&self, bar: usize) -> String {
        if self.hourly {
            let (_, h, _) = usage::local((self.first + bar as i64 * 3600) as u64);
            format!("{h:02}:00")
        } else {
            usage::day_label(self.first + bar as i64 * self.step)
        }
    }

    fn title(&self, bar: usize) -> String {
        if self.hourly {
            format!("{}, {}", usage::day_label(usage::local_day((self.first + bar as i64 * 3600) as u64)), self.label(bar))
        } else if self.step > 1 {
            format!("{} +{} days", self.label(bar), self.step)
        } else {
            self.label(bar)
        }
    }
}

/// Adds up `calls` as `q` says. Bars: the last 24 hours by the hour, or the range's local days (older ranges
/// cover at most 120 bars, several days each).
pub(super) fn analyse(calls: &[Call], q: &Query, now: u64) -> Analysis {
    let since = q.range.since(now);
    // A day number per call, asked of the OS once per quarter hour.
    let mut memo: HashMap<u64, i64> = HashMap::new();
    let mut day = |t: u64| *memo.entry(t / 900).or_insert_with(|| usage::local_day(t / 900 * 900));
    let today = day(now);
    let (hourly, first, step, buckets) = match q.range {
        Range::H24 => (true, since as i64, 3600, 24),
        Range::All => {
            let oldest = calls.iter().map(|c| c.t).filter(|t| *t <= now).min().map_or(today, &mut day);
            let n = (today - oldest + 1).max(1);
            let step = (n + 119) / 120;
            (false, oldest, step, ((n + step - 1) / step) as usize)
        }
        r => (false, today - r.days() + 1, 1, r.days() as usize),
    };
    let mut a = Analysis { hourly, first, step, buckets, ..Default::default() };
    let mut groups: HashMap<String, (i64, Totals, Vec<f64>)> = HashMap::new();
    for c in calls.iter().filter(|c| c.t >= since && q.matches(c)) {
        let d = day(c.t);
        let bar = if hourly { (c.t as i64 - first) / 3600 } else if d < first { -1 } else { (d - first) / step };
        if bar < 0 || bar as usize >= buckets {
            continue;
        }
        let key = match q.group {
            Group::Model => c.model.clone(),
            Group::Provider => c.provider.clone(),
            Group::Tier if c.tier.is_empty() => "—".to_string(),
            Group::Tier => c.tier.clone(),
            Group::Day => usage::day_label(d),
        };
        let g = groups.entry(key).or_insert_with(|| (d, Totals::default(), vec![0.0; buckets]));
        g.1.add(c);
        g.2[bar as usize] += value(c, q.metric);
        a.total.add(c);
    }
    a.rows = groups.iter().map(|(k, g)| Row { name: k.clone(), day: g.0, totals: g.1.clone() }).collect();
    if q.group == Group::Day {
        a.rows.sort_by(|x, y| y.day.cmp(&x.day));
        a.names = vec!["Total".into()];
        a.cells = vec![(0..buckets).map(|b| groups.values().map(|g| g.2[b]).sum()).collect()];
    } else {
        a.rows.sort_by(|x, y| y.totals.value(q.metric).total_cmp(&x.totals.value(q.metric)).then(x.name.cmp(&y.name)));
        a.names = a.rows.iter().map(|r| r.name.clone()).collect();
        a.cells = a.rows.iter().map(|r| groups[&r.name].2.clone()).collect();
    }
    a
}

/// What the History page shows: a range, a provider and a tier ("" for all), and words that must all be in a
/// call's model, provider, tier, account or stop reason.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct HQuery {
    pub range: Range,
    pub provider: String,
    pub tier: String,
    pub text: String,
}

/// The calls `q` lets through as indexes into `calls`, newest first (as the file has them: the proxy appends),
/// and their totals.
pub(super) fn history(calls: &[Call], q: &HQuery, now: u64) -> (Vec<usize>, Totals) {
    let since = q.range.since(now);
    let terms: Vec<String> = q.text.split_whitespace().map(str::to_lowercase).collect();
    let is = |want: &str, got: &str| want.is_empty() || want == got;
    let (mut found, mut totals) = (Vec::new(), Totals::default());
    for (i, c) in calls.iter().enumerate().rev() {
        if c.t < since || !is(&q.provider, &c.provider) || !is(&q.tier, &c.tier) {
            continue;
        }
        let fields = [&c.model, &c.provider, &c.tier, &c.acct, &c.stop].map(|f| f.to_lowercase());
        if terms.iter().all(|t| fields.iter().any(|f| f.contains(t.as_str()))) {
            found.push(i);
            totals.add(c);
        }
    }
    (found, totals)
}

// ---------------------------------------------------------------- formatting

/// 3412 → "3,412".
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 1234 → "1.2 s", 80 → "0.08 s"; "—" for a line that has no duration.
fn seconds(ms: Option<f64>) -> String {
    match ms {
        None => "—".into(),
        Some(ms) if ms < 1000.0 => format!("{:.2} s", ms / 1000.0),
        Some(ms) if ms < 100_000.0 => format!("{:.1} s", ms / 1000.0),
        Some(ms) => format!("{:.0} s", ms / 1000.0),
    }
}

/// "Oct 26 14:05:09" in local time.
fn when(t: u64) -> String {
    let (_, h, m) = usage::local(t);
    format!("{} {h:02}:{m:02}:{:02}", usage::day_label(usage::local_day(t)), t % 60)
}

fn percent(v: Option<f64>) -> String {
    v.map_or("—".into(), |v| format!("{:.1}%", v * 100.0))
}

// ---------------------------------------------------------------- tables

/// A table column: `w` wide, or the rest of the row when 0; its header; right-aligned numbers.
#[derive(Clone, Copy)]
struct Col {
    name: &'static str,
    w: f32,
    right: bool,
    /// Drawn in the quieter grey.
    quiet: bool,
}

const COL_GAP: f32 = 8.0;

/// Each column's (left, right) edge across a row `width` wide.
fn spans(width: f32, cols: &[Col]) -> Vec<(f32, f32)> {
    let fixed: f32 = cols.iter().map(|c| c.w).sum::<f32>() + COL_GAP * cols.len().saturating_sub(1) as f32;
    let rest = (width - fixed).max(40.0);
    let mut x = 0.0;
    cols.iter().map(|c| {
        let w = if c.w == 0.0 { rest } else { c.w };
        let at = (x, x + w);
        x += w + COL_GAP;
        at
    }).collect()
}

fn header(ui: &mut egui::Ui, cols: &[Col]) {
    let p = crate::pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    for (c, (x0, x1)) in cols.iter().zip(spans(rect.width(), cols)) {
        let (at, align) = if c.right { (rect.left() + x1, Align2::RIGHT_CENTER) } else { (rect.left() + x0, Align2::LEFT_CENTER) };
        ui.painter().text(egui::pos2(at, rect.center().y), align, c.name, FontId::proportional(11.5), p.subtle);
    }
    ui.painter().hline(rect.x_range(), rect.bottom(), egui::Stroke::new(1.0, p.hsep));
}

/// One table row: `cells` per column, and a colour chip before the first when `chip` says. Returns the row's response.
fn table_row(ui: &mut egui::Ui, cols: &[Col], cells: &[String], chip: Option<Color32>, sense: Sense) -> egui::Response {
    let p = crate::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), sense);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    if sense != Sense::hover() && resp.hovered() {
        ui.painter().rect_filled(rect.expand2(vec2(6.0, 0.0)), 5.0, p.hover.gamma_multiply(0.6));
    }
    for (i, ((c, (x0, x1)), text)) in cols.iter().zip(spans(rect.width(), cols)).zip(cells).enumerate() {
        let lead = if i == 0 && chip.is_some() { 16.0 } else { 0.0 };
        let cell = egui::Rect::from_min_max(egui::pos2(rect.left() + x0 + lead, rect.top()), egui::pos2(rect.left() + x1, rect.bottom()));
        let (at, align) = if c.right { (cell.right_center(), Align2::RIGHT_CENTER) } else { (cell.left_center(), Align2::LEFT_CENTER) };
        let color = if c.quiet || text == "—" { p.muted } else { p.fg };
        ui.painter().with_clip_rect(cell).text(at, align, text, FontId::proportional(12.5), color);
    }
    if let Some(color) = chip {
        ui.painter().rect_filled(egui::Rect::from_center_size(egui::pos2(rect.left() + 4.0, rect.center().y), vec2(8.0, 8.0)), 2.0, color);
    }
    resp
}

/// A row of up to four figures, each with a caption under it.
fn stats(ui: &mut egui::Ui, tiles: &[(String, &str)]) {
    let p = crate::pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::hover());
    let w = (rect.width() - 3.0 * COL_GAP) / 4.0;
    for (i, (v, caption)) in tiles.iter().enumerate() {
        let x = rect.left() + i as f32 * (w + COL_GAP);
        let cell = egui::Rect::from_min_size(egui::pos2(x, rect.top()), vec2(w, rect.height()));
        let pt = ui.painter().with_clip_rect(cell);
        pt.text(cell.left_top(), Align2::LEFT_TOP, v, FontId::proportional(20.0), p.fg);
        pt.text(cell.left_top() + vec2(0.0, 29.0), Align2::LEFT_TOP, caption, FontId::proportional(11.5), p.subtle);
    }
}

fn empty_note(ui: &mut egui::Ui, title: &str, hint: &str) {
    let p = crate::pal(ui);
    ui.add_space(6.0);
    ui.label(RichText::new(title).size(14.5).color(p.fg));
    ui.add_space(3.0);
    if !hint.is_empty() {
        ui.add(egui::Label::new(RichText::new(hint).size(12.5).color(p.subtle)).wrap());
    }
}

const NO_REQUESTS: &str = "No requests recorded yet";
const NO_REQUESTS_HINT: &str = "ccat logs every request that goes through its proxy: launches with a tier on another provider, or with a fallback. Plain claude sessions are not counted.";

/// `options` as the "All …" choice and then one per name.
fn choices(all: &str, names: Vec<String>) -> Vec<(String, String)> {
    std::iter::once((all.to_string(), String::new())).chain(names.into_iter().map(|n| (n.clone(), n))).collect()
}

/// Filters list the busiest names; the one picked stays listed even when it is not among them.
fn facet(log: &Log, kind: usize, cap: usize, current: &str) -> Vec<String> {
    let mut v = log.facet(kind, cap);
    if !current.is_empty() && !v.iter().any(|n| n == current) {
        v.push(current.to_string());
    }
    v
}

/// A left part and a right part of one toolbar row.
fn toolbar(ui: &mut egui::Ui, left: impl FnOnce(&mut egui::Ui), right: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 30.0), Layout::left_to_right(Align::Center), |ui| {
        left(ui);
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
}

// ---------------------------------------------------------------- the pages

pub(super) struct Insights {
    log: Log,
    aq: Query,
    /// Analytics' table shows every group, not just the first ones.
    all_rows: bool,
    analysis: Option<((u64, u64, Query), Analysis)>,
    hq: HQuery,
    /// How many History rows are shown, and the one opened (its index in `Log::calls` and its time).
    limit: usize,
    open: Option<(usize, u64)>,
    found: Option<((u64, u64, HQuery), (Vec<usize>, Totals))>,
}

impl Insights {
    pub(super) fn new() -> Self {
        Self {
            log: Log::new(),
            aq: Query { range: Range::D7, provider: String::new(), tier: String::new(), model: String::new(), group: Group::Model, metric: Metric::Tokens },
            all_rows: false,
            analysis: None,
            hq: HQuery { range: Range::D7, provider: String::new(), tier: String::new(), text: String::new() },
            limit: PAGE,
            open: None,
            found: None,
        }
    }

    pub(super) fn analytics(&mut self, ui: &mut egui::Ui) {
        self.log.poll();
        heading(ui, "Analytics");
        if self.log.calls.is_empty() {
            return empty_note(ui, NO_REQUESTS, NO_REQUESTS_HINT);
        }
        let providers = choices("All providers", facet(&self.log, 0, 12, &self.aq.provider));
        let tiers = choices("All tiers", facet(&self.log, 1, 12, &self.aq.tier));
        let models = choices("All models", facet(&self.log, 2, MODEL_CHOICES, &self.aq.model));
        let q = &mut self.aq;
        let before = q.clone();
        group(ui, |ui, g| {
            g.item(ui, |ui| {
                toolbar(
                    ui,
                    |ui| {
                        usage::segmented_lg(ui, &mut q.range, &Range::ALL);
                    },
                    |ui| {
                        let options: Vec<(String, Metric)> = Metric::ALL.iter().map(|(m, n)| (n.to_string(), *m)).collect();
                        if let Some(m) = select(ui, "an-metric", 150.0, &options, &q.metric) {
                            q.metric = m;
                        }
                    },
                );
            });
            g.item(ui, |ui| {
                let p = crate::pal(ui);
                toolbar(
                    ui,
                    |ui| {
                        ui.label(RichText::new("Group by").size(13.0).color(p.muted));
                        ui.add_space(4.0);
                        usage::segmented_lg(ui, &mut q.group, &[(Group::Model, "Model"), (Group::Provider, "Provider"), (Group::Tier, "Tier"), (Group::Day, "Day")]);
                    },
                    |_| {},
                );
            });
            g.item(ui, |ui| {
                let w = ((ui.available_width() - 2.0 * COL_GAP) / 3.0).floor();
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = COL_GAP;
                    if let Some(v) = select(ui, "an-provider", w, &providers, &q.provider) {
                        q.provider = v;
                    }
                    if let Some(v) = select(ui, "an-tier", w, &tiers, &q.tier) {
                        q.tier = v;
                    }
                    if let Some(v) = select(ui, "an-model", w, &models, &q.model) {
                        q.model = v;
                    }
                });
            });
        });
        if *q != before {
            self.all_rows = false;
        }
        let now = crate::now();
        let key = (self.log.version, now / 60, self.aq.clone());
        if self.analysis.as_ref().is_none_or(|(k, _)| *k != key) {
            let a = analyse(&self.log.calls, &self.aq, now);
            self.analysis = Some((key, a));
        }
        let a = &self.analysis.as_ref().unwrap().1;
        let (metric, group_by) = (self.aq.metric, self.aq.group);
        let p = crate::pal(ui);
        if a.total.requests == 0 {
            return empty_note(ui, "Nothing matches these filters in this range", "");
        }
        group(ui, |ui, g| {
            g.item(ui, |ui| {
                ui.label(RichText::new(format!("{} per {}", metric.name(), if a.hourly { "hour" } else if a.step > 1 { "period" } else { "day" })).size(14.0).color(p.fg));
                ui.add_space(8.0);
                let label = |b: usize| a.label(b);
                let title = |b: usize| a.title(b);
                let fmt = |v: f64| metric.fmt(v);
                let bars = Bars {
                    count: a.buckets,
                    names: &a.names,
                    value: &|b, l| a.cells[l][b],
                    fmt: &fmt,
                    label: &label,
                    title: &title,
                    every: if a.buckets <= 8 { 1 } else { (a.buckets + 5) / 6 },
                    first: 0,
                    color: &usage::hue,
                };
                usage::bar_chart(ui, 190.0, &bars);
            });
        });
        let t = &a.total;
        group(ui, |ui, g| {
            g.item(ui, |ui| {
                stats(ui, &[
                    (thousands(t.requests), "Requests"),
                    (usage::fmt_tokens(t.input as f64), "Input tokens (uncached)"),
                    (usage::fmt_tokens(t.output as f64), "Output tokens"),
                    (percent(t.cache_hit()), "Cache hit rate"),
                ]);
                ui.add_space(12.0);
                let mut second = vec![
                    (usage::fmt_cost(t.cost), "Est. cost (API-equivalent)"),
                    (usage::fmt_tokens(t.cache_read as f64), "Cache read tokens"),
                    (usage::fmt_tokens(t.avg_output()), "Avg output / request"),
                ];
                if let Some(ms) = t.avg_ms() {
                    second.push((seconds(Some(ms)), "Avg latency"));
                }
                stats(ui, &second);
            });
        });
        let wide = ui.available_width() > 520.0;
        let num = |name, w| Col { name, w, right: true, quiet: false };
        let mut cols = vec![Col { name: group_name(group_by), w: 0.0, right: false, quiet: false }, num("Requests", 62.0)];
        if wide {
            cols.extend([num("Input", 62.0), num("Output", 62.0), num("Cache read", 76.0)]);
        }
        cols.extend([num("Est. cost", 62.0), num("Share", 54.0)]);
        let shown = if self.all_rows { a.rows.len() } else { a.rows.len().min(10) };
        let sum = a.rows.iter().map(|r| r.totals.value(metric)).sum::<f64>();
        let mut more = self.all_rows;
        group(ui, |ui, g| {
            g.item(ui, |ui| {
                header(ui, &cols);
                for (i, r) in a.rows.iter().take(shown).enumerate() {
                    let t = &r.totals;
                    let mut cells = vec![r.name.clone(), thousands(t.requests)];
                    if wide {
                        cells.extend([usage::fmt_tokens(t.input as f64), usage::fmt_tokens(t.output as f64), usage::fmt_tokens(t.cache_read as f64)]);
                    }
                    cells.extend([usage::fmt_cost(t.cost), format!("{:.1}%", if sum > 0.0 { t.value(metric) / sum * 100.0 } else { 0.0 })]);
                    let chip = (group_by != Group::Day).then(|| usage::hue(i, p));
                    table_row(ui, &cols, &cells, chip, Sense::hover());
                }
                if a.rows.len() > 10 {
                    let text = if more { "Show fewer".to_string() } else { format!("Show {} more", a.rows.len() - 10) };
                    ui.add_space(4.0);
                    if ui.add(egui::Label::new(RichText::new(text).size(12.0).color(p.subtle)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        more = !more;
                    }
                }
            });
        });
        self.all_rows = more;
    }

    pub(super) fn history(&mut self, ui: &mut egui::Ui) {
        self.log.poll();
        heading(ui, "History");
        if self.log.calls.is_empty() {
            return empty_note(ui, NO_REQUESTS, NO_REQUESTS_HINT);
        }
        let providers = choices("All providers", facet(&self.log, 0, 12, &self.hq.provider));
        let tiers = choices("All tiers", facet(&self.log, 1, 12, &self.hq.tier));
        let q = &mut self.hq;
        let before = q.clone();
        group(ui, |ui, g| {
            g.item(ui, |ui| {
                ui.add(TextEdit::singleline(&mut q.text).hint_text("Search model, provider, tier, account").desired_width(f32::INFINITY).margin(egui::Margin::symmetric(10, 6)));
            });
            g.item(ui, |ui| {
                toolbar(
                    ui,
                    |ui| {
                        ui.spacing_mut().item_spacing.x = COL_GAP;
                        if let Some(v) = select(ui, "hi-provider", 140.0, &providers, &q.provider) {
                            q.provider = v;
                        }
                        if let Some(v) = select(ui, "hi-tier", 120.0, &tiers, &q.tier) {
                            q.tier = v;
                        }
                    },
                    |ui| {
                        usage::segmented_lg(ui, &mut q.range, &Range::ALL);
                    },
                );
            });
        });
        if *q != before {
            self.limit = PAGE;
            self.open = None;
        }
        let key = (self.log.version, crate::now() / 60, self.hq.clone());
        if self.found.as_ref().is_none_or(|(k, _)| *k != key) {
            let f = history(&self.log.calls, &self.hq, crate::now());
            self.found = Some((key, f));
        }
        let (rows, totals) = &self.found.as_ref().unwrap().1;
        let p = crate::pal(ui);
        if rows.is_empty() {
            return empty_note(ui, "Nothing matches these filters in this range", "");
        }
        // The columns that fit: the ones on the left always, the rest while the model keeps a readable width.
        let avail = ui.available_width() - 36.0;
        let col = |name, w, right, quiet| Col { name, w, right, quiet };
        let mut cols: Vec<(usize, Col)> = vec![
            (0, col("Time", 106.0, false, true)),
            (3, col("Model", 0.0, false, false)),
            (4, col("Input", 54.0, true, false)),
            (5, col("Output", 54.0, true, false)),
            (6, col("Cache", 54.0, true, false)),
            (7, col("Took", 50.0, true, false)),
        ];
        for extra in [(1, col("Provider", 84.0, false, true)), (2, col("Tier", 62.0, false, true)), (8, col("Stop", 74.0, false, true)), (9, col("Account", 80.0, false, true))] {
            let used: f32 = cols.iter().map(|(_, c)| c.w + COL_GAP).sum::<f32>() + extra.1.w + COL_GAP;
            if avail - used >= 110.0 {
                cols.push(extra);
            }
        }
        cols.sort_by_key(|(id, _)| *id);
        let layout: Vec<Col> = cols.iter().map(|(_, c)| *c).collect();
        let (limit, calls) = (self.limit, &self.log.calls);
        let mut open = self.open;
        let mut more = false;
        group(ui, |ui, g| {
            g.item(ui, |ui| {
                header(ui, &layout);
                for &i in rows.iter().take(limit) {
                    let c = &calls[i];
                    let cells: Vec<String> = cols.iter().map(|(id, _)| match id {
                        0 => when(c.t),
                        1 => c.provider.clone(),
                        2 => c.tier.clone(),
                        3 => c.model.clone(),
                        4 => usage::fmt_tokens(c.input as f64),
                        5 => usage::fmt_tokens(c.output as f64),
                        6 => usage::fmt_tokens(c.cache_read as f64),
                        7 => seconds(c.ms.map(|m| m as f64)),
                        8 => if c.stop.is_empty() { "—".into() } else { c.stop.replace('_', " ") },
                        _ => if c.acct.is_empty() { "—".into() } else { c.acct.clone() },
                    }).collect();
                    let shown = open == Some((i, c.t));
                    if table_row(ui, &layout, &cells, None, Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        open = if shown { None } else { Some((i, c.t)) };
                    }
                    if shown {
                        details(ui, c);
                    }
                }
                ui.add_space(8.0);
                ui.painter().hline(ui.available_rect_before_wrap().x_range(), ui.cursor().top(), egui::Stroke::new(1.0, p.hsep));
                ui.add_space(8.0);
                toolbar(
                    ui,
                    |ui| {
                        let t = totals;
                        let text = format!(
                            "{} of {} requests · in {} · out {} · cache read {} · est. {}",
                            thousands(rows.len().min(limit) as u64),
                            thousands(rows.len() as u64),
                            usage::fmt_tokens(t.input as f64),
                            usage::fmt_tokens(t.output as f64),
                            usage::fmt_tokens(t.cache_read as f64),
                            usage::fmt_cost(t.cost),
                        );
                        ui.add(egui::Label::new(RichText::new(text).size(12.0).color(p.subtle)).truncate());
                    },
                    |ui| {
                        if rows.len() > limit && button(ui, &format!("Load {} more", PAGE.min(rows.len() - limit))).clicked() {
                            more = true;
                        }
                    },
                );
            });
        });
        self.open = open;
        if more {
            self.limit += PAGE;
        }
    }
}

fn group_name(g: Group) -> &'static str {
    match g {
        Group::Model => "Model",
        Group::Provider => "Provider",
        Group::Tier => "Tier",
        Group::Day => "Day",
    }
}

/// The opened row's full line: every field the table may have left out.
fn details(ui: &mut egui::Ui, c: &Call) {
    let p = crate::pal(ui);
    let or = |s: &str| if s.is_empty() { "—".to_string() } else { s.to_string() };
    egui::Frame::new().fill(p.field).corner_radius(egui::CornerRadius::same(8)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 3.0;
        let line = |ui: &mut egui::Ui, text: String| {
            ui.add(egui::Label::new(RichText::new(text).size(12.0).color(p.muted)).wrap());
        };
        line(ui, format!("{} · {} · tier {} · account {} · stopped: {}", c.model, c.provider, or(&c.tier), or(&c.acct), or(&c.stop.replace('_', " "))));
        line(ui, format!(
            "input {} · output {} · cache read {} · cache write {} · took {} · est. {}",
            thousands(c.input),
            thousands(c.output),
            thousands(c.cache_read),
            thousands(c.cache_write),
            seconds(c.ms.map(|m| m as f64)),
            usage::fmt_cost(c.cost)
        ));
    });
    ui.add_space(4.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 06:30 UTC: no time zone has its midnight within minutes of it, so calls just before it share its day.
    const NOW: u64 = 1_792_564_200;

    fn call(ago_s: u64, provider: &str, model: &str, input: u64, output: u64, cache_read: u64) -> Call {
        Call { t: NOW - ago_s, tier: "opus".into(), provider: provider.into(), model: model.into(), input, output, cache_read, cost: 0.5, ..Default::default() }
    }

    fn query(range: Range, group: Group, metric: Metric) -> Query {
        Query { range, provider: String::new(), tier: String::new(), model: String::new(), group, metric }
    }

    #[test]
    fn old_and_new_lines_both_parse_and_unknown_fields_are_ignored() {
        let pricing = Pricing::default();
        let old = json!({ "t": 10, "tier": "opus", "provider": "Claude", "model": "claude-opus-5-5", "input": 5, "output": 7, "cache_read": 100, "cache_write": 1 });
        let c = Call::parse(&old, &pricing).unwrap();
        assert_eq!((c.ms, c.stop.as_str(), c.acct.as_str(), c.input, c.cache_read), (None, "", "", 5, 100), "no duration, stop or account");
        assert_eq!(c.model, "Claude Opus 5.5");
        let new = json!({ "t": 11, "provider": "ChatGPT", "model": "gpt-6-astra", "input": 1, "output": 2, "ms": 1500, "pid": 4242, "stop": "tool_use", "acct": "work", "future": [1, 2] });
        let c = Call::parse(&new, &pricing).unwrap();
        assert_eq!((c.ms, c.stop.as_str(), c.acct.as_str(), c.tier.as_str()), (Some(1500), "tool_use", "work", ""));
        assert!(Call::parse(&json!({ "provider": "x" }), &pricing).is_none(), "a line without a time is no call");
    }

    #[test]
    fn ranges_start_where_they_say() {
        assert_eq!(Range::H24.since(NOW) % 3600, 0);
        assert!(NOW - Range::H24.since(NOW) >= 23 * 3600 && NOW - Range::H24.since(NOW) < 24 * 3600);
        assert_eq!(Range::All.since(NOW), 0);
        let age = |r: Range| NOW - r.since(NOW);
        assert!(age(Range::D7) >= 6 * 86_400 && age(Range::D7) < 7 * 86_400);
        assert!(age(Range::D7) < age(Range::D30) && age(Range::D30) < age(Range::D90));
    }

    #[test]
    fn analysing_filters_groups_and_adds_up() {
        let calls = vec![
            call(100, "Claude", "Claude Opus 5.5", 10, 20, 970), // within the hour
            call(200, "ChatGPT", "GPT 6 Astra", 30, 40, 0),
            call(3 * 86_400, "Claude", "Claude Opus 5.5", 1, 2, 3),
            call(40 * 86_400, "Claude", "Claude Opus 5.5", 100, 100, 0), // outside 7d and 30d
        ];
        let a = analyse(&calls, &query(Range::D7, Group::Provider, Metric::Tokens), NOW);
        assert_eq!((a.total.requests, a.buckets, a.hourly), (3, 7, false));
        assert_eq!(a.names, ["ChatGPT", "Claude"], "biggest first: ChatGPT 70 tokens, Claude 33");
    }

    #[test]
    fn groups_are_ordered_by_the_metric_and_the_bars_hold_the_same_sum() {
        let calls = vec![call(100, "Claude", "A", 10, 20, 0), call(200, "ChatGPT", "B", 30, 40, 0), call(300, "ChatGPT", "B", 1, 1, 0)];
        let a = analyse(&calls, &query(Range::D7, Group::Provider, Metric::Tokens), NOW);
        assert_eq!(a.names, ["ChatGPT", "Claude"]);
        let bars: f64 = a.cells.iter().flatten().sum();
        assert_eq!(bars, 102.0, "tokens in + out");
        assert_eq!(a.rows[0].totals.requests, 2);
        let a = analyse(&calls, &query(Range::D7, Group::Model, Metric::Requests), NOW);
        assert_eq!((a.names[0].as_str(), a.cells[0].iter().sum::<f64>()), ("B", 2.0));
        let a = analyse(&calls, &query(Range::D7, Group::Tier, Metric::Cost), NOW);
        assert_eq!((a.names.len(), a.cells[0].iter().sum::<f64>()), (1, 1.5), "one tier; cost is summed");
        let a = analyse(&calls, &query(Range::D7, Group::Day, Metric::Output), NOW);
        assert_eq!((a.names.len(), a.rows.len(), a.cells[0].iter().sum::<f64>()), (1, 1, 61.0), "by day: one chart layer, a row per day");
    }

    #[test]
    fn filters_narrow_the_calls() {
        let calls = vec![call(100, "Claude", "A", 1, 1, 0), call(100, "ChatGPT", "B", 1, 1, 0), call(100, "ChatGPT", "A", 1, 1, 0)];
        let mut q = query(Range::D7, Group::Model, Metric::Requests);
        q.provider = "ChatGPT".into();
        assert_eq!(analyse(&calls, &q, NOW).total.requests, 2);
        q.model = "A".into();
        assert_eq!(analyse(&calls, &q, NOW).total.requests, 1);
        q.tier = "haiku".into();
        assert_eq!(analyse(&calls, &q, NOW).total.requests, 0);
    }

    #[test]
    fn the_last_day_is_hourly_and_all_covers_the_oldest_call() {
        let calls = vec![call(100, "Claude", "A", 1, 1, 0), call(2 * 3600 + 100, "Claude", "A", 1, 1, 0), call(30 * 3600, "Claude", "A", 1, 1, 0)];
        let newest = |a: &Analysis| a.cells[0].iter().rposition(|v| *v > 0.0);
        let a = analyse(&calls, &query(Range::H24, Group::Provider, Metric::Requests), NOW);
        assert_eq!((a.hourly, a.buckets, a.total.requests), (true, 24, 2), "the one 30 hours old is left out");
        assert_eq!((newest(&a), a.cells[0][21]), (Some(23), 1.0), "the newest is in the last bar, the other two hours before");
        let a = analyse(&calls, &query(Range::All, Group::Provider, Metric::Requests), NOW);
        assert!(!a.hourly && a.total.requests == 3 && (2..=3).contains(&a.buckets), "today and the day or two before");
        let old = vec![call(400 * 86_400, "Claude", "A", 1, 1, 0)];
        let a = analyse(&old, &query(Range::All, Group::Provider, Metric::Requests), NOW);
        assert!(a.step >= 4 && a.buckets <= 120, "a long history is cut into at most 120 bars");
        assert_eq!(a.cells[0].iter().sum::<f64>(), 1.0);
    }

    #[test]
    fn the_cache_hit_rate_is_cache_read_over_the_whole_prompt() {
        let mut t = Totals::default();
        assert_eq!((t.cache_hit(), t.avg_ms()), (None, None), "nothing yet");
        t.add(&Call { input: 10, cache_read: 80, cache_write: 10, output: 5, ms: Some(1000), ..Default::default() });
        t.add(&Call { input: 100, output: 15, ..Default::default() });
        assert_eq!(t.cache_hit(), Some(80.0 / 200.0));
        assert_eq!((t.avg_output(), t.avg_ms()), (10.0, Some(1000.0)), "old lines without ms don't drag the average");
        assert_eq!(percent(Some(0.4)), "40.0%");
    }

    #[test]
    fn history_is_newest_first_filtered_and_totalled() {
        let mut calls: Vec<Call> = (0..5).map(|i| call(1000 - i * 10, "Claude", "Claude Opus 5.5", 1, 1, 0)).collect();
        calls.push(Call { acct: "work".into(), stop: "tool_use".into(), ..call(5, "ChatGPT", "GPT 6 Astra", 7, 3, 0) });
        calls.push(call(40 * 86_400, "Claude", "Claude Opus 5.5", 1, 1, 0));
        let q = |text: &str| HQuery { range: Range::D7, provider: String::new(), tier: String::new(), text: text.into() };
        let (rows, totals) = history(&calls, &q(""), NOW);
        assert_eq!(rows, [5, 4, 3, 2, 1, 0], "newest first; the 40-day-old one is out of range");
        assert_eq!((totals.requests, totals.input), (6, 12));
        assert_eq!(history(&calls, &q("astra"), NOW).0, [5], "the model");
        assert_eq!(history(&calls, &q("WORK tool"), NOW).0, [5], "every word, in any field, whatever the case");
        assert!(history(&calls, &q("haiku astra"), NOW).0.is_empty(), "all words must match one call");
        let by_provider = HQuery { provider: "Claude".into(), ..q("") };
        assert_eq!(history(&calls, &by_provider, NOW).0.len(), 5);
        let all = HQuery { range: Range::All, ..q("") };
        assert_eq!(history(&calls, &all, NOW).0.len(), 7);
        let (page, _) = history(&calls, &q(""), NOW);
        assert_eq!(page.iter().take(2).copied().collect::<Vec<_>>(), [5, 4], "a page is the first rows");
    }

    #[test]
    fn the_log_reads_the_tail_once_and_then_only_what_is_appended() {
        let dir = std::env::temp_dir().join(format!("ccat-insights-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tokens.jsonl");
        let line = |t: u64| format!("{}\n", json!({ "t": t, "provider": "Claude", "model": "m", "input": 1, "output": 1 }));
        fs::write(&path, format!("{}{}{{\"t\": 3, \"prov", line(1), line(2))).unwrap(); // the last line is mid-write
        let mut log = Log::with(path.clone(), Pricing::default());
        log.refresh();
        assert_eq!(log.calls.iter().map(|c| c.t).collect::<Vec<_>>(), [1, 2], "a half-written line waits");
        let v = log.version;
        log.refresh();
        assert_eq!(log.version, v, "nothing new: nothing changes");
        fs::write(&path, format!("{}{}{}{}", line(1), line(2), line(3), line(4))).unwrap();
        log.refresh();
        assert_eq!(log.calls.iter().map(|c| c.t).collect::<Vec<_>>(), [1, 2, 3, 4], "the finished line and the new one come in once each");
        assert_eq!(log.facet(0, 5), ["Claude"]);
        fs::write(&path, line(9)).unwrap(); // cleared by hand
        log.refresh();
        assert_eq!(log.calls.iter().map(|c| c.t).collect::<Vec<_>>(), [9], "a file that shrank is read again");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn columns_share_a_row_and_numbers_format() {
        let col = |w| Col { name: "", w, right: false, quiet: false };
        let s = spans(300.0, &[col(0.0), col(50.0), col(60.0)]);
        assert_eq!(s[1], (s[0].1 + COL_GAP, s[0].1 + COL_GAP + 50.0));
        assert_eq!(s[2].1, 300.0, "the flexible column takes what the others leave");
        assert_eq!((thousands(0), thousands(999), thousands(3412), thousands(1_234_567)), ("0".into(), "999".into(), "3,412".into(), "1,234,567".into()));
        assert_eq!((seconds(None).as_str(), seconds(Some(80.0)).as_str(), seconds(Some(1234.0)).as_str(), seconds(Some(250_000.0)).as_str()), ("—", "0.08 s", "1.2 s", "250 s"));
    }
}
