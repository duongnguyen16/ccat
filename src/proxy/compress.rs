//! Lossless-ish cleanup of `tool_result` text for third-party routes (never Claude): ANSI escapes, CR redraws,
//! trailing spaces, runs of blank or identical lines. Providers cache the prompt prefix, so this is a PURE,
//! DETERMINISTIC, IDEMPOTENT function of the text, applied to EVERY tool result on EVERY request (not just the
//! newest): the same history compresses to the same bytes each turn, and a result that arrives pre-compressed
//! (RTK as a Bash hook) comes out unchanged. It only cleans, never truncates, summarises or reorders; error results
//! (`is_error`), images and every other block are left alone.
//! ponytail: no semantic compression (dedup of similar-but-not-equal lines, log summarising); that needs a heuristic
//! that can lose what the model needs, so it would be its own opt-in.

use serde_json::Value;

/// Cleans every tool result's text in `body["messages"]`; the bytes saved.
pub(crate) fn tool_results(body: &mut Value) -> usize {
    let blocks = body.get_mut("messages").and_then(Value::as_array_mut).into_iter().flatten().filter_map(|m| m.get_mut("content")?.as_array_mut()).flatten();
    blocks
        .filter(|b| b["type"] == "tool_result" && b["is_error"] != true)
        .map(|b| match b.get_mut("content") {
            Some(c @ Value::String(_)) => squeeze(c),
            Some(Value::Array(a)) => a.iter_mut().filter(|x| x["type"] == "text").filter_map(|x| x.get_mut("text")).map(squeeze).sum(),
            _ => 0,
        })
        .sum()
}

fn squeeze(v: &mut Value) -> usize {
    let Some(old) = v.as_str() else { return 0 };
    let Some(new) = text(old) else { return 0 };
    let saved = old.len() - new.len();
    *v = Value::String(new);
    saved
}

/// The cleaned text, or `None` when that would not be shorter (the original stays).
pub(crate) fn text(s: &str) -> Option<String> {
    // To a fixed point, so `text(text(x))` is `text(x)` whatever the input (a marker line could, in theory, itself
    // sit in a run). Every pass only shrinks, so it ends.
    let mut cur = pass(s);
    loop {
        let next = pass(&cur);
        if next == cur {
            break;
        }
        cur = next;
    }
    (cur.len() < s.len()).then_some(cur)
}

fn pass(s: &str) -> String {
    let s = strip_escapes(s).replace("\r\n", "\n");
    // What a terminal shows: after the last `\r` (a trailing one keeps the text before it).
    let lines: Vec<&str> = s.split('\n').map(|l| l.rsplit('\r').find(|p| !p.is_empty()).unwrap_or("").trim_end()).collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        let n = lines[i..].iter().take_while(|x| **x == l).count();
        i += n;
        let marker = format!("… (same line repeated {} more times)", n - 1);
        // A run is only worth a marker when the marker is shorter than the lines it replaces.
        let keep = if l.is_empty() && n >= 3 {
            1
        } else if n >= 4 && (n - 1) * (l.len() + 1) > marker.len() + 1 {
            out.push(l.to_string());
            out.push(marker);
            continue;
        } else {
            n
        };
        out.extend(std::iter::repeat_n(l.to_string(), keep));
    }
    out.join("\n")
}

/// Drops CSI (`ESC [ … final`), OSC (`ESC ] … BEL | ESC \`) and the short `ESC`-sequences. No ESC survives.
fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('[') => {
                while it.next_if(|c| ('\x20'..='\x3f').contains(c)).is_some() {}
                it.next_if(|c| ('\x40'..='\x7e').contains(c));
            }
            Some(']') => loop {
                match it.next() {
                    None | Some('\x07') => break,
                    Some('\x1b') => {
                        it.next_if_eq(&'\\');
                        break;
                    }
                    _ => {}
                }
            },
            // `ESC ( B` and the like: intermediates, then the final byte.
            Some('\x20'..='\x2f') => {
                while it.next_if(|c| ('\x20'..='\x2f').contains(c)).is_some() {}
                it.next();
            }
            _ => {} // `ESC c`, `ESC 7`…: the character after it went with it
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_escape_sequences() {
        assert_eq!(text("\x1b[1m\x1b[32m   Compiling\x1b[0m foo\n").unwrap(), "   Compiling foo\n");
        assert_eq!(text("a\x1b]0;title\x07b\x1b]8;;http://x\x1b\\c\x1b(Bd\x1bce").unwrap(), "abcde", "OSC (BEL and ST), charset and lone ESC sequences");
        assert_eq!(text("a\x1b[2K\x1b[1Gb").unwrap(), "ab", "params and final bytes of any kind");
        assert!(!text("x\x1b[31").unwrap_or_default().contains('\x1b'), "an unfinished sequence still leaves no ESC");
    }

    #[test]
    fn carriage_returns_keep_what_a_terminal_shows() {
        assert_eq!(text("a\r\nb\r\n").unwrap(), "a\nb\n", "CRLF is LF");
        assert_eq!(text("10%\r50%\r100% done\nnext").unwrap(), "100% done\nnext", "a redraw keeps the last text");
        assert_eq!(text("work\r\r\nok").unwrap(), "work\nok", "a trailing CR keeps the line");
    }

    #[test]
    fn trims_trailing_spaces_and_collapses_blank_runs() {
        assert_eq!(text("a  \t\nb \n").unwrap(), "a\nb\n");
        assert_eq!(text("a\n\n\n\n\nb").unwrap(), "a\n\nb", "3+ blank lines become one");
        assert!(text("a\n\n\n\nb").is_some() && text("a\n\n\nb").is_none(), "two blank lines stay");
    }

    #[test]
    fn collapses_runs_of_identical_lines() {
        let line = "warning: unused variable `x` in this function";
        let long = format!("start\n{}end", format!("{line}\n").repeat(10));
        assert_eq!(text(&long).unwrap(), format!("start\n{line}\n… (same line repeated 9 more times)\nend"));
        assert!(text(&format!("{line}\n{line}\n{line}\n")).is_none(), "three are left alone");
        assert!(text("x\nx\nx\nx\nx\n").is_none(), "short lines: the marker would be longer than the run");
    }

    #[test]
    fn nothing_else_changes_and_not_shorter_returns_the_original() {
        let plain = "line one\n  indented  text\n\nline three\n";
        assert!(text(plain).is_none());
        assert!(text("").is_none());
        assert!(text("héllo wörld ✓\n").is_none(), "no truncation, no reordering, non-ASCII intact");
    }

    #[test]
    fn idempotent_and_deterministic_over_a_mixed_sample() {
        let marker_run = format!("{}\n{}", "… (same line repeated 3 more times)", "x");
        let samples = [
            cargo_log(),
            "a\x1b[31mred\x1b[0m   \r\n\n\n\n\n\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\nb\n".into(),
            // An input that already holds marker lines, even a run of them after a collapsed run.
            format!("{}{}", "ab\n".repeat(30), "… (same line repeated 29 more times)\n".repeat(5)),
            format!("{marker_run}\n{marker_run}"),
        ];
        for s in samples {
            let once = text(&s).unwrap_or_else(|| s.clone());
            assert_eq!(text(&s).unwrap_or_else(|| s.clone()), once, "deterministic");
            assert_eq!(text(&once).unwrap_or_else(|| once.clone()), once, "idempotent: {once:?}");
        }
    }

    #[test]
    fn tool_results_in_a_request() {
        let dirty = "\x1b[31mred\x1b[0m   \n\n\n\n\nend";
        let mut body = json!({"model":"m","messages":[
            {"role":"user","content":"\x1b[31mnot a tool result\x1b[0m"},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"1","content":dirty},
                {"type":"tool_result","tool_use_id":"2","content":[{"type":"text","text":dirty},{"type":"image","source":{"type":"base64","data":"\x1b[1m"}}]},
                {"type":"tool_result","tool_use_id":"3","is_error":true,"content":dirty},
                {"type":"tool_result","tool_use_id":"4"},
                {"type":"text","text":dirty}]}]});
        let before = body.clone();
        let saved = tool_results(&mut body);
        let cleaned = "red\n\nend";
        assert_eq!(saved, 2 * (dirty.len() - cleaned.len()));
        let blocks = &body["messages"][1]["content"];
        assert_eq!(blocks[0]["content"], cleaned);
        assert_eq!(blocks[1]["content"][0]["text"], cleaned);
        assert_eq!(blocks[1]["content"][1], before["messages"][1]["content"][1]["content"][1], "images are left alone");
        assert_eq!(blocks[2], before["messages"][1]["content"][2], "an error result is left alone");
        assert_eq!(blocks[3], before["messages"][1]["content"][3], "no content: no key added");
        assert_eq!(blocks[4], before["messages"][1]["content"][4], "only tool results");
        assert_eq!(body["messages"][0], before["messages"][0]);
        let again = body.clone();
        assert_eq!(tool_results(&mut body), 0);
        assert_eq!(body, again, "a second request with the same history changes nothing more");
    }

    /// A cargo build with colours and a progress bar redrawn on one line.
    fn cargo_log() -> String {
        let mut log = String::new();
        for i in 0..120 {
            log.push_str(&format!("\x1b[1m\x1b[32m   Compiling\x1b[0m crate-{i} v1.{i}.0\n"));
            log.push_str(&format!("\x1b[1m\x1b[36m    Building\x1b[0m [{}>{}] {i}/120: crate-{i}        \r", "=".repeat(i / 4), " ".repeat(30 - i / 4)));
        }
        log.push_str(&format!("{}\x1b[1m\x1b[33mwarning\x1b[0m: unused import: `std::fmt`   \n", "\n".repeat(6)));
        log.push_str(&"\x1b[1m\x1b[33mwarning\x1b[0m: `foo` generated 1 warning\n".repeat(25));
        log.push_str("\x1b[1m\x1b[32m    Finished\x1b[0m `dev` profile in 41.2s\n");
        log
    }

    #[test]
    fn a_realistic_cargo_log_shrinks_a_lot() {
        let log = cargo_log();
        let out = text(&log).unwrap();
        eprintln!("cargo log: {} -> {} bytes ({:.0}% saved)", log.len(), out.len(), 100.0 * (log.len() - out.len()) as f64 / log.len() as f64);
        assert!(out.len() * 2 < log.len());
        assert!(!out.contains('\x1b') && !out.contains('\r') && out.contains("Compiling crate-119 v1.119.0") && out.contains("Finished `dev` profile in 41.2s"));
    }
}
