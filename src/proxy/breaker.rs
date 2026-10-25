//! A per-upstream circuit breaker: after `TRIP_AFTER` failures in a row (5xx, timeouts, connection errors — never a
//! 429 or other 4xx, those are usage limits and refusals with their own handling) a provider is skipped for the
//! cooldown, then one request may probe it: success closes it, failure re-opens it. It only changes WHICH route is
//! tried, never a request. The caller keeps the "never fail without trying" rule: when every route is open it tries all.
//! ponytail: per process (not shared across ccat sessions), keyed by provider name, and the last try of a request
//! (whose failure passes straight to Claude Code, see `Fail`) isn't seen, so a provider that only ever ends a chain never trips.

use crate::common;
use std::{collections::HashMap, sync::Mutex, time::{Duration, Instant}};

const TRIP_AFTER: u32 = 3;
const COOLDOWN: Duration = Duration::from_secs(30);

pub(crate) struct Breaker {
    cooldown: Duration,
    /// provider → (consecutive failures, open until).
    state: Mutex<HashMap<String, (u32, Option<Instant>)>>,
}

impl Default for Breaker {
    fn default() -> Self {
        Self::new(COOLDOWN)
    }
}

impl Breaker {
    pub(crate) fn new(cooldown: Duration) -> Self {
        Breaker { cooldown, state: Mutex::default() }
    }

    /// Whether `key` may be tried now. Open: no. Cooldown over: yes, and the others wait another cooldown, so one
    /// request probes (ponytail: a probe that never reports back lets the next one through after that).
    pub(crate) fn allow(&self, key: &str) -> bool {
        let mut m = self.state.lock().unwrap();
        let Some((_, open)) = m.get_mut(key) else { return true };
        match *open {
            Some(t) if Instant::now() < t => false,
            Some(_) => {
                *open = Some(Instant::now() + self.cooldown);
                common::log(&format!("circuit breaker: {key} half-open, probing"));
                true
            }
            None => true,
        }
    }

    /// The upstream answered (anything but a failure): closes the breaker and forgets the failures.
    pub(crate) fn ok(&self, key: &str) {
        if self.state.lock().unwrap().remove(key).is_some_and(|(_, open)| open.is_some()) {
            common::log(&format!("circuit breaker: {key} closed"));
        }
    }

    /// A 5xx, a timeout or a connection error.
    pub(crate) fn fail(&self, key: &str) {
        let mut m = self.state.lock().unwrap();
        let (n, open) = m.entry(key.to_string()).or_default();
        *n += 1;
        if *n >= TRIP_AFTER {
            *open = Some(Instant::now() + self.cooldown);
            common::log(&format!("circuit breaker: {key} open for {}s after {n} failures in a row", self.cooldown.as_secs()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trips_after_three_failures_and_a_success_resets_the_count() {
        let b = Breaker::default();
        b.fail("a");
        b.fail("a");
        b.ok("a");
        b.fail("a");
        b.fail("a");
        assert!(b.allow("a"), "two in a row after a success: still closed");
        b.fail("a");
        assert!(!b.allow("a") && b.allow("b"), "third in a row opens a, only a");
    }

    #[test]
    fn half_opens_after_the_cooldown_with_one_probe() {
        let b = Breaker::new(Duration::from_millis(60));
        for _ in 0..3 {
            b.fail("a");
        }
        assert!(!b.allow("a"));
        std::thread::sleep(Duration::from_millis(80));
        assert!(b.allow("a"), "the probe goes through");
        assert!(!b.allow("a"), "and holds the others off while it runs");
        b.fail("a");
        assert!(!b.allow("a"), "a failed probe re-opens it");
        std::thread::sleep(Duration::from_millis(80));
        assert!(b.allow("a"));
        b.ok("a");
        assert!(b.allow("a") && b.allow("a"), "a successful probe closes it");
        b.fail("a");
        b.fail("a");
        assert!(b.allow("a"), "closing forgot the old failures");
    }
}
