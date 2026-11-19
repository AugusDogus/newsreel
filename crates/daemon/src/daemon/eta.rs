//! A download's time left, steady enough to read.
//!
//! The Usenet engine counts the bytes that landed each second, and articles land whole, about
//! 768 KB at a time, so one second's rate swings from nothing to several times the mean. Time left
//! from that rate jumped between 102 s and 726 s within 12 s while Charade (1963) was repaired. So
//! the time left comes from the mean rate over the last [`WINDOW`], and the time shown counts down
//! by itself and moves toward a new estimate by at most a fortieth a second.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How far back the rate is averaged.
pub const WINDOW: Duration = Duration::from_secs(30);
/// How long bytes must have been arriving before there is a time left.
pub const AFTER: Duration = Duration::from_secs(10);
/// The most the time shown moves toward a new estimate each second, as a part of it.
const STEP: u64 = 40;
/// The least it may move each second, so a short time left settles soon.
const STEP_LEAST: u64 = 2;

#[derive(Clone, Debug, Default)]
pub struct Eta {
    /// The rates sampled within the window: when, and bytes per second.
    rates: VecDeque<(Instant, u64)>,
    /// Since when the mean rate has been above zero.
    receiving_since: Option<Instant>,
    /// The time left shown, and when.
    shown: Option<(u64, Instant)>,
}

impl Eta {
    /// Takes the rate the engine measured at `at`, and `remaining` bytes. `known` is false while
    /// nothing says how long the rest takes, such as when no source has it, which forgets the time
    /// shown.
    pub fn sample(&mut self, rate: u64, remaining: u64, known: bool, at: Instant) {
        self.rates.push_back((at, rate));
        while self.rates.front().is_some_and(|&(when, _)| at.saturating_duration_since(when) > WINDOW) {
            self.rates.pop_front();
        }
        let mean = self.rate();
        self.receiving_since = if mean > 0 { self.receiving_since.or(Some(at)) } else { None };
        let settled = self.receiving_since.is_some_and(|since| at.saturating_duration_since(since) >= AFTER);
        if !known || !settled {
            self.shown = None;
            return;
        }
        let estimate = remaining.div_ceil(mean);
        let shown = match self.shown {
            None => estimate,
            Some((shown, when)) => {
                let seconds = at.saturating_duration_since(when).as_secs_f64().max(1.0);
                let counted = shown.saturating_sub(at.saturating_duration_since(when).as_secs());
                let most = ((counted / STEP).max(STEP_LEAST) as f64 * seconds) as u64;
                if estimate > counted {
                    counted + (estimate - counted).min(most)
                } else {
                    counted - (counted - estimate).min(most)
                }
            }
        };
        self.shown = Some((shown, at));
    }

    /// Seconds left, or None while unknown.
    pub fn seconds(&self) -> Option<u64> {
        self.shown.map(|(shown, _)| shown)
    }

    /// The mean rate over the window, in bytes per second.
    pub fn rate(&self) -> u64 {
        match self.rates.len() {
            0 => 0,
            n => self.rates.iter().map(|&(_, rate)| rate).sum::<u64>() / n as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1_000_000;

    /// The download's rate every 3 s while Charade (1963) was repaired on Linux, in MB/s, from
    /// 62.6 s on (`repair-api-timeline.log`). The engine's own time left read 125, 492, 726, 102,
    /// and 465 s over the second to sixth of them.
    const CHARADE: [f64; 30] = [
        4.4, 8.7, 2.2, 1.5, 10.2, 2.2, 4.4, 6.5, 7.3, 5.1, 5.1, 4.4, 6.6, 7.3, 7.3, 6.6, 3.7, 2.9, 5.1, 6.6, 5.1, 4.4,
        5.8, 3.7, 5.8, 2.9, 2.9, 5.1, 7.3, 6.6,
    ];

    /// The time left shown after each 3 s rate, fed as three one-second samples, from `remaining`
    /// bytes after the download has been receiving for `AFTER`.
    fn shown(rates: &[f64], mut remaining: u64) -> Vec<Option<u64>> {
        let start = Instant::now();
        let mut eta = Eta::default();
        let mut second = 0;
        for _ in 0..AFTER.as_secs() {
            eta.sample(5 * MB, remaining, true, start + Duration::from_secs(second));
            second += 1;
        }
        let mut out = Vec::new();
        for &rate in rates {
            let rate = (rate * MB as f64) as u64;
            for _ in 0..3 {
                remaining = remaining.saturating_sub(rate);
                eta.sample(rate, remaining, true, start + Duration::from_secs(second));
                second += 1;
            }
            out.push(eta.seconds());
        }
        out
    }

    #[test]
    fn the_time_left_moves_gradually_through_a_rate_that_swings_and_follows_the_mean() {
        let shown: Vec<u64> = shown(&CHARADE, 1_130 * MB).into_iter().map(Option::unwrap).collect();
        // Every 3 s it counts down 3 s, and moves at most a fortieth a second besides.
        for pair in shown.windows(2) {
            let counted = pair[0].saturating_sub(3);
            let most = (counted / STEP).max(STEP_LEAST) * 3;
            assert!(pair[1].abs_diff(counted) <= most, "{shown:?}");
        }
        // Over the 12 s in which the engine's said 125 to 726 s, it stays within a fifth.
        let (lo, hi) = (shown[1..6].iter().min().unwrap(), shown[1..6].iter().max().unwrap());
        assert!(hi * 5 <= lo * 6, "{shown:?}");
        // The rate's mean is about 5 MB/s, so 1,117 MB left after the first 3 s take about 223 s.
        assert!(shown[0].abs_diff(223) * 5 <= 223, "{shown:?}");
    }

    #[test]
    fn there_is_no_time_left_until_bytes_have_arrived_for_a_while_or_while_no_source_has_the_rest() {
        let start = Instant::now();
        let mut eta = Eta::default();
        let at = |s| start + Duration::from_secs(s);
        eta.sample(10 * MB, 600 * MB, true, at(0));
        eta.sample(10 * MB, 600 * MB, true, at(9));
        assert_eq!(eta.seconds(), None);
        eta.sample(10 * MB, 600 * MB, true, at(10));
        assert_eq!(eta.seconds(), Some(60));
        // A second with nothing landed is part of the mean, not the end of the estimate.
        eta.sample(0, 600 * MB, true, at(11));
        assert!(eta.seconds().is_some());
        eta.sample(10 * MB, 600 * MB, false, at(12));
        assert_eq!(eta.seconds(), None);
        // Nothing for a whole window starts over.
        for s in 13..=45 {
            eta.sample(0, 600 * MB, true, at(s));
        }
        eta.sample(10 * MB, 600 * MB, true, at(46));
        assert_eq!(eta.seconds(), None);
    }
}
