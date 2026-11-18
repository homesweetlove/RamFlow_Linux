//! Pressure policy: turn raw readings into a stable decision.
//!
//! Raw readings jump around, and acting on every spike would make RamFlow itself the source
//! of jitter. So: a weighted score from the sensors that exist, smoothed with an EMA, and a
//! hysteresis band with N-in-a-row confirmation before any state change.

use crate::config::Config;
use crate::proc::{MemInfo, Psi};

/// Exponential moving average.
#[derive(Debug, Clone)]
pub struct Ema {
    alpha: f64,
    value: Option<f64>,
}

impl Ema {
    pub fn value(&self) -> Option<f64> {
        self.value
    }

    pub fn new(alpha: f64) -> Self {
        Ema {
            alpha: alpha.clamp(0.01, 1.0),
            value: None,
        }
    }

    pub fn update(&mut self, x: f64) -> f64 {
        let v = match self.value {
            None => x,
            Some(prev) => prev + self.alpha * (x - prev),
        };
        self.value = Some(v);
        v
    }
}

/// Schmitt-trigger with confirmation: enters at `>= enter` and leaves at `<= leave`, and
/// each change needs `confirm` consecutive samples.
#[derive(Debug, Clone)]
pub struct Hysteresis {
    enter: f64,
    leave: f64,
    confirm: u32,
    active: bool,
    streak: u32,
}

impl Hysteresis {
    pub fn new(enter: f64, leave: f64, confirm: u32) -> Self {
        Hysteresis {
            enter,
            leave,
            confirm: confirm.max(1),
            active: false,
            streak: 0,
        }
    }

    pub fn update(&mut self, x: f64) -> bool {
        let crossing = if self.active {
            x <= self.leave
        } else {
            x >= self.enter
        };
        if crossing {
            self.streak += 1;
            if self.streak >= self.confirm {
                self.active = !self.active;
                self.streak = 0;
            }
        } else {
            self.streak = 0;
        }
        self.active
    }

    pub fn active(&self) -> bool {
        self.active
    }
}

/// Sensor readings relevant to memory pressure. Missing sensors are simply left out of the
/// score rather than counted as zero.
#[derive(Debug, Clone, Copy, Default)]
pub struct Signals {
    pub available_pct: f64,
    pub psi_some10: Option<f64>,
    pub psi_full10: Option<f64>,
    pub swap_used_pct: Option<f64>,
}

impl Signals {
    pub fn from_readings(mem: &MemInfo, psi: Option<&Psi>) -> Self {
        Signals {
            available_pct: mem.available_pct(),
            psi_some10: psi.map(|p| p.some_avg10),
            psi_full10: psi.map(|p| p.full_avg10),
            swap_used_pct: mem.swap_used_pct(),
        }
    }
}

fn scale(x: f64, per_unit: f64) -> f64 {
    (x * per_unit).clamp(0.0, 100.0)
}

/// Memory pressure score, 0 (relaxed) .. 100 (severe).
///
/// * available memory: 25 % free or more scores 0, 5 % free or less scores 100
/// * PSI `some`: 25 % of the time stalled scores 100
/// * PSI `full`: 10 % of the time with *everyone* stalled scores 100
/// * swap in use: above 20 % starts to count
pub fn memory_pressure(s: &Signals) -> f64 {
    let mut score = scale(25.0 - s.available_pct, 5.0) * 0.40;
    let mut weight = 0.40;
    if let Some(v) = s.psi_some10 {
        score += scale(v, 4.0) * 0.35;
        weight += 0.35;
    }
    if let Some(v) = s.psi_full10 {
        score += scale(v, 10.0) * 0.15;
        weight += 0.15;
    }
    if let Some(v) = s.swap_used_pct {
        score += scale(v - 20.0, 1.25) * 0.10;
        weight += 0.10;
    }
    (score / weight).clamp(0.0, 100.0)
}

/// What the memory policy concluded for one sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryState {
    pub score: f64,
    pub critical: bool,
}

/// Smooths the memory score and decides when memory is *critical*.
#[derive(Debug, Clone)]
pub struct MemoryTracker {
    ema: Ema,
    crit: Hysteresis,
}

impl MemoryTracker {
    pub fn new(c: &Config) -> Self {
        MemoryTracker {
            ema: Ema::new(0.4),
            crit: Hysteresis::new(c.critical_enter, c.critical_leave, c.confirm),
        }
    }

    pub fn update(&mut self, signals: &Signals) -> MemoryState {
        let score = self.ema.update(memory_pressure(signals));
        MemoryState {
            score,
            critical: self.crit.update(score),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ema_seeds_with_first_value_then_smooths() {
        let mut e = Ema::new(0.5);
        assert_eq!(e.update(100.0), 100.0);
        assert_eq!(e.update(0.0), 50.0);
        assert_eq!(e.update(0.0), 25.0);
    }

    #[test]
    fn hysteresis_needs_confirmation_to_enter() {
        let mut h = Hysteresis::new(80.0, 60.0, 3);
        assert!(!h.update(90.0));
        assert!(!h.update(90.0));
        assert!(h.update(90.0), "third consecutive sample enters");
    }

    #[test]
    fn a_single_dip_resets_the_entry_streak() {
        let mut h = Hysteresis::new(80.0, 60.0, 3);
        h.update(90.0);
        h.update(90.0);
        h.update(70.0); // dip below enter: streak reset
        assert!(!h.update(90.0));
        assert!(!h.update(90.0));
        assert!(h.update(90.0));
    }

    #[test]
    fn values_inside_the_band_hold_the_current_state() {
        let mut h = Hysteresis::new(80.0, 60.0, 1);
        assert!(h.update(85.0));
        for _ in 0..10 {
            assert!(h.update(70.0), "70 is between leave and enter: stay active");
        }
        assert!(!h.update(55.0));
        for _ in 0..10 {
            assert!(!h.update(70.0), "and stay inactive from the other side too");
        }
    }

    #[test]
    fn hysteresis_needs_confirmation_to_leave() {
        let mut h = Hysteresis::new(80.0, 60.0, 2);
        h.update(90.0);
        h.update(90.0);
        assert!(h.active());
        assert!(h.update(50.0));
        assert!(!h.update(50.0));
    }

    #[test]
    fn relaxed_machine_scores_zero() {
        let s = Signals {
            available_pct: 60.0,
            psi_some10: Some(0.0),
            psi_full10: Some(0.0),
            swap_used_pct: Some(0.0),
        };
        assert_eq!(memory_pressure(&s), 0.0);
    }

    #[test]
    fn starved_machine_scores_near_max() {
        let s = Signals {
            available_pct: 2.0,
            psi_some10: Some(40.0),
            psi_full10: Some(20.0),
            swap_used_pct: Some(100.0),
        };
        assert!(memory_pressure(&s) > 99.0);
    }

    #[test]
    fn missing_sensors_do_not_dilute_the_score() {
        // Only "available" exists and it is at the worst value → still 100, not 40.
        let s = Signals {
            available_pct: 1.0,
            ..Default::default()
        };
        assert!((memory_pressure(&s) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn psi_stalls_alone_raise_the_score() {
        let calm = Signals {
            available_pct: 50.0,
            psi_some10: Some(0.0),
            psi_full10: Some(0.0),
            swap_used_pct: None,
        };
        let stalled = Signals {
            psi_some10: Some(30.0),
            psi_full10: Some(8.0),
            ..calm
        };
        assert!(memory_pressure(&stalled) > memory_pressure(&calm) + 30.0);
    }

    #[test]
    fn score_is_monotonic_in_available_memory() {
        let mut last = f64::MAX;
        for pct in (0..=100).step_by(5) {
            let s = Signals {
                available_pct: pct as f64,
                ..Default::default()
            };
            let v = memory_pressure(&s);
            assert!(v <= last + 1e-9);
            last = v;
        }
    }

    #[test]
    fn memory_exhaustion_is_critical_only_after_confirmation() {
        let c = Config::default(); // confirm = 3
        let mut t = MemoryTracker::new(&c);
        let dire = Signals {
            available_pct: 1.0,
            psi_some10: Some(50.0),
            psi_full10: Some(30.0),
            swap_used_pct: Some(95.0),
        };
        assert!(!t.update(&dire).critical);
        assert!(!t.update(&dire).critical);
        assert!(t.update(&dire).critical);
        let calm = Signals {
            available_pct: 80.0,
            ..Default::default()
        };
        let mut still = true;
        for _ in 0..12 {
            still = t.update(&calm).critical;
        }
        assert!(!still, "and it recovers");
    }
}
