//! The Dashboard page: the usage popup's Live chart, roomy. One area per model (or provider) over a time range from
//! the last hour to all time, a running list of totals and shares under it, and a few figures. The range gives
//! (first bucket, bucket seconds, buckets) and everything else follows (`Win::span`); the series are the popup's own
//! (`usage::live_series`), so both count the same.

use super::*;
use usage::{Area, XMode};

type Span = (u64, u64, usize);

/// The page repaints this often while it is shown, so new requests appear (the log itself is polled every 3 s).
const REFRESH: Duration = Duration::from_secs(2);
const CHART_H: f32 = 260.0;
/// All time never has more buckets than this, so the chart stays quick however old the log is.
const MAX_BUCKETS: u64 = 120;
/// The sizes All time picks its bucket from (seconds); the smallest that fits is used.
const LADDER: [u64; 12] = [60, 300, 900, 1800, 3600, 7200, 14_400, 43_200, 86_400, 172_800, 604_800, 2_592_000];
/// The series list shows this many names until asked for more.
const LIST: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Win {
    Hour,
    Today,
    H24,
    Week,
    D7,
    D31,
    All,
}

impl Win {
    const ALL: [(Win, &'static str); 7] = [
        (Win::Hour, "Last hour"),
        (Win::Today, "Today"),
        (Win::H24, "Last 24 hours"),
        (Win::Week, "This week"),
        (Win::D7, "Last 7 days"),
        (Win::D31, "Last 31 days"),
        (Win::All, "All time"),
    ];

    /// The range at `now` as (first bucket's start, bucket seconds, buckets): the last hour by the minute, a day by
    /// 15 minutes, a week by the hour, 31 days by the day, and all time from the day of the `oldest` request in at
    /// most `MAX_BUCKETS` buckets. Today and This week are the popup's own.
    fn span(self, now: u64, oldest: Option<u64>) -> Span {
        match self {
            Win::Hour => usage::range_span(0, now),
            Win::Today => usage::range_span(1, now),
            Win::Week => usage::range_span(2, now),
            Win::H24 => usage::trailing(now, 900, 96),
            Win::D7 => usage::trailing(now, 3600, 168),
            // ponytail: 24-hour days back from midnight; a month with a DST change is an hour off
            Win::D31 => (usage::midnight(now).saturating_sub(30 * 86_400), 86_400, 31),
            Win::All => all_time(now, oldest),
        }
    }

    fn mode(self) -> XMode {
        match self {
            Win::Hour | Win::H24 | Win::D7 => XMode::Slide,
            Win::Today => XMode::Full,
            Win::Week | Win::D31 | Win::All => XMode::Fit,
        }
    }
}

/// From the first request's day (or hour, when the log is young) to now, in at most `MAX_BUCKETS` buckets of a round
/// size. A log without a request (or only ones from the future) gets the last hour.
fn all_time(now: u64, oldest: Option<u64>) -> Span {
    let Some(oldest) = oldest.filter(|o| *o <= now) else { return usage::range_span(0, now) };
    // Whole days start at local midnight; shorter buckets at a multiple of their size.
    let fit = |w: u64| {
        let start = if w >= 86_400 { usage::midnight(oldest) } else { oldest / w * w };
        (start, w, ((now - start) / w + 1) as usize)
    };
    LADDER.iter().copied().chain((1..).map(|k| k * 31_536_000)).map(fit).find(|s| s.2 as u64 <= MAX_BUCKETS).unwrap_or_else(|| fit(31_536_000))
}

/// The oldest request that is not from the future.
fn oldest(calls: &[Call], now: u64) -> Option<u64> {
    calls.iter().map(|c| c.t).filter(|t| *t <= now).min()
}

/// What the chart shows: the series and the sums of the range.
#[derive(Default, Debug)]
struct View {
    names: Vec<String>,
    series: Vec<Vec<f64>>,
    tally: Tally,
}

/// Calls, tokens moved (cache included, as the chart counts them) and estimated cost of a range.
#[derive(Default, Debug, PartialEq)]
struct Tally {
    calls: u64,
    tokens: u64,
    cost: f64,
}

fn tally(calls: &[Call], span: Span) -> Tally {
    let (start, w, n) = span;
    let mut t = Tally::default();
    for c in calls.iter().filter(|c| c.t >= start && c.t < start + w * n as u64) {
        t.calls += 1;
        t.tokens += c.tokens();
        t.cost += c.cost;
    }
    t
}

/// `by`: 1 per model, 2 per provider; `metric`: 0 tokens, 1 calls, 2 cost (as in `usage::live_series`).
fn view(calls: &[Call], span: Span, by: u8, metric: u8, cumulative: bool) -> View {
    let (names, series) = usage::live_series(calls, span, by, metric, cumulative);
    View { names, series, tally: tally(calls, span) }
}

/// Requests in the last hour, whatever the range.
fn last_hour(calls: &[Call], now: u64) -> usize {
    calls.iter().rev().take_while(|c| c.t + 3600 >= now).count()
}

/// Per name: its total (the running total's end when cumulative) and its share of all of them.
fn shares(series: &[Vec<f64>], cur: usize, cumulative: bool) -> Vec<(f64, f64)> {
    let totals: Vec<f64> = series.iter().map(|s| if cumulative { s[cur] } else { s.iter().sum() }).collect();
    let sum: f64 = totals.iter().sum();
    totals.into_iter().map(|t| (t, if sum > 0.0 { t / sum } else { 0.0 })).collect()
}

/// The time marks under the plot, as (position 0 to 1, text): five of them, "now" at the right of a sliding chart.
fn marks(mode: XMode, span: Span, now: u64) -> Vec<(f32, String)> {
    let (start, w, n) = span;
    let (cur, total) = (usage::current_bucket(span, now), w * n as u64);
    (0..=4)
        .map(|i| {
            let f = i as f64 / 4.0;
            let t = match mode {
                XMode::Slide => now.saturating_sub(((1.0 - f) * total as f64) as u64),
                XMode::Fit => start + (f * cur as f64 * w as f64) as u64,
                XMode::Full => start + (f * total as f64) as u64,
            };
            let text = if mode == XMode::Slide && i == 4 {
                "now".to_string()
            } else if total <= 2 * 86_400 {
                let (_, h, m) = usage::local(t);
                format!("{h:02}:{m:02}")
            } else {
                usage::day_label(usage::local_day(t))
            };
            (f as f32, text)
        })
        .collect()
}

/// The hover heading of the bucket of `w` seconds starting at `at`: its day, or its hours.
fn when(at: u64, w: u64) -> String {
    let day = |t: u64| usage::day_label(usage::local_day(t));
    let clock = |t: u64| {
        let (_, h, m) = usage::local(t);
        format!("{h:02}:{m:02}")
    };
    match w {
        86_400 => day(at),
        w if w > 86_400 => format!("{} – {}", day(at), day(at + w - 1)),
        w => format!("{} {}–{}", day(at), clock(at), clock(at + w)),
    }
}

/// The page's options and what it built last.
pub(super) struct Dash {
    win: Win,
    /// 1 per model, 2 per provider.
    by: u8,
    /// 0 tokens, 1 calls, 2 cost.
    metric: u8,
    cumulative: bool,
    more: bool,
    /// The oldest request, found again when the log changes: (log version, time).
    oldest: Option<(u64, Option<u64>)>,
    /// The last view with what it was built from: the log's version, the range, and the options.
    built: Option<((u64, Span, u8, u8, bool), View)>,
}

impl Default for Dash {
    fn default() -> Self {
        Self { win: Win::D7, by: 1, metric: 0, cumulative: false, more: false, oldest: None, built: None }
    }
}

impl Insights {
    pub(in crate::settings) fn dashboard(&mut self, ui: &mut egui::Ui) {
        self.log.poll();
        heading(ui, "Dashboard");
        if self.log.calls.is_empty() {
            return empty_note(ui, NO_REQUESTS, NO_REQUESTS_HINT);
        }
        ui.ctx().request_repaint_after(REFRESH);
        let Insights { log, dash: d, .. } = self;
        let ranges: Vec<(String, Win)> = Win::ALL.iter().map(|(w, n)| (n.to_string(), *w)).collect();
        let before = (d.win, d.by, d.metric, d.cumulative);
        group(ui, |ui, g| {
            g.item(ui, |ui| {
                toolbar(
                    ui,
                    |ui| {
                        if let Some(w) = select(ui, "dash-range", 170.0, &ranges, &d.win) {
                            d.win = w;
                        }
                    },
                    |ui| {
                        usage::segmented_lg(ui, &mut d.metric, &[(0u8, "Tokens"), (1, "Calls"), (2, "Cost")]);
                    },
                );
            });
            g.item(ui, |ui| {
                let p = crate::pal(ui);
                toolbar(
                    ui,
                    |ui| {
                        ui.label(RichText::new("Show by").size(13.0).color(p.muted));
                        ui.add_space(4.0);
                        usage::segmented_lg(ui, &mut d.by, &[(1u8, "Model"), (2, "Provider")]);
                    },
                    |ui| {
                        crate::switch(ui, &mut d.cumulative, "Cumulative");
                    },
                );
            });
        });
        if (d.win, d.by, d.metric, d.cumulative) != before {
            d.more = false;
        }
        let exact = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |t| t.as_secs_f64());
        let now = exact as u64;
        if d.oldest.is_none_or(|(v, _)| v != log.version) {
            d.oldest = Some((log.version, oldest(&log.calls, now)));
        }
        let span = d.win.span(now, d.oldest.and_then(|o| o.1));
        let key = (log.version, span, d.by, d.metric, d.cumulative);
        if d.built.as_ref().is_none_or(|(k, _)| *k != key) {
            d.built = Some((key, view(&log.calls, span, d.by, d.metric, d.cumulative)));
        }
        let v = &d.built.as_ref().unwrap().1;
        let (by, metric, cumulative, win) = (d.by, d.metric, d.cumulative, d.win);
        let p = crate::pal(ui);
        let cur = usage::current_bucket(span, now);
        let is_running = |name: &str| usage::running(&log.calls, now, by, name);
        let at = |t: u64| when(t, span.1);
        let marks = marks(win.mode(), span, now);
        let area = Area {
            names: &v.names,
            series: &v.series,
            span,
            mode: win.mode(),
            metric,
            cumulative,
            height: CHART_H,
            marks: &marks,
            when: &at,
            running: &is_running,
            id: egui::Id::new(("dash-top", win, by, metric, cumulative)),
            pulse: false,
        };
        let what = ["Tokens", "Calls", "Estimated cost"][metric.min(2) as usize];
        group(ui, |ui, g| {
            g.item(ui, |ui| {
                let title = format!("{what} by {}{}", if by == 2 { "provider" } else { "model" }, if cumulative { ", running total" } else { "" });
                ui.label(RichText::new(title).size(14.0).color(p.fg));
                ui.add_space(8.0);
                usage::area_chart(ui, exact, &area);
            });
        });
        if v.names.is_empty() {
            return empty_note(ui, "Nothing in this range yet", "");
        }
        group(ui, |ui, g| {
            g.item(ui, |ui| {
                stats(ui, &[
                    (thousands(v.tally.calls), "Calls"),
                    (usage::fmt_tokens(v.tally.tokens as f64), "Tokens"),
                    (usage::fmt_cost(v.tally.cost), "Est. cost"),
                    (thousands(last_hour(&log.calls, now) as u64), "Calls in the last hour"),
                ]);
            });
        });
        let fmt = |x: f64| match metric {
            0 => usage::fmt_tokens(x),
            1 => thousands(x as u64),
            _ => usage::fmt_cost(x),
        };
        let col = |name, w, right| Col { name, w, right, quiet: false };
        let cols = [col(if by == 2 { "Provider" } else { "Model" }, 0.0, false), col("", 62.0, true), col(["Tokens", "Calls", "Cost"][metric.min(2) as usize], 84.0, true), col("Share", 54.0, true)];
        let rows = shares(&v.series, cur, cumulative);
        let shown = if d.more { v.names.len() } else { v.names.len().min(LIST) };
        let mut more = d.more;
        group(ui, |ui, g| {
            g.item(ui, |ui| {
                header(ui, &cols);
                for (i, name) in v.names.iter().take(shown).enumerate() {
                    let (total, share) = rows[i];
                    let cells = [name.clone(), String::new(), fmt(total), format!("{:.1}%", share * 100.0)];
                    let resp = table_row(ui, &cols, &cells, Some(usage::hue(i, p)), Sense::hover());
                    if is_running(name) {
                        let (_, x1) = spans(resp.rect.width(), &cols)[1];
                        ui.painter().text(egui::pos2(resp.rect.left() + x1, resp.rect.center().y), Align2::RIGHT_CENTER, "running", FontId::proportional(11.5), p.blue);
                    }
                }
                if v.names.len() > LIST {
                    let text = if more { "Show fewer".to_string() } else { format!("Show {} more", v.names.len() - LIST) };
                    ui.add_space(4.0);
                    if ui.add(egui::Label::new(RichText::new(text).size(12.0).color(p.subtle)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        more = !more;
                    }
                }
            });
        });
        d.more = more;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 06:30 UTC (as in the parent's tests): no time zone has its midnight within minutes of it.
    const NOW: u64 = 1_792_564_200;

    fn call(ago_s: u64, provider: &str, model: &str, tokens: u64) -> Call {
        Call { t: NOW - ago_s, provider: provider.into(), model: model.into(), input: tokens, cost: 0.5, ..Default::default() }
    }

    #[test]
    fn every_range_is_a_whole_window_ending_now() {
        let oldest = Some(NOW - 400 * 86_400);
        for (win, _) in Win::ALL {
            let (start, w, n) = win.span(NOW, oldest);
            assert!(n >= 1 && w >= 60, "{win:?}");
            assert!(start <= NOW && NOW < start + w * n as u64, "{win:?}: now falls inside the last bucket (or the day)");
            assert!(usage::current_bucket((start, w, n), NOW) < n);
        }
        assert_eq!(Win::Hour.span(NOW, None), (usage::trailing(NOW, 60, 60).0, 60, 60), "minutes for the hour");
        assert_eq!(Win::H24.span(NOW, None).1 * Win::H24.span(NOW, None).2 as u64, 86_400, "24 hours");
        assert_eq!(Win::D7.span(NOW, None), (usage::trailing(NOW, 3600, 168).0, 3600, 168), "hours for 7 days");
        assert_eq!(Win::Today.span(NOW, None), usage::range_span(1, NOW), "the popup's own Today");
        assert_eq!(Win::Week.span(NOW, None), usage::range_span(2, NOW), "…and This week");
        let (start, w, n) = Win::D31.span(NOW, None);
        assert_eq!((w, n), (86_400, 31), "31 daily buckets");
        assert_eq!(start % 60, 0);
        assert!(NOW - start >= 30 * 86_400 && NOW - start < 31 * 86_400 + 3600, "from 30 days before today's midnight");
    }

    #[test]
    fn all_time_fits_the_log_in_a_capped_number_of_buckets() {
        // Empty (or only the future): the last hour, which the page never shows (it has its empty state first).
        assert_eq!(Win::All.span(NOW, None), usage::range_span(0, NOW));
        assert_eq!(Win::All.span(NOW, Some(NOW + 5)), usage::range_span(0, NOW), "a request from the future is no history");
        // One request just now: one or two minute buckets, holding it.
        let (start, w, n) = Win::All.span(NOW, Some(NOW - 3));
        assert!(w == 60 && (1..=2).contains(&n) && start <= NOW - 3, "{start} {w} {n}");
        // One from yesterday, a month ago, a year, decades: always at most 120 buckets, the oldest in the first.
        for days in [0, 1, 2, 30, 31, 365, 2000, 20_000] {
            let o = NOW - days * 86_400 - 1000;
            let (start, w, n) = Win::All.span(NOW, Some(o));
            assert!(n as u64 <= MAX_BUCKETS && n >= 1, "{days} days: {n} buckets of {w}");
            assert!(start <= o && o < start + w, "{days} days: the oldest is in the first bucket");
            assert!(NOW < start + w * n as u64, "{days} days: now is inside");
        }
        let (_, w, n) = Win::All.span(NOW, Some(NOW - 7 * 86_400));
        assert!(w < 86_400 || n == 8 || n == 9, "a week of log keeps hours: {w} {n}");
        assert_eq!(Win::All.span(NOW, Some(NOW - 90 * 86_400)).1, 86_400 * 1, "90 days is still one day each: 91 buckets");
        let (_, w, n) = Win::All.span(NOW, Some(NOW - 400 * 86_400));
        assert_eq!((w, n <= 120), (604_800, true), "400 days: weeks");
        // The corrupt zero time: a year per bucket at most, still capped.
        assert!(Win::All.span(NOW, Some(1)).2 as u64 <= MAX_BUCKETS);
    }

    #[test]
    fn the_oldest_request_ignores_the_future() {
        assert_eq!(oldest(&[], NOW), None);
        let calls = vec![call(10, "Claude", "A", 1), call(500, "Claude", "A", 1), Call { t: NOW + 100, ..Default::default() }];
        assert_eq!(oldest(&calls, NOW), Some(NOW - 500));
        assert_eq!(oldest(&calls[2..], NOW), None);
    }

    #[test]
    fn series_are_built_per_model_or_provider_and_summed() {
        let calls = vec![call(30, "Claude", "Opus", 10), call(90, "Claude", "Opus", 20), call(150, "ChatGPT", "GPT", 100), call(2 * 3600, "Claude", "Old", 7)];
        let span = Win::Hour.span(NOW, None);
        let v = view(&calls, span, 1, 0, false);
        assert_eq!(v.names, ["GPT", "Opus"], "biggest first; the one 2 hours old is out of the hour");
        assert_eq!(v.series[0].iter().sum::<f64>(), 100.0);
        assert_eq!(v.series[1].iter().sum::<f64>(), 30.0);
        assert_eq!(v.tally, Tally { calls: 3, tokens: 130, cost: 1.5 });
        let v = view(&calls, span, 2, 1, false);
        assert_eq!((v.names.as_slice(), v.series[0].iter().sum::<f64>()), (&["Claude".to_string(), "Codex".to_string()][..], 2.0), "calls per provider");
        // A day by 15 minutes sees the 2-hour-old one too; all time by its own buckets sees everything.
        assert_eq!(view(&calls, Win::H24.span(NOW, None), 1, 0, false).tally.calls, 4);
        let all = Win::All.span(NOW, oldest(&calls, NOW));
        let v = view(&calls, all, 1, 2, false);
        assert_eq!((v.tally.calls, v.series.len(), v.series.iter().flatten().sum::<f64>()), (4, 3, 2.0), "cost 0.5 each");
        assert!(v.series.iter().all(|s| s.len() == all.2));
        assert_eq!(view(&[], all, 1, 0, true).names.len(), 0, "an empty log builds nothing");
    }

    #[test]
    fn cumulative_series_only_rise_and_end_at_the_total() {
        let calls: Vec<Call> = (0..40).map(|i| call(i * 700, "Claude", if i % 3 == 0 { "A" } else { "B" }, 5 + i)).collect();
        for win in [Win::Today, Win::H24, Win::D7, Win::D31, Win::All] {
            let span = win.span(NOW, oldest(&calls, NOW));
            let plain = view(&calls, span, 1, 0, false);
            let run = view(&calls, span, 1, 0, true);
            assert_eq!(plain.names, run.names, "{win:?}");
            for (p, r) in plain.series.iter().zip(&run.series) {
                assert!(r.windows(2).all(|w| w[1] >= w[0]), "{win:?}: monotonic");
                assert_eq!(*r.last().unwrap(), p.iter().sum::<f64>(), "{win:?}: ends at the plain total");
            }
            let cur = usage::current_bucket(span, NOW);
            let (t, s): (Vec<f64>, Vec<f64>) = shares(&run.series, cur, true).into_iter().unzip();
            assert!((s.iter().sum::<f64>() - 1.0).abs() < 1e-9 || t.iter().all(|x| *x == 0.0), "{win:?}: shares add to 1");
        }
    }

    #[test]
    fn totals_shares_and_the_last_hour() {
        assert!(shares(&[], 0, false).is_empty());
        assert_eq!(shares(&[vec![0.0, 0.0]], 1, false), [(0.0, 0.0)], "nothing: no share, no NaN");
        let s = shares(&[vec![1.0, 2.0, 1.0], vec![0.0, 4.0, 0.0]], 2, false);
        assert_eq!(s, [(4.0, 0.5), (4.0, 0.5)]);
        let calls = vec![call(7200, "A", "m", 1), call(3000, "A", "m", 1), call(10, "A", "m", 1)];
        assert_eq!(last_hour(&calls, NOW), 2);
        assert_eq!(last_hour(&[], NOW), 0);
        assert_eq!(tally(&calls, Win::Hour.span(NOW, None)), Tally { calls: 2, tokens: 2, cost: 1.0 });
    }

    #[test]
    fn marks_and_headings_follow_the_range() {
        let m = marks(XMode::Slide, Win::Hour.span(NOW, None), NOW);
        assert_eq!((m.len(), m[0].0, m[4].0, m[4].1.as_str()), (5, 0.0, 1.0, "now"));
        assert!(m[0].1.contains(':'), "a day or less: clock times");
        let m = marks(XMode::Fit, Win::D31.span(NOW, None), NOW);
        assert!(m.iter().all(|(_, t)| !t.contains(':')), "longer: dates, {m:?}");
        assert!(marks(XMode::Fit, Win::Week.span(NOW, None), NOW).len() == 5);
        assert!(when(NOW, 86_400).len() <= 6, "a day: its date");
        assert!(when(NOW, 7 * 86_400).contains('–') && when(NOW, 900).contains('–'));
    }
}
