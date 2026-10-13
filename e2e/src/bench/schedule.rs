//! When each vote of a benchmark wave is sent: arrival phases (a uniform or
//! Poisson rate, a burst, a pause) and the follow-up rounds of revoters.

use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// `per_min` votes a minute for `secs`, evenly spaced or Poisson.
    Rate {
        per_min: f64,
        secs: u64,
        poisson: bool,
    },
    /// `n` votes at the phase start.
    Burst {
        n: usize,
    },
    Idle {
        secs: u64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    pub phases: Vec<Phase>,
}

/// Seconds to a duration; a non-finite or negative value is zero.
fn dur(s: f64) -> Duration {
    if s.is_finite() && s > 0.0 {
        Duration::from_secs_f64(s)
    } else {
        Duration::ZERO
    }
}

/// Emits votes of one rate from `t0` on, never going back in time.
struct Arrivals {
    out: Vec<Duration>,
    n: usize,
}

impl Arrivals {
    fn push(&mut self, t: f64) {
        if self.out.len() < self.n {
            let d = dur(t).max(self.out.last().copied().unwrap_or_default());
            self.out.push(d);
        }
    }

    fn full(&self) -> bool {
        self.out.len() >= self.n
    }

    /// `count` votes (all that fit when `None`) at `per_min` from `t0`.
    fn rate(
        &mut self,
        t0: f64,
        per_min: f64,
        poisson: bool,
        count: Option<usize>,
        rng: &mut StdRng,
    ) {
        if per_min <= 0.0 {
            return;
        }
        let step = 60.0 / per_min;
        let mut t = t0;
        for k in 0..count.unwrap_or(usize::MAX) {
            if self.full() {
                break;
            }
            if poisson {
                // Inverse-CDF exponential gap; 1 - u is never 0.
                t += -(1.0 - rng.r#gen::<f64>()).ln() * step;
                self.push(t);
            } else {
                self.push(t0 + k as f64 * step);
            }
        }
    }
}

impl Schedule {
    /// Votes the phases hold (a rate holds `per_min * secs / 60`).
    pub fn capacity(&self) -> usize {
        self.phases
            .iter()
            .map(|p| match p {
                Phase::Rate { per_min, secs, .. } => {
                    (per_min * *secs as f64 / 60.0).round() as usize
                }
                Phase::Burst { n } => *n,
                Phase::Idle { .. } => 0,
            })
            .sum()
    }

    /// Exactly `n` send times, in order. Past the phases the last rate goes on.
    pub fn offsets(&self, n: usize, seed: u64) -> Vec<Duration> {
        let mut a = Arrivals {
            out: Vec::with_capacity(n),
            n,
        };
        let mut t = 0.0;
        let mut last_rate = None;
        for (i, p) in self.phases.iter().enumerate() {
            if a.full() {
                break;
            }
            match *p {
                Phase::Rate {
                    per_min,
                    secs,
                    poisson,
                } => {
                    let mut rng = StdRng::seed_from_u64(seed ^ i as u64);
                    let count = (per_min * secs as f64 / 60.0).round() as usize;
                    a.rate(t, per_min, poisson, Some(count), &mut rng);
                    t += secs as f64;
                    last_rate = Some((per_min, poisson));
                }
                Phase::Burst { n } => {
                    for _ in 0..n {
                        a.push(t);
                    }
                }
                Phase::Idle { secs } => t += secs as f64,
            }
        }
        if !a.full() {
            // After the plan: the last rate (one a second if there was none)
            // from where the last emitted vote or phase ended.
            let (per_min, poisson) = last_rate.unwrap_or((60.0, false));
            let from = t.max(a.out.last().map_or(0.0, Duration::as_secs_f64));
            let mut rng = StdRng::seed_from_u64(seed ^ self.phases.len() as u64);
            a.rate(from, per_min, poisson, None, &mut rng);
        }
        a.out
    }
}

/// How long after a vote the same voter's next round goes out.
#[derive(Clone, Debug, PartialEq)]
pub enum Gap {
    /// Uniform in `[min, max]` seconds.
    Overwrite { min: u64, max: u64 },
    /// A fixed number of seconds.
    Revote { secs: u64 },
}

/// Round 1 at `offsets[i]` for `first[i]`; round r >= 2 of voter v at its
/// round r-1 time plus the gap. A later round whose predecessor is not
/// scheduled is dropped. Sorted by time.
pub fn order(
    first: &[u32],
    later: &[(u32, u8)],
    offsets: &[Duration],
    gap: &Gap,
    seed: u64,
) -> Vec<(Duration, u32, u8)> {
    let mut out: Vec<(Duration, u32, u8)> = first
        .iter()
        .zip(offsets)
        .map(|(v, t)| (*t, *v, 1))
        .collect();
    let mut at: std::collections::HashMap<(u32, u8), Duration> =
        out.iter().map(|(t, v, r)| ((*v, *r), *t)).collect();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut later = later.to_vec();
    later.sort_by_key(|(_, r)| *r); // stable: a round follows the one before it
    for (v, r) in later {
        let Some(prev) = r.checked_sub(1).and_then(|p| at.get(&(v, p))).copied() else {
            continue;
        };
        let secs = match *gap {
            Gap::Overwrite { min, max } => rng.gen_range(min.min(max) as f64..=min.max(max) as f64),
            Gap::Revote { secs } => secs as f64,
        };
        let t = prev + dur(secs);
        at.insert((v, r), t);
        out.push((t, v, r));
    }
    out.sort_by_key(|(t, _, _)| *t);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn rate(per_min: f64, secs: u64, poisson: bool) -> Phase {
        Phase::Rate {
            per_min,
            secs,
            poisson,
        }
    }

    #[test]
    fn uniform_rate_burst_and_determinism() {
        let s = Schedule {
            phases: vec![
                rate(60.0, 600, false),
                Phase::Burst { n: 50 },
                Phase::Idle { secs: 30 },
                rate(120.0, 60, true),
            ],
        };
        assert_eq!(s.capacity(), 600 + 50 + 120);
        let o = s.offsets(s.capacity(), 7);
        assert_eq!(o.len(), 770);
        assert!(o.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!((o[0], o[599]), (Duration::ZERO, Duration::from_secs(599)));
        assert!(o[600..650].iter().all(|t| *t == Duration::from_secs(600)));
        assert!(o[650] >= Duration::from_secs(630)); // after the idle phase
        assert_eq!(s.offsets(100, 7), s.offsets(100, 7));
        assert_ne!(s.offsets(770, 7)[700..], s.offsets(770, 8)[700..]);
        assert_eq!(s.offsets(800, 7).len(), 800); // past the end the last rate continues
    }

    #[test]
    fn poisson_keeps_the_mean_rate() {
        let s = Schedule {
            phases: vec![rate(600.0, 3600, true)],
        };
        let span = s.offsets(6000, 1).last().unwrap().as_secs_f64();
        assert!((span - 600.0).abs() < 30.0, "{span}"); // 10/s; sd ≈ 7.7 s
    }

    #[test]
    fn later_rounds_follow_their_voter() {
        let first: Vec<u32> = (0..100).collect();
        let offs = Schedule {
            phases: vec![rate(60.0, 100, false)],
        }
        .offsets(100, 1);
        let ord = order(
            &first,
            &[(3, 2), (3, 3), (50, 2)],
            &offs,
            &Gap::Overwrite { min: 5, max: 30 },
            9,
        );
        let at = |v: u32, r: u8| ord.iter().find(|x| x.1 == v && x.2 == r).unwrap().0;
        assert_eq!(ord.len(), 103);
        assert!(ord.windows(2).all(|w| w[0].0 <= w[1].0));
        let d = at(3, 2) - at(3, 1);
        assert!(
            d >= Duration::from_secs(5) && d <= Duration::from_secs(30),
            "{d:?}"
        );
        assert!(at(3, 3) > at(3, 2));
        let rv = order(&first, &[(50, 2)], &offs, &Gap::Revote { secs: 900 }, 9);
        assert_eq!(rv.last().unwrap(), &(Duration::from_secs(50 + 900), 50, 2));
    }
}
