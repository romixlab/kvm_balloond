//! Damping between the plan and the balloons: a busy guest's numbers wobble every round (a build's page cache
//! comes and goes), and setting each round's plan made the balloon swing up and down every 2 s. Pure (the time
//! comes in as an argument), so every case is a test.
//!
//! - **Growing** goes through at once: a guest that needs memory must not wait.
//! - **Giving idle memory back** waits until the plan has wanted it for `shrink_settle`, and then goes to the
//!   highest target seen in that time, so a dip of a few rounds takes nothing. A round that plans nothing for the
//!   guest (it needs about what it has) starts the wait again.
//! - **Shrinking because the host is short** goes through at once: the host comes first.
//! - **A moving balloon** is left alone: after a change, nothing more is set for the guest until its balloon
//!   reports the target (within `min_change`), or `settle_timeout` has passed (a guest that can't give the
//!   memory back doesn't freeze its planning).

use crate::policy::Change;
use std::collections::HashMap;
use std::time::Duration;

/// The why of a shrink that may wait (see policy::plan).
const IDLE: &str = "idle memory back to the host";

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub shrink_settle: Duration,
    pub settle_timeout: Duration,
    /// KiB: how close the balloon must be to its last target to count as there.
    pub tolerance: u64,
}

#[derive(Default)]
struct Track {
    /// The last size set and when.
    set: Option<(u64, Duration)>,
    /// Since when the plan has wanted to give idle memory back, and the highest target since.
    shrink: Option<(Duration, u64)>,
}

#[derive(Default)]
pub struct Damper {
    guests: HashMap<String, Track>,
}

impl Damper {
    /// The changes of this round's plan that are made now. `running` are the guests still there (the others are
    /// forgotten).
    pub fn filter(
        &mut self,
        now: Duration,
        planned: Vec<Change>,
        running: &[&str],
        s: Settings,
    ) -> Vec<Change> {
        self.guests.retain(|n, _| running.contains(&n.as_str()));
        let mut out = Vec::new();
        let mut seen = Vec::new();
        for c in planned {
            seen.push(c.name.clone());
            let t = self.guests.entry(c.name.clone()).or_default();
            if let Some((to, at)) = t.set {
                let moving = c.from.abs_diff(to) > s.tolerance;
                if moving && now.saturating_sub(at) < s.settle_timeout {
                    continue;
                }
            }
            let c = if c.to < c.from && c.why == IDLE {
                let (since, high) = match t.shrink {
                    Some((since, high)) => (since, high.max(c.to)),
                    None => (now, c.to),
                };
                t.shrink = Some((since, high));
                if now.saturating_sub(since) < s.shrink_settle {
                    continue;
                }
                t.shrink = None;
                if high + s.tolerance >= c.from {
                    continue;
                }
                Change { to: high, ..c }
            } else {
                t.shrink = None;
                c
            };
            t.set = Some((c.to, now));
            out.push(c);
        }
        // no change planned: the guest needs about what it has, so a pending give-back starts over
        for (n, t) in self.guests.iter_mut() {
            if !seen.contains(n) {
                t.shrink = None;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1024 * 1024;
    const M: u64 = 1024;

    fn s() -> Settings {
        Settings {
            shrink_settle: Duration::from_secs(30),
            settle_timeout: Duration::from_secs(20),
            tolerance: 512 * M,
        }
    }

    fn change(from: u64, to: u64, why: &'static str) -> Change {
        Change {
            name: "b".into(),
            from,
            to,
            why,
        }
    }

    fn at(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    #[test]
    fn growing_goes_through_at_once() {
        let mut d = Damper::default();
        let out = d.filter(
            at(0),
            vec![change(4 * G, 6 * G, "guest short")],
            &["b"],
            s(),
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn host_short_shrinks_at_once() {
        let mut d = Damper::default();
        let out = d.filter(at(0), vec![change(8 * G, 6 * G, "host short")], &["b"], s());
        assert_eq!(out[0].to, 6 * G);
    }

    #[test]
    fn idle_give_back_waits_and_takes_the_highest_target() {
        let mut d = Damper::default();
        for (t, to) in [(0, 6 * G), (10, 7 * G), (20, 5 * G)] {
            assert!(
                d.filter(at(t), vec![change(10 * G, to, IDLE)], &["b"], s())
                    .is_empty()
            );
        }
        let out = d.filter(at(30), vec![change(10 * G, 6 * G, IDLE)], &["b"], s());
        assert_eq!(out, vec![change(10 * G, 7 * G, IDLE)]);
    }

    #[test]
    fn a_round_without_a_change_starts_the_wait_over() {
        let mut d = Damper::default();
        d.filter(at(0), vec![change(10 * G, 6 * G, IDLE)], &["b"], s());
        d.filter(at(20), vec![], &["b"], s());
        assert!(
            d.filter(at(30), vec![change(10 * G, 6 * G, IDLE)], &["b"], s())
                .is_empty()
        );
        assert_eq!(
            d.filter(at(60), vec![change(10 * G, 6 * G, IDLE)], &["b"], s())
                .len(),
            1
        );
    }

    #[test]
    fn a_moving_balloon_is_left_alone_until_it_arrives_or_times_out() {
        let mut d = Damper::default();
        d.filter(
            at(0),
            vec![change(4 * G, 8 * G, "guest short")],
            &["b"],
            s(),
        );
        // still at 5G two rounds later: wait
        assert!(
            d.filter(
                at(2),
                vec![change(5 * G, 9 * G, "guest short")],
                &["b"],
                s()
            )
            .is_empty()
        );
        // arrived (within the tolerance): go on
        assert_eq!(
            d.filter(
                at(4),
                vec![change(8 * G - 100 * M, 9 * G, "guest short")],
                &["b"],
                s()
            )
            .len(),
            1
        );
        // stuck: after the timeout it is planned again
        assert!(
            d.filter(
                at(6),
                vec![change(8 * G, 10 * G, "guest short")],
                &["b"],
                s()
            )
            .is_empty()
        );
        assert_eq!(
            d.filter(
                at(30),
                vec![change(8 * G, 10 * G, "guest short")],
                &["b"],
                s()
            )
            .len(),
            1
        );
    }

    #[test]
    fn a_wobbling_build_makes_few_changes() {
        // alma-1b on 14 Nov 2026: the plan swung between 10.9G and 12.1G every 2 s, 323 changes in 16 min
        let mut d = Damper::default();
        let mut current = 11 * G + 500 * M;
        let mut made = 0;
        for i in 0..480u64 {
            let wobble = [
                12 * G + 100 * M,
                11 * G + 200 * M,
                11 * G + 900 * M,
                10 * G + 900 * M,
            ][(i % 4) as usize];
            let c = if wobble > current {
                change(current, wobble, "grow")
            } else {
                change(current, wobble, IDLE)
            };
            let planned = if c.to.abs_diff(c.from) < 512 * M {
                vec![]
            } else {
                vec![c]
            };
            for c in d.filter(at(i * 2), planned, &["b"], s()) {
                current = c.to;
                made += 1;
            }
        }
        assert!(made <= 3, "{made} changes in 16 min");
    }

    #[test]
    fn gone_guests_are_forgotten() {
        let mut d = Damper::default();
        d.filter(
            at(0),
            vec![change(4 * G, 8 * G, "guest short")],
            &["b"],
            s(),
        );
        d.filter(at(1), vec![], &[], s());
        // back with a fresh start: not taken as a moving balloon
        assert_eq!(
            d.filter(
                at(2),
                vec![change(4 * G, 8 * G, "guest short")],
                &["b"],
                s()
            )
            .len(),
            1
        );
    }
}
