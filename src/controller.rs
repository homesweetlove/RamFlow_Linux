//! The feedback controller: decides *how hard* to yield, and whether it is working.
//!
//! The input is a single "demand" number — how much the thing we protect is suffering (the
//! foreground's run-queue wait, or the system CPU stall rate when the foreground is unknown).
//! The output is a level: 0 = leave everything alone, 1..=max = progressively stronger
//! yielding (the engine maps levels to nice values). Three properties matter:
//!
//! * graded: the level rises while the suffering stays high and falls when it is low, so
//!   background work is not starved more than needed;
//! * stable: a dead band between `low` and `high`, and N-in-a-row confirmation, prevent flapping;
//! * honest: if, at the strongest level, the suffering did not drop, the action is withdrawn
//!   ([`Decision::NoEffect`]) instead of keeping a change that does nothing.

use crate::policy::Ema;

#[derive(Debug, Clone)]
pub struct ControllerCfg {
    pub enter: f64,
    pub high: f64,
    pub low: f64,
    /// Engagement is released when the protected side's activity stays below this …
    pub release_below: f64,
    /// … for this many samples.
    pub release_ticks: u32,
    pub confirm: u32,
    pub start_level: u8,
    pub max_level: u8,
    /// Samples to wait at the strongest level before judging whether it helped.
    pub settle_ticks: u32,
    /// Fraction of the engagement-time demand that must have disappeared to count as "helped".
    pub min_gain: f64,
    pub effect_check: bool,
    /// After a withdrawal, do not engage again for this long.
    pub retry_after_ms: u64,
}

/// The foreground must be doing at least this much (percent of one core, run + wait) for its
/// suffering to start an engagement: a wake-up of a few percent is not a user waiting.
const ENGAGE_MIN_ACTIVITY: f64 = 10.0;

/// Below this much culprit CPU the "they gave way" evidence is too weak to trust.
const CULPRIT_MIN_CPU: f64 = 20.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    Hold,
    Engage(u8),
    SetLevel(u8),
    Release,
    NoEffect,
}

#[derive(Debug, Clone, Copy)]
pub struct Input {
    pub now_ms: u64,
    /// How much the protected side suffers, 0..100.
    pub demand: f64,
    /// How active the protected side is at all (CPU use + waiting), 0..100+.
    pub activity: f64,
    /// Is there anything we could yield right now?
    pub have_targets: bool,
    /// CPU the yielded programs use together (percent of one core). Lets the controller see
    /// whether yielding really took CPU away from them.
    pub culprit_cpu: f64,
}

#[derive(Debug, Clone)]
pub struct Controller {
    cfg: ControllerCfg,
    level: u8,
    ema: Ema,
    in_streak: u32,
    /// Sum of the raw demand readings of the current streak.
    in_sum: f64,
    hi_streak: u32,
    lo_streak: u32,
    idle_streak: u32,
    ticks_at_max: u32,
    win_sum: f64,
    judged: bool,
    baseline: f64,
    culprit_ema: Ema,
    culprit_base: f64,
    cooldown_until_ms: u64,
}

impl Controller {
    pub fn new(cfg: ControllerCfg) -> Self {
        Controller {
            cfg,
            level: 0,
            ema: Ema::new(0.5),
            in_streak: 0,
            in_sum: 0.0,
            hi_streak: 0,
            lo_streak: 0,
            idle_streak: 0,
            ticks_at_max: 0,
            win_sum: 0.0,
            judged: false,
            baseline: 0.0,
            culprit_ema: Ema::new(0.5),
            culprit_base: 0.0,
            cooldown_until_ms: 0,
        }
    }

    pub fn level(&self) -> u8 {
        self.level
    }

    pub fn demand(&self) -> f64 {
        self.ema.value().unwrap_or(0.0)
    }

    /// Called on samples where nothing was examined because the system was calm.
    pub fn calm_tick(&mut self) {
        self.ema.update(0.0);
        self.in_streak = 0;
        self.in_sum = 0.0;
    }

    /// Forget the engagement (the engine restored everything for another reason).
    pub fn reset(&mut self) {
        self.level = 0;
        self.in_streak = 0;
        self.in_sum = 0.0;
        self.hi_streak = 0;
        self.lo_streak = 0;
        self.idle_streak = 0;
        self.ticks_at_max = 0;
        self.win_sum = 0.0;
        self.judged = false;
    }

    pub fn step(&mut self, i: &Input) -> Decision {
        let d = self.ema.update(i.demand);
        let cul = self.culprit_ema.update(i.culprit_cpu);
        if self.level == 0 {
            if i.now_ms < self.cooldown_until_ms || !i.have_targets {
                self.in_streak = 0;
                self.in_sum = 0.0;
                return Decision::Hold;
            }
            // Raw samples decide whether the suffering is sustained (a single spike must not
            // count twice just because the smoothed value decays slowly).
            if i.demand >= self.cfg.enter
                && d >= self.cfg.enter * 0.5
                && i.activity >= ENGAGE_MIN_ACTIVITY
            {
                self.in_streak += 1;
                self.in_sum += i.demand;
            } else {
                self.in_streak = 0;
                self.in_sum = 0.0;
            }
            if self.in_streak >= self.cfg.confirm {
                // What the foreground suffered before we did anything: the readings that
                // triggered the engagement (the smoothed value still carries the warm-up).
                let before = self.in_sum / self.in_streak as f64;
                self.reset();
                self.level = self.cfg.start_level.clamp(1, self.cfg.max_level);
                self.baseline = before.max(d);
                self.culprit_base = cul;
                return Decision::Engage(self.level);
            }
            return Decision::Hold;
        }

        // Engaged.
        if !i.have_targets {
            self.reset();
            return Decision::Release;
        }
        if i.activity < self.cfg.release_below {
            self.idle_streak += 1;
        } else {
            self.idle_streak = 0;
        }
        if self.idle_streak >= self.cfg.release_ticks {
            self.reset();
            return Decision::Release;
        }

        if d >= self.cfg.high {
            self.hi_streak += 1;
            self.lo_streak = 0;
        } else if d <= self.cfg.low {
            self.lo_streak += 1;
            self.hi_streak = 0;
        } else {
            self.hi_streak = 0;
            self.lo_streak = 0;
        }

        if self.hi_streak >= self.cfg.confirm && self.level < self.cfg.max_level {
            self.level += 1;
            self.hi_streak = 0;
            self.ticks_at_max = 0;
            self.win_sum = 0.0;
            self.judged = false;
            return Decision::SetLevel(self.level);
        }
        if self.lo_streak >= self.cfg.confirm * 2 && self.level > 1 {
            self.level -= 1;
            self.lo_streak = 0;
            self.ticks_at_max = 0;
            self.win_sum = 0.0;
            self.judged = false;
            return Decision::SetLevel(self.level);
        }

        if self.level == self.cfg.max_level && !self.judged {
            // One verdict per stay at the strongest level, from the average of a whole window
            // of raw readings: a single noisy sample (or a lull at the end of a burst) must
            // not decide, and once yielding has been judged useful it is not judged again.
            self.ticks_at_max += 1;
            self.win_sum += i.demand;
            if self.ticks_at_max >= self.cfg.settle_ticks {
                self.judged = true;
                let mean = self.win_sum / self.ticks_at_max as f64;
                let still_bad = mean >= self.cfg.high;
                let barely_better = mean > self.baseline * (1.0 - self.cfg.min_gain);
                // Yielding clearly takes CPU away from the culprits: they were competing, so
                // the remaining wait comes from elsewhere (a browser's own threads, say).
                let culprits_yielded = self.culprit_base >= CULPRIT_MIN_CPU
                    && cul <= self.culprit_base * (1.0 - self.cfg.min_gain);
                if self.cfg.effect_check && still_bad && barely_better && !culprits_yielded {
                    self.reset();
                    self.cooldown_until_ms = i.now_ms + self.cfg.retry_after_ms;
                    return Decision::NoEffect;
                }
            }
        }
        Decision::Hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ControllerCfg {
        ControllerCfg {
            enter: 20.0,
            high: 35.0,
            low: 10.0,
            release_below: 5.0,
            release_ticks: 4,
            confirm: 2,
            start_level: 3,
            max_level: 4,
            settle_ticks: 5,
            min_gain: 0.3,
            effect_check: true,
            retry_after_ms: 60_000,
        }
    }

    fn inp(now_ms: u64, demand: f64) -> Input {
        Input {
            now_ms,
            demand,
            activity: demand.max(50.0),
            have_targets: true,
            culprit_cpu: 0.0,
        }
    }

    fn run(c: &mut Controller, from: u64, n: u64, demand: f64) -> Vec<Decision> {
        (0..n)
            .map(|k| c.step(&inp(from + k * 1000, demand)))
            .collect()
    }

    #[test]
    fn engages_only_after_confirmation() {
        let mut c = Controller::new(cfg());
        assert_eq!(c.step(&inp(0, 80.0)), Decision::Hold);
        assert_eq!(c.step(&inp(1000, 80.0)), Decision::Engage(3));
        assert_eq!(c.level(), 3);
    }

    #[test]
    fn a_blip_does_not_engage() {
        let mut c = Controller::new(cfg());
        c.step(&inp(0, 80.0));
        for k in 1..8 {
            assert_eq!(c.step(&inp(k * 1000, 0.0)), Decision::Hold);
        }
        assert_eq!(c.level(), 0);
    }

    #[test]
    fn nothing_to_yield_means_no_engagement() {
        let mut c = Controller::new(cfg());
        for k in 0..10 {
            let d = c.step(&Input {
                have_targets: false,
                ..inp(k * 1000, 90.0)
            });
            assert_eq!(d, Decision::Hold);
        }
        assert_eq!(c.level(), 0);
    }

    #[test]
    fn level_rises_while_suffering_stays_high() {
        let mut c = Controller::new(cfg());
        run(&mut c, 0, 2, 80.0); // engage at 3
        let ds = run(&mut c, 2000, 3, 80.0);
        assert!(ds.contains(&Decision::SetLevel(4)), "{ds:?}");
        assert_eq!(c.level(), 4);
    }

    #[test]
    fn level_falls_when_suffering_is_low_but_never_below_one() {
        let mut c = Controller::new(cfg());
        run(&mut c, 0, 2, 80.0);
        let mut seen = vec![];
        for k in 0..40 {
            // activity high enough that it is not an idle release
            let d = c.step(&Input {
                now_ms: 10_000 + k * 1000,
                demand: 2.0,
                activity: 60.0,
                have_targets: true,
                culprit_cpu: 0.0,
            });
            if let Decision::SetLevel(l) = d {
                seen.push(l);
            }
        }
        assert_eq!(seen, vec![2, 1], "steps down one level at a time");
        assert_eq!(c.level(), 1);
    }

    #[test]
    fn dead_band_holds_the_level() {
        let mut c = Controller::new(cfg());
        run(&mut c, 0, 2, 25.0); // engages without ever reaching "high"
        let ds = run(&mut c, 2000, 30, 22.0); // between low (10) and high (35)
        assert!(ds.iter().all(|d| *d == Decision::Hold), "{ds:?}");
        assert_eq!(c.level(), 3);
    }

    #[test]
    fn releases_when_the_protected_side_goes_idle() {
        let mut c = Controller::new(cfg());
        run(&mut c, 0, 2, 80.0);
        let mut released = false;
        for k in 0..10 {
            let d = c.step(&Input {
                now_ms: 5000 + k * 1000,
                demand: 0.0,
                activity: 1.0,
                have_targets: true,
                culprit_cpu: 0.0,
            });
            released |= d == Decision::Release;
        }
        assert!(released);
        assert_eq!(c.level(), 0);
    }

    #[test]
    fn releases_when_targets_disappear() {
        let mut c = Controller::new(cfg());
        run(&mut c, 0, 2, 80.0);
        let d = c.step(&Input {
            have_targets: false,
            ..inp(5000, 80.0)
        });
        assert_eq!(d, Decision::Release);
    }

    #[test]
    fn withdraws_when_the_strongest_level_did_not_help() {
        let mut c = Controller::new(cfg());
        let ds = run(&mut c, 0, 30, 80.0); // suffering never improves
        assert!(ds.contains(&Decision::NoEffect), "{ds:?}");
        assert_eq!(c.level(), 0);
    }

    #[test]
    fn does_not_withdraw_when_it_helped() {
        let mut c = Controller::new(cfg());
        run(&mut c, 0, 2, 80.0); // baseline ≈ 70+
        run(&mut c, 2000, 3, 80.0); // reach level 4
                                    // now it works: suffering falls to the dead band
        let ds = run(&mut c, 5000, 30, 30.0);
        assert!(!ds.contains(&Decision::NoEffect), "{ds:?}");
    }

    #[test]
    fn after_a_withdrawal_it_waits_before_trying_again() {
        let mut c = Controller::new(cfg());
        let mut t = 0;
        while c.step(&inp(t, 80.0)) != Decision::NoEffect {
            t += 1000;
            assert!(t < 100_000);
        }
        // still suffering, but within the retry window: no new engagement
        for k in 1..30 {
            assert_eq!(c.step(&inp(t + k * 1000, 80.0)), Decision::Hold);
        }
        // after the window it may try again
        let mut engaged = false;
        for k in 0..5 {
            engaged |= matches!(
                c.step(&inp(t + 61_000 + k * 1000, 80.0)),
                Decision::Engage(_)
            );
        }
        assert!(engaged);
    }

    #[test]
    fn effect_check_can_be_switched_off_for_dry_runs() {
        let mut c = Controller::new(ControllerCfg {
            effect_check: false,
            ..cfg()
        });
        let ds = run(&mut c, 0, 60, 80.0);
        assert!(!ds.contains(&Decision::NoEffect));
        assert_eq!(c.level(), 4);
    }

    /// Feeds the controller: before engaging the culprits use `before` percent CPU, afterwards
    /// `after`; the foreground keeps suffering at 80.
    fn run_with_culprits(c: &mut Controller, before: f64, after: f64, secs: u64) -> Vec<Decision> {
        let mut out = Vec::new();
        for k in 0..secs {
            let engaged = c.level() > 0;
            out.push(c.step(&Input {
                culprit_cpu: if engaged { after } else { before },
                ..inp(k * 1000, 80.0)
            }));
        }
        out
    }

    #[test]
    fn keeps_yielding_when_the_culprits_clearly_gave_way() {
        // The foreground still waits (its own threads compete), but the yielded programs lost
        // most of their CPU: yielding works, so it is not withdrawn.
        let mut c = Controller::new(cfg());
        let ds = run_with_culprits(&mut c, 190.0, 30.0, 40);
        assert!(!ds.contains(&Decision::NoEffect), "{ds:?}");
        assert_eq!(c.level(), 4);
    }

    #[test]
    fn withdraws_when_the_culprits_kept_their_cpu() {
        let mut c = Controller::new(cfg());
        let ds = run_with_culprits(&mut c, 190.0, 185.0, 40);
        assert!(ds.contains(&Decision::NoEffect), "{ds:?}");
    }

    #[test]
    fn faint_culprit_evidence_does_not_block_withdrawal() {
        let mut c = Controller::new(cfg());
        let ds = run_with_culprits(&mut c, 8.0, 0.0, 40);
        assert!(ds.contains(&Decision::NoEffect), "{ds:?}");
    }

    #[test]
    fn the_baseline_is_what_triggered_the_engagement_not_the_warm_up() {
        // The smoothed value starts from the first (zero) reading and understates the real
        // suffering; judging against it would call a real improvement "no effect".
        let mut c = Controller::new(cfg());
        let mut ds = vec![c.step(&inp(0, 0.0))];
        for k in 1..=3 {
            ds.push(c.step(&inp(k * 1000, 80.0)));
        }
        assert!(
            ds.iter().any(|d| matches!(d, Decision::Engage(_))),
            "{ds:?}"
        );
        // 80 -> 50 is a 37 % improvement (needed: 30 %), although 50 is still above "high".
        let after = run(&mut c, 4000, 30, 50.0);
        assert!(!after.contains(&Decision::NoEffect), "{after:?}");
    }

    #[test]
    fn a_verdict_is_given_once_and_a_later_lull_does_not_undo_it() {
        let mut c = Controller::new(cfg());
        run(&mut c, 0, 3, 80.0);
        // yielding helped (80 -> 40) ...
        let ds = run(&mut c, 3000, 30, 40.0);
        assert!(!ds.contains(&Decision::NoEffect), "{ds:?}");
        // ... so a burst of bad readings afterwards (a different phase of the work) is not
        // judged against the yielding again.
        let ds = run(&mut c, 40_000, 30, 95.0);
        assert!(!ds.contains(&Decision::NoEffect), "{ds:?}");
        assert_eq!(c.level(), 4);
    }

    #[test]
    fn one_noisy_sample_in_the_window_does_not_decide() {
        let mut c = Controller::new(cfg());
        run(&mut c, 0, 3, 80.0);
        let mut ds = run(&mut c, 3000, 8, 36.0); // improving, reaches the top level
        ds.extend(run(&mut c, 11_000, 1, 100.0)); // one spike
        ds.extend(run(&mut c, 12_000, 20, 36.0));
        assert!(!ds.contains(&Decision::NoEffect), "{ds:?}");
    }

    #[test]
    fn a_nearly_idle_foreground_does_not_start_an_engagement() {
        let mut c = Controller::new(cfg());
        let ds: Vec<_> = (0..20)
            .map(|k| {
                c.step(&Input {
                    activity: 7.0,
                    ..inp(k * 1000, 60.0)
                })
            })
            .collect();
        assert!(ds.iter().all(|d| *d == Decision::Hold), "{ds:?}");
        assert_eq!(c.level(), 0);
    }

    #[test]
    fn start_level_is_clamped_to_the_ladder() {
        let mut c = Controller::new(ControllerCfg {
            start_level: 9,
            max_level: 2,
            ..cfg()
        });
        run(&mut c, 0, 1, 80.0);
        assert_eq!(c.step(&inp(1000, 80.0)), Decision::Engage(2));
    }
}
