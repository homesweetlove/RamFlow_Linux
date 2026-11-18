//! The decision engine: one [`Engine::tick`] per sample.
//!
//! What it does, in one paragraph: it watches how much the program you are working in
//! (the foreground) is *made to wait for a CPU*. Only when that wait is high — a victim exists —
//! it looks for culprits: the programs you listed under `yield`, or, in `auto` mode, programs
//! that have been hogging a core. It lowers the culprits' priority one step at a time while the
//! victim keeps suffering, backs off when the suffering is gone, and takes the action back
//! entirely if it did not help.
//!
//! Safety rules, in order of importance:
//! 1. Nothing is changed in dry-run mode.
//! 2. The original value is journaled to disk *before* a change; if that fails, no change is made.
//! 3. The foreground process, everything it started, same-named peers and the built-in protected
//!    set are never lowered; a whole session is only lowered if none of its members is protected.
//! 4. A change is undone as soon as the suffering ends, the action proves useless, the process
//!    becomes the foreground, or the daemon stops — but never over a value somebody else set.

use crate::config::{trunc_comm, Config};
use crate::controller::{Controller, ControllerCfg, Decision, Input};
use crate::journal::{Entry, Journal, Key, Kind};
use crate::policy::{MemoryTracker, Signals};
use crate::proc::{Identity, ProcessSample, Psi};
use crate::provider::Provider;
use crate::sys;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Core system and desktop processes that are never touched, whatever the config says.
pub const DEFAULT_PROTECT: &[&str] = &[
    "systemd",
    "systemd-journal",
    "systemd-logind",
    "systemd-udevd",
    "sshd",
    "dbus-daemon",
    "dbus-broker",
    "NetworkManager",
    "Xorg",
    "Xwayland",
    "gnome-shell",
    "kwin_x11",
    "kwin_wayland",
    "plasmashell",
    "mutter",
    "sway",
    "pipewire",
    "wireplumber",
    "pulseaudio",
    "login",
    "agetty",
    "dockerd",
    "containerd",
    "ramflow",
];

/// A restore that keeps failing is given up after this many attempts.
pub const MAX_RESTORE_FAILURES: u32 = 5;

/// The strongest nice value of each level. Level 1 is a nudge, level 4 the floor.
const LADDER: [i32; 4] = [5, 10, 15, 19];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source {
    /// The foreground program's own wait for a CPU.
    Foreground,
    /// The whole machine's CPU stall rate (used when the foreground is unknown).
    System,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    CriticalStarted {
        memory: f64,
    },
    CriticalEnded,
    Engaged {
        level: u8,
        nice: i32,
        demand: f64,
        source: Source,
    },
    LevelChanged {
        level: u8,
        nice: i32,
        demand: f64,
    },
    Released,
    NoEffect {
        demand: f64,
        retry_secs: u64,
    },
    HogFound {
        pid: i32,
        comm: String,
        cpu: f64,
    },
    Yielded {
        pid: i32,
        comm: String,
        from: i32,
        to: i32,
        group: bool,
    },
    WouldYield {
        pid: i32,
        comm: String,
        from: i32,
        to: i32,
        group: bool,
    },
    Restored {
        pid: i32,
        comm: String,
        nice: i32,
        group: bool,
    },
    RestoreFailed {
        pid: i32,
        comm: String,
        reason: String,
    },
    ApplyFailed {
        pid: i32,
        comm: String,
        reason: String,
    },
    WouldTrim {
        pid: i32,
        comm: String,
        rss: u64,
    },
    Trimmed {
        pid: i32,
        comm: String,
        bytes: u64,
    },
    TrimFailed {
        pid: i32,
        comm: String,
        reason: String,
    },
    JournalError(String),
    SensorError(String),
}

fn scope(group: bool) -> &'static str {
    if group {
        "세션 전체"
    } else {
        "프로세스"
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::fmt::bytes;
        match self {
            Event::CriticalStarted { memory } => {
                write!(f, "메모리 위기 시작 (압박 {memory:.0}/100)")
            }
            Event::CriticalEnded => write!(f, "메모리 위기 끝"),
            Event::Engaged {
                level,
                nice,
                demand,
                source,
            } => {
                let what = match source {
                    Source::Foreground => "앞에 있는 프로그램이 CPU를 기다리는 비율",
                    Source::System => "시스템 CPU 정체율",
                };
                write!(
                    f,
                    "양보 시작: {what} {demand:.0}% → 단계 {level} (우선순위값 {nice})"
                )
            }
            Event::LevelChanged {
                level,
                nice,
                demand,
            } => {
                write!(
                    f,
                    "양보 단계 조절: 단계 {level} (우선순위값 {nice}), 기다림 {demand:.0}%"
                )
            }
            Event::Released => write!(f, "양보 끝"),
            Event::NoEffect { demand, retry_secs } => write!(
                f,
                "효과 없음 (기다림 {demand:.0}%가 줄지 않음) → 모두 원복하고 {retry_secs}초 쉼"
            ),
            Event::HogFound { pid, comm, cpu } => {
                write!(f, "CPU 독차지 발견: {comm}({pid}) 코어의 {cpu:.0}% 사용")
            }
            Event::Yielded {
                pid,
                comm,
                from,
                to,
                group,
            } => {
                write!(
                    f,
                    "양보: {comm}({pid}) {} 우선순위값 {from} → {to}",
                    scope(*group)
                )
            }
            Event::WouldYield {
                pid,
                comm,
                from,
                to,
                group,
            } => write!(
                f,
                "[예행연습] 양보 예정: {comm}({pid}) {} 우선순위값 {from} → {to}",
                scope(*group)
            ),
            Event::Restored {
                pid,
                comm,
                nice,
                group,
            } => {
                write!(
                    f,
                    "원복: {comm}({pid}) {} 우선순위값 {nice}로 되돌림",
                    scope(*group)
                )
            }
            Event::RestoreFailed { pid, comm, reason } => {
                write!(f, "원복 실패: {comm}({pid}) — {reason}")
            }
            Event::ApplyFailed { pid, comm, reason } => {
                write!(f, "양보 실패: {comm}({pid}) — {reason}")
            }
            Event::WouldTrim { pid, comm, rss } => {
                write!(
                    f,
                    "[예행연습] 메모리 정리 예정: {comm}({pid}), 상주 {}",
                    bytes(*rss)
                )
            }
            Event::Trimmed {
                pid,
                comm,
                bytes: b,
            } => {
                write!(f, "메모리 정리: {comm}({pid}) {} 회수 요청", bytes(*b))
            }
            Event::TrimFailed { pid, comm, reason } => {
                write!(f, "메모리 정리 실패: {comm}({pid}) — {reason}")
            }
            Event::JournalError(e) => write!(f, "저널 오류 (안전을 위해 변경하지 않음): {e}"),
            Event::SensorError(e) => write!(f, "측정 오류: {e}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Restored,
    /// The process (or session) exited, or the pid now belongs to a different process.
    Gone,
    /// The value is no longer what we set: somebody else changed it, leave it alone.
    ChangedByUser,
    Failed(String),
}

fn ours(e: &Entry, current: i32) -> bool {
    current == e.applied_nice || current == e.previous_nice
}

/// Undo one journaled change, if (and only if) it is still ours to undo.
pub fn restore_one<P: Provider>(provider: &mut P, e: &Entry) -> Outcome {
    match e.kind {
        Kind::Task => restore_task(provider, e),
        Kind::Group => restore_group(provider, e),
    }
}

fn restore_task<P: Provider>(provider: &mut P, e: &Entry) -> Outcome {
    let pid = e.id.pid;
    match provider.read(pid) {
        None => return Outcome::Gone,
        Some(cur) if cur.start != e.id.start => return Outcome::Gone,
        Some(cur) if !ours(e, cur.nice) => return Outcome::ChangedByUser,
        Some(_) => {}
    }
    let first = provider.restore_nice(pid, e.applied_nice, e.original_nice);
    let second = if e.previous_nice != e.applied_nice {
        provider.restore_nice(pid, e.previous_nice, e.original_nice)
    } else {
        Ok(())
    };
    match provider.read(pid) {
        Some(cur) if cur.start == e.id.start => {
            if cur.nice == e.original_nice {
                Outcome::Restored
            } else {
                let why = first.err().or(second.err()).map(|x| x.to_string());
                Outcome::Failed(why.unwrap_or_else(|| format!("값이 {}로 남아 있음", cur.nice)))
            }
        }
        _ => Outcome::Gone, // it exited while we were working
    }
}

fn restore_group<P: Provider>(provider: &mut P, e: &Entry) -> Outcome {
    // Address the session through the recorded member, or through any other member.
    let mut member = provider
        .read(e.id.pid)
        .filter(|c| c.start == e.id.start && c.autogroup == e.group)
        .map(|c| c.pid);
    if member.is_none() {
        member = provider
            .scan()
            .into_iter()
            .find(|c| c.autogroup == e.group)
            .map(|c| c.pid);
    }
    let Some(pid) = member else {
        return Outcome::Gone;
    };
    match provider.autogroup(pid) {
        Some((g, n)) if g == e.group => {
            if n == e.original_nice || !ours(e, n) {
                return Outcome::ChangedByUser;
            }
        }
        _ => return Outcome::Gone,
    }
    let r = provider.set_autogroup_nice(pid, e.original_nice);
    match provider.autogroup(pid) {
        Some((g, n)) if g == e.group => {
            if n == e.original_nice {
                Outcome::Restored
            } else {
                Outcome::Failed(
                    r.err()
                        .map(|x| x.to_string())
                        .unwrap_or_else(|| format!("값이 {n}로 남아 있음")),
                )
            }
        }
        _ => Outcome::Gone,
    }
}

/// Pids that must be protected because the user is working in them: the foreground process,
/// everything it started, and other processes of the same program (browser helpers, ...).
pub fn protected_set(procs: &[ProcessSample], foreground: Option<i32>) -> HashSet<i32> {
    let mut set = HashSet::new();
    let Some(fg) = foreground else { return set };
    set.insert(fg);
    if let Some(f) = procs.iter().find(|p| p.pid == fg) {
        for p in procs.iter().filter(|p| p.comm == f.comm) {
            set.insert(p.pid);
        }
    }
    loop {
        let before = set.len();
        for p in procs {
            if p.pid > 2 && set.contains(&p.ppid) {
                set.insert(p.pid);
            }
        }
        if set.len() == before {
            break;
        }
    }
    set
}

/// The nice value for a level, never stronger than the configured floor.
pub fn nice_for_level(level: u8, floor: i32) -> i32 {
    LADDER[(level.max(1) as usize - 1).min(LADDER.len() - 1)].min(floor)
}

/// How many levels are actually different from each other under this floor.
fn distinct_levels(floor: i32) -> u8 {
    let mut seen: Vec<i32> = Vec::new();
    for l in 1..=LADDER.len() as u8 {
        let n = nice_for_level(l, floor);
        if !seen.contains(&n) {
            seen.push(n);
        }
    }
    seen.len() as u8
}

fn controller_cfg(cfg: &Config) -> ControllerCfg {
    let max_level = distinct_levels(cfg.nice_target);
    let interval = cfg.interval_ms.max(1);
    ControllerCfg {
        enter: cfg.demand_enter,
        high: cfg.demand_high,
        low: cfg.demand_low,
        release_below: cfg.release_below,
        release_ticks: (cfg.release_secs * 1000 / interval).max(1) as u32,
        confirm: cfg.confirm.max(1),
        start_level: 2.min(max_level),
        max_level,
        settle_ticks: (5000 / interval).max(3) as u32,
        min_gain: 0.3,
        // In dry-run nothing changes, so there is nothing to judge.
        effect_check: cfg.effect_check && !cfg.dry_run,
        retry_after_ms: cfg.retry_after_secs * 1000,
    }
}

fn stall_percent(prev: (u64, u64), now: (u64, u64)) -> f64 {
    let dt_ms = now.1.saturating_sub(prev.1);
    if dt_ms == 0 {
        return 0.0;
    }
    let busy_us = now.0.saturating_sub(prev.0) as f64;
    (busy_us / (dt_ms as f64 * 1000.0) * 100.0).clamp(0.0, 100.0)
}

/// Previous reading of the foreground, to turn counters into rates.
#[derive(Debug, Clone, Copy)]
struct FgPrev {
    id: Identity,
    wait_ns: u64,
    ticks: u64,
    ms: u64,
}

pub struct Engine<P: Provider> {
    cfg: Config,
    provider: P,
    mem: MemoryTracker,
    ctl: Controller,
    journal: Journal,
    announced: HashSet<Key>,
    /// identity -> (cpu ticks last seen, time of the last change in ms)
    activity: HashMap<Identity, (u64, u64)>,
    last_trim: HashMap<Identity, u64>,
    restore_failures: HashMap<Key, u32>,
    prev_psi: Option<(u64, u64)>,
    fg_prev: Option<FgPrev>,
    hog_prev: HashMap<Identity, (u64, u64)>,
    /// identity -> (since ms, last cpu percent of one core)
    hog_since: HashMap<Identity, (u64, f64)>,
    hog_announced: HashSet<Identity>,
    /// Sessions whose nice value could not be written (typically a missing capability):
    /// they are handled per process from then on, instead of failing again every sample.
    group_blocked: HashSet<u64>,
    was_critical: bool,
    me: i32,
}

struct Measure {
    demand: f64,
    activity: f64,
    source: Source,
}

impl<P: Provider> Engine<P> {
    pub fn new(cfg: Config, provider: P, journal: Journal) -> Self {
        let mem = MemoryTracker::new(&cfg);
        let ctl = Controller::new(controller_cfg(&cfg));
        Engine {
            cfg,
            provider,
            mem,
            ctl,
            journal,
            announced: HashSet::new(),
            activity: HashMap::new(),
            last_trim: HashMap::new(),
            restore_failures: HashMap::new(),
            prev_psi: None,
            fg_prev: None,
            hog_prev: HashMap::new(),
            hog_since: HashMap::new(),
            hog_announced: HashSet::new(),
            group_blocked: HashSet::new(),
            was_critical: false,
            me: std::process::id() as i32,
        }
    }

    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    pub fn provider(&self) -> &P {
        &self.provider
    }

    pub fn provider_mut(&mut self) -> &mut P {
        &mut self.provider
    }

    /// The current yielding level (0 = not yielding).
    pub fn level(&self) -> u8 {
        self.ctl.level()
    }

    /// Undo everything still in the journal (crash recovery at start, and at shutdown).
    pub fn restore_everything(&mut self) -> Vec<Event> {
        let mut ev = Vec::new();
        for _ in 0..3 {
            self.restore_entries(&|_| true, &mut ev);
            if self.journal.is_empty() {
                break;
            }
        }
        self.ctl.reset();
        ev
    }

    fn stall_rate(&mut self, psi: Option<&Psi>, cpu_pct: f64, now_ms: u64) -> f64 {
        match psi {
            Some(p) => {
                let cur = (p.some_total_us, now_ms);
                let rate = self
                    .prev_psi
                    .map(|prev| stall_percent(prev, cur))
                    .unwrap_or(0.0);
                self.prev_psi = Some(cur);
                rate
            }
            // No pressure information on this kernel: a saturated CPU is the best proxy.
            None => {
                if cpu_pct >= 90.0 {
                    cpu_pct
                } else {
                    0.0
                }
            }
        }
    }

    pub fn tick(&mut self, now_ms: u64) -> Vec<Event> {
        let mut ev = Vec::new();
        let snap = match self.provider.snapshot() {
            Ok(s) => s,
            Err(e) => {
                ev.push(Event::SensorError(e.to_string()));
                return ev;
            }
        };
        let signals = Signals::from_readings(&snap.mem, snap.mem_psi.as_ref());
        let m = self.mem.update(&signals);
        if m.critical != self.was_critical {
            ev.push(if m.critical {
                Event::CriticalStarted { memory: m.score }
            } else {
                Event::CriticalEnded
            });
            self.was_critical = m.critical;
        }
        let stall = self.stall_rate(snap.cpu_psi.as_ref(), snap.cpu_pct, now_ms);

        let engaged = self.ctl.level() > 0 || !self.journal.is_empty();
        let trim_wanted = m.critical && self.cfg.trim_idle;
        if !engaged && stall < self.cfg.stall_gate && !trim_wanted {
            // Calm and nothing to undo: stay cheap, do not even look at processes.
            self.ctl.calm_tick();
            self.announced.clear();
            self.activity.clear();
            self.fg_prev = None;
            self.hog_prev.clear();
            self.hog_since.clear();
            self.hog_announced.clear();
            return ev;
        }

        let procs = self.provider.scan();
        let foreground = self.provider.foreground();
        let protected = protected_set(&procs, foreground);

        // Housekeeping first: forget what exited, give back what the user switched to.
        let alive: HashSet<Identity> = procs.iter().map(|p| p.identity()).collect();
        let alive_groups: HashSet<u64> = procs
            .iter()
            .map(|p| p.autogroup)
            .filter(|g| *g != 0)
            .collect();
        let protected_groups: HashSet<u64> = procs
            .iter()
            .filter(|p| p.autogroup != 0 && protected.contains(&p.pid))
            .map(|p| p.autogroup)
            .collect();
        self.restore_entries(
            &|e| match e.kind {
                Kind::Task => !alive.contains(&e.id) || protected.contains(&e.id.pid),
                Kind::Group => {
                    !alive_groups.contains(&e.group) || protected_groups.contains(&e.group)
                }
            },
            &mut ev,
        );

        let targets =
            self.collect_targets(&procs, &protected, foreground.is_some(), now_ms, &mut ev);
        let measure = self.measure(&procs, foreground, stall, snap.cpu_pct, now_ms);

        if self.ctl.level() == 0 && !self.journal.is_empty() {
            // Leftovers whose restore failed earlier: keep trying.
            self.restore_entries(&|_| true, &mut ev);
        }

        let decision = self.ctl.step(&Input {
            now_ms,
            demand: measure.demand,
            activity: measure.activity,
            have_targets: !targets.is_empty() || !self.journal.is_empty(),
        });
        let demand = self.ctl.demand();
        match decision {
            Decision::Engage(level) => {
                ev.push(Event::Engaged {
                    level,
                    nice: nice_for_level(level, self.cfg.nice_target),
                    demand,
                    source: measure.source,
                });
                self.apply_level(level, true, &targets, &procs, &protected, &mut ev);
            }
            Decision::SetLevel(level) => {
                ev.push(Event::LevelChanged {
                    level,
                    nice: nice_for_level(level, self.cfg.nice_target),
                    demand,
                });
                self.apply_level(level, true, &targets, &procs, &protected, &mut ev);
            }
            Decision::Hold => {
                if self.ctl.level() > 0 {
                    // New culprits that appeared while we are already yielding.
                    let level = self.ctl.level();
                    self.apply_level(level, false, &targets, &procs, &protected, &mut ev);
                }
            }
            Decision::Release => {
                self.restore_entries(&|_| true, &mut ev);
                self.announced.clear();
                ev.push(Event::Released);
            }
            Decision::NoEffect => {
                self.restore_entries(&|_| true, &mut ev);
                self.announced.clear();
                ev.push(Event::NoEffect {
                    demand,
                    retry_secs: self.cfg.retry_after_secs,
                });
            }
        }

        if trim_wanted {
            self.update_activity(&procs, now_ms);
            if foreground.is_some() {
                self.trim_idle(&procs, &protected, now_ms, &mut ev);
            }
        }
        ev
    }

    /// How much the protected side suffers, and how active it is.
    fn measure(
        &mut self,
        procs: &[ProcessSample],
        foreground: Option<i32>,
        stall: f64,
        cpu_pct: f64,
        now_ms: u64,
    ) -> Measure {
        let fallback = Measure {
            demand: stall,
            activity: cpu_pct,
            source: Source::System,
        };
        let Some(fg) = foreground.and_then(|pid| procs.iter().find(|p| p.pid == pid)) else {
            self.fg_prev = None;
            return fallback;
        };
        let Some(wait_ns) = self.provider.wait_ns(fg.pid) else {
            self.fg_prev = None;
            return fallback;
        };
        let cur = FgPrev {
            id: fg.identity(),
            wait_ns,
            ticks: fg.ticks,
            ms: now_ms,
        };
        let prev = self.fg_prev.replace(cur);
        let Some(prev) = prev.filter(|p| p.id == cur.id && cur.ms > p.ms) else {
            // The foreground is known but this is its first reading: there is no rate yet.
            // Do not fall back to the machine-wide number — that would blame the culprits
            // without a victim.
            return Measure {
                demand: 0.0,
                activity: cpu_pct,
                source: Source::Foreground,
            };
        };
        let dt_ms = (cur.ms - prev.ms) as f64;
        let wait = (cur.wait_ns.saturating_sub(prev.wait_ns) as f64) / (dt_ms * 1e6) * 100.0;
        let run = (cur.ticks.saturating_sub(prev.ticks) as f64)
            / (sys::clock_ticks().max(1) as f64)
            * 1000.0
            / dt_ms
            * 100.0;
        let activity = wait + run;
        // The slowdown the foreground experiences: the share of its wanted CPU time it spent
        // waiting. Idle foregrounds suffer nothing, whatever the machine is doing.
        let demand = if activity >= 5.0 {
            (wait / activity * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        Measure {
            demand,
            activity,
            source: Source::Foreground,
        }
    }

    fn never_touch(&self, s: &ProcessSample) -> bool {
        s.pid <= 1
            || s.pid == self.me
            || s.is_kernel_thread()
            || s.state == 'Z'
            || s.state == 'X'
            || DEFAULT_PROTECT.iter().any(|n| trunc_comm(n) == s.comm)
            || self
                .cfg
                .protect
                .iter()
                .any(|m| m.matches(&s.comm, || self.provider.exe(s.pid)))
    }

    fn is_yield_target(&self, s: &ProcessSample) -> bool {
        self.cfg
            .yield_list
            .iter()
            .any(|m| m.matches(&s.comm, || self.provider.exe(s.pid)))
    }

    /// The culprits: processes the user listed, plus (in `auto` mode, with a known foreground)
    /// processes that have been hogging a core for a while.
    fn collect_targets(
        &mut self,
        procs: &[ProcessSample],
        protected: &HashSet<i32>,
        fg_known: bool,
        now_ms: u64,
        ev: &mut Vec<Event>,
    ) -> Vec<ProcessSample> {
        let clk = sys::clock_ticks().max(1) as f64;
        let interval = self.cfg.interval_ms.max(1);
        let hog_ms = self.cfg.auto_hog_secs * 1000;
        let mut targets = Vec::new();
        let mut seen = HashSet::new();
        for s in procs {
            if protected.contains(&s.pid) || self.never_touch(s) {
                continue;
            }
            let id = s.identity();
            if self.cfg.auto && fg_known && s.uid >= self.cfg.auto_min_uid {
                seen.insert(id);
                let prev = self.hog_prev.insert(id, (s.ticks, now_ms));
                match prev {
                    Some((pt, pms)) if now_ms > pms && now_ms - pms <= interval * 4 => {
                        let dt = (now_ms - pms) as f64;
                        let rate = s.ticks.saturating_sub(pt) as f64 / clk * 1000.0 / dt * 100.0;
                        if rate >= self.cfg.auto_hog_cpu {
                            let since = self.hog_since.get(&id).map(|x| x.0).unwrap_or(now_ms);
                            self.hog_since.insert(id, (since, rate));
                        } else {
                            self.hog_since.remove(&id);
                        }
                    }
                    _ => {
                        self.hog_since.remove(&id);
                    }
                }
            }
            let hog = self
                .hog_since
                .get(&id)
                .filter(|(since, _)| now_ms.saturating_sub(*since) >= hog_ms);
            if let Some(&(_, rate)) = hog {
                if self.hog_announced.insert(id) {
                    ev.push(Event::HogFound {
                        pid: s.pid,
                        comm: s.comm.clone(),
                        cpu: rate,
                    });
                }
            }
            if hog.is_some() || self.is_yield_target(s) {
                targets.push(s.clone());
            }
        }
        self.hog_prev.retain(|k, _| seen.contains(k));
        self.hog_since.retain(|k, _| seen.contains(k));
        self.hog_announced.retain(|k| seen.contains(k));
        targets
    }

    /// May the whole session (scheduler autogroup) `g` be lowered? Only if no member of it is
    /// something we must not slow down.
    fn group_is_safe(&self, g: u64, procs: &[ProcessSample], protected: &HashSet<i32>) -> bool {
        procs
            .iter()
            .filter(|p| p.autogroup == g)
            .all(|p| !protected.contains(&p.pid) && !self.never_touch(p))
    }

    /// Bring the culprits to the nice value of `level`. Unless `change_existing` is set,
    /// culprits that are already lowered are left exactly as they are (so a value set by hand
    /// is not overwritten).
    fn apply_level(
        &mut self,
        level: u8,
        change_existing: bool,
        targets: &[ProcessSample],
        procs: &[ProcessSample],
        protected: &HashSet<i32>,
        ev: &mut Vec<Event>,
    ) {
        let to = nice_for_level(level, self.cfg.nice_target);
        let mut handled_groups = HashSet::new();
        let mut group_safety: HashMap<u64, bool> = HashMap::new();
        for s in targets {
            let use_group = self.cfg.use_autogroup
                && s.autogroup != 0
                && !self.group_blocked.contains(&s.autogroup)
                && {
                    let g = s.autogroup;
                    let safe = match group_safety.get(&g) {
                        Some(v) => *v,
                        None => {
                            let v = self.group_is_safe(g, procs, protected);
                            group_safety.insert(g, v);
                            v
                        }
                    };
                    safe && self.provider.autogroup(s.pid).is_some()
                };
            if use_group {
                if !handled_groups.insert(s.autogroup) {
                    continue;
                }
                let original = self.provider.autogroup(s.pid).map(|(_, n)| n).unwrap_or(0);
                let fresh = !self.journal.contains(Key::Group(s.autogroup));
                let done = self.lower(
                    Key::Group(s.autogroup),
                    s,
                    original,
                    to,
                    change_existing,
                    ev,
                );
                if !done && fresh {
                    // The session could not be changed: do what is still possible, per process.
                    self.group_blocked.insert(s.autogroup);
                    self.lower(Key::Task(s.identity()), s, s.nice, to, change_existing, ev);
                }
            } else {
                self.lower(Key::Task(s.identity()), s, s.nice, to, change_existing, ev);
            }
        }
    }

    /// Lower (or re-level) one task or one session, journal first.
    fn lower(
        &mut self,
        key: Key,
        s: &ProcessSample,
        current: i32,
        to: i32,
        change_existing: bool,
        ev: &mut Vec<Event>,
    ) -> bool {
        let group = matches!(key, Key::Group(_));
        let existing = self.journal.get(key).cloned();
        if let Some(old) = &existing {
            if !change_existing || old.applied_nice == to {
                return true;
            }
            if to <= old.original_nice {
                // The ladder came back down to (or below) where it started: just undo.
                let k = key;
                self.restore_entries(&|e| e.key() == k, ev);
                return true;
            }
        } else if current >= to {
            return true; // already at least that low
        }
        let from = existing
            .as_ref()
            .map(|e| e.original_nice)
            .unwrap_or(current);

        if self.cfg.dry_run {
            if self.announced.insert(key) {
                ev.push(Event::WouldYield {
                    pid: s.pid,
                    comm: s.comm.clone(),
                    from,
                    to,
                    group,
                });
            }
            return true;
        }

        // 1) Make the undo possible first. If it cannot be recorded, do not change anything.
        let mut entry = match &existing {
            Some(old) => Entry {
                id: s.identity(),
                previous_nice: old.applied_nice,
                applied_nice: to,
                ..old.clone()
            },
            None => match key {
                Key::Group(g) => Entry::group(s.identity(), &s.comm, g, from, to),
                Key::Task(_) => Entry::task(s.identity(), &s.comm, from, to),
            },
        };
        if existing.is_none() {
            entry.previous_nice = to;
        }
        self.journal.record(entry.clone());
        if let Err(e) = self.journal.save() {
            self.rollback_entry(key, &existing);
            ev.push(Event::JournalError(e.to_string()));
            return false;
        }
        // 2) Re-check identity right before acting, then 3) change.
        let still_ours = matches!(self.provider.read(s.pid), Some(c) if c.start == s.start);
        let result = if !still_ours {
            Err("프로세스가 그 사이에 바뀌었습니다".to_string())
        } else if group {
            self.provider
                .set_autogroup_nice(s.pid, to)
                .map_err(|e| e.to_string())
        } else {
            self.provider.set_nice(s.pid, to).map_err(|e| e.to_string())
        };
        match result {
            Ok(()) => {
                // The change is complete: the "in flight" value is now the same as the applied one.
                entry.previous_nice = to;
                self.journal.record(entry);
                if let Err(e) = self.journal.save() {
                    ev.push(Event::JournalError(e.to_string()));
                }
                self.restore_failures.remove(&key);
                ev.push(Event::Yielded {
                    pid: s.pid,
                    comm: s.comm.clone(),
                    from,
                    to,
                    group,
                });
                true
            }
            Err(reason) => {
                // Undo whatever part may have been applied, then go back to the old journal state.
                if still_ours {
                    let _ = match (&existing, group) {
                        (Some(old), true) => {
                            self.provider.set_autogroup_nice(s.pid, old.applied_nice)
                        }
                        (Some(old), false) => self.provider.set_nice(s.pid, old.applied_nice),
                        (None, true) => self.provider.set_autogroup_nice(s.pid, from),
                        (None, false) => self.provider.restore_nice(s.pid, to, from),
                    };
                }
                self.rollback_entry(key, &existing);
                if let Err(e) = self.journal.save() {
                    ev.push(Event::JournalError(e.to_string()));
                }
                ev.push(Event::ApplyFailed {
                    pid: s.pid,
                    comm: s.comm.clone(),
                    reason,
                });
                false
            }
        }
    }

    fn rollback_entry(&mut self, key: Key, old: &Option<Entry>) {
        match old {
            Some(e) => self.journal.record(e.clone()),
            None => self.journal.remove(key),
        }
    }

    fn restore_entries(&mut self, filter: &dyn Fn(&Entry) -> bool, ev: &mut Vec<Event>) {
        let todo: Vec<Entry> = self
            .journal
            .entries()
            .iter()
            .filter(|e| filter(e))
            .cloned()
            .collect();
        if todo.is_empty() {
            return;
        }
        for e in todo {
            let key = e.key();
            let group = e.kind == Kind::Group;
            match restore_one(&mut self.provider, &e) {
                Outcome::Restored => {
                    self.restore_failures.remove(&key);
                    self.journal.remove(key);
                    self.announced.remove(&key);
                    ev.push(Event::Restored {
                        pid: e.id.pid,
                        comm: e.comm.clone(),
                        nice: e.original_nice,
                        group,
                    });
                }
                Outcome::Gone | Outcome::ChangedByUser => {
                    self.restore_failures.remove(&key);
                    self.journal.remove(key);
                    self.announced.remove(&key);
                }
                Outcome::Failed(reason) => {
                    let n = self.restore_failures.entry(key).or_insert(0);
                    *n += 1;
                    let give_up = *n >= MAX_RESTORE_FAILURES;
                    ev.push(Event::RestoreFailed {
                        pid: e.id.pid,
                        comm: e.comm.clone(),
                        reason,
                    });
                    if give_up {
                        self.restore_failures.remove(&key);
                        self.journal.remove(key);
                    }
                }
            }
        }
        if let Err(err) = self.journal.save() {
            ev.push(Event::JournalError(err.to_string()));
        }
        if self.journal.is_empty() {
            self.ctl.reset();
        }
    }

    fn update_activity(&mut self, procs: &[ProcessSample], now_ms: u64) {
        let min_rss = self.cfg.trim_min_rss_mib * 1024 * 1024;
        let mut live = HashSet::new();
        for s in procs.iter().filter(|s| s.rss >= min_rss) {
            let id = s.identity();
            live.insert(id);
            let slot = self.activity.entry(id).or_insert((s.ticks, now_ms));
            if slot.0 != s.ticks {
                *slot = (s.ticks, now_ms);
            }
        }
        self.activity.retain(|k, _| live.contains(k));
        self.last_trim
            .retain(|k, _| live.contains(k) || self.activity.contains_key(k));
    }

    fn trim_idle(
        &mut self,
        procs: &[ProcessSample],
        protected: &HashSet<i32>,
        now_ms: u64,
        ev: &mut Vec<Event>,
    ) {
        let min_rss = self.cfg.trim_min_rss_mib * 1024 * 1024;
        let idle_ms = self.cfg.idle_secs * 1000;
        let cooldown_ms = self.cfg.trim_cooldown_secs * 1000;
        let mut candidates: Vec<&ProcessSample> = procs
            .iter()
            .filter(|s| {
                s.uid >= self.cfg.trim_min_uid
                    && s.rss >= min_rss
                    && !protected.contains(&s.pid)
                    && !self.never_touch(s)
                    && self
                        .activity
                        .get(&s.identity())
                        .map(|&(_, since)| now_ms.saturating_sub(since) >= idle_ms)
                        .unwrap_or(false)
                    && self
                        .last_trim
                        .get(&s.identity())
                        .map(|&t| now_ms.saturating_sub(t) >= cooldown_ms)
                        .unwrap_or(true)
            })
            .collect();
        candidates.sort_by_key(|s| std::cmp::Reverse(s.rss));
        // One process per sample: gentle, and the next sample re-measures the pressure.
        let Some(s) = candidates.first() else { return };
        let id = s.identity();
        self.last_trim.insert(id, now_ms);
        if self.cfg.dry_run {
            ev.push(Event::WouldTrim {
                pid: s.pid,
                comm: s.comm.clone(),
                rss: s.rss,
            });
            return;
        }
        match self.provider.page_out(s.pid) {
            Ok(bytes) => ev.push(Event::Trimmed {
                pid: s.pid,
                comm: s.comm.clone(),
                bytes,
            }),
            Err(e) => ev.push(Event::TrimFailed {
                pid: s.pid,
                comm: s.comm.clone(),
                reason: e.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Matcher;
    use crate::proc::MemInfo;
    use crate::provider::Snapshot;
    use std::io;

    const MIB: u64 = 1024 * 1024;
    const FG: i32 = 500;

    struct Mock {
        snap: Snapshot,
        procs: Vec<ProcessSample>,
        exes: HashMap<i32, String>,
        fg: Option<i32>,
        group_nice: HashMap<u64, i32>,
        waits: HashMap<i32, u64>,
        fail_set: bool,
        fail_group_set: bool,
        fail_restore: bool,
        set_calls: Vec<(i32, i32)>,
        group_calls: Vec<(u64, i32)>,
        paged: Vec<i32>,
    }

    fn calm() -> Snapshot {
        Snapshot {
            mem: MemInfo {
                total: 8192 * MIB,
                available: 6000 * MIB,
                swap_total: 0,
                swap_free: 0,
            },
            mem_psi: None,
            cpu_psi: Some(Psi {
                some_avg10: 0.0,
                some_avg60: 0.0,
                full_avg10: 0.0,
                full_avg60: 0.0,
                some_total_us: 0,
            }),
            cpu_pct: 5.0,
        }
    }

    fn starving_memory() -> Snapshot {
        Snapshot {
            mem: MemInfo {
                total: 8192 * MIB,
                available: 80 * MIB,
                swap_total: 4096 * MIB,
                swap_free: 100 * MIB,
            },
            mem_psi: Some(Psi {
                some_avg10: 60.0,
                some_avg60: 40.0,
                full_avg10: 30.0,
                full_avg60: 20.0,
                some_total_us: 0,
            }),
            ..calm()
        }
    }

    fn proc_(pid: i32, comm: &str, nice: i32, group: u64) -> ProcessSample {
        ProcessSample {
            pid,
            ppid: 1000,
            comm: comm.into(),
            state: 'S',
            nice,
            start: 5000 + pid as u64,
            ticks: 10,
            rss: 10 * MIB,
            uid: 1000,
            autogroup: group,
        }
    }

    impl Mock {
        fn new(procs: Vec<ProcessSample>) -> Self {
            let exes = procs
                .iter()
                .map(|p| (p.pid, format!("/usr/bin/{}", p.comm)))
                .collect();
            let mut group_nice = HashMap::new();
            for p in &procs {
                if p.autogroup != 0 {
                    group_nice.entry(p.autogroup).or_insert(0);
                }
            }
            Mock {
                snap: calm(),
                procs,
                exes,
                fg: None,
                group_nice,
                waits: HashMap::new(),
                fail_set: false,
                fail_group_set: false,
                fail_restore: false,
                set_calls: vec![],
                group_calls: vec![],
                paged: vec![],
            }
        }
        fn by(&self, pid: i32) -> &ProcessSample {
            self.procs.iter().find(|p| p.pid == pid).unwrap()
        }
        fn gn(&self, g: u64) -> i32 {
            self.group_nice[&g]
        }
    }

    impl Provider for Mock {
        fn snapshot(&mut self) -> io::Result<Snapshot> {
            Ok(self.snap.clone())
        }
        fn scan(&mut self) -> Vec<ProcessSample> {
            self.procs.clone()
        }
        fn read(&self, pid: i32) -> Option<ProcessSample> {
            self.procs.iter().find(|p| p.pid == pid).cloned()
        }
        fn exe(&self, pid: i32) -> Option<String> {
            self.exes.get(&pid).cloned()
        }
        fn foreground(&mut self) -> Option<i32> {
            self.fg
        }
        fn set_nice(&mut self, pid: i32, nice: i32) -> io::Result<()> {
            self.set_calls.push((pid, nice));
            if self.fail_set {
                return Err(io::Error::from_raw_os_error(1));
            }
            if let Some(p) = self.procs.iter_mut().find(|p| p.pid == pid) {
                p.nice = nice;
            }
            Ok(())
        }
        fn restore_nice(&mut self, pid: i32, applied: i32, original: i32) -> io::Result<()> {
            if self.fail_restore {
                return Err(io::Error::from_raw_os_error(1));
            }
            if let Some(p) = self.procs.iter_mut().find(|p| p.pid == pid) {
                if p.nice == applied {
                    p.nice = original;
                }
            }
            Ok(())
        }
        fn autogroup(&self, pid: i32) -> Option<(u64, i32)> {
            let g = self.procs.iter().find(|p| p.pid == pid)?.autogroup;
            (g != 0).then(|| (g, self.group_nice[&g]))
        }
        fn set_autogroup_nice(&mut self, pid: i32, nice: i32) -> io::Result<()> {
            let g = self
                .procs
                .iter()
                .find(|p| p.pid == pid)
                .map(|p| p.autogroup)
                .unwrap_or(0);
            self.group_calls.push((g, nice));
            if self.fail_group_set || self.fail_restore && nice <= 0 {
                return Err(io::Error::from_raw_os_error(1));
            }
            self.group_nice.insert(g, nice);
            Ok(())
        }
        fn wait_ns(&self, pid: i32) -> Option<u64> {
            Some(*self.waits.get(&pid).unwrap_or(&0))
        }
        fn page_out(&mut self, pid: i32) -> io::Result<u64> {
            self.paged.push(pid);
            Ok(50 * MIB)
        }
        fn boot_id(&self) -> String {
            "test-boot".into()
        }
    }

    fn cfg() -> Config {
        Config {
            dry_run: false,
            confirm: 1,
            interval_ms: 1000,
            release_secs: 3,
            yield_list: vec![Matcher::Path("/usr/bin/ffmpeg".into())],
            ..Config::default()
        }
    }

    fn engine(cfg: Config, mock: Mock) -> Engine<Mock> {
        Engine::new(cfg, mock, Journal::in_memory("test-boot"))
    }

    fn has<F: Fn(&Event) -> bool>(ev: &[Event], f: F) -> bool {
        ev.iter().any(f)
    }

    /// One second passes. The foreground wanted `wait + run` percent of a core and spent `wait`
    /// of it waiting; the machine's CPU stall counter grew by `stall` percent.
    fn second(e: &mut Engine<Mock>, t: &mut u64, wait: f64, run: f64, stall: f64) -> Vec<Event> {
        *t += 1000;
        let clk = sys::clock_ticks() as f64;
        let m = e.provider_mut();
        *m.waits.entry(FG).or_insert(0) += (wait / 100.0 * 1e9) as u64;
        if let Some(p) = m.procs.iter_mut().find(|p| p.pid == FG) {
            p.ticks += (run / 100.0 * clk).round() as u64;
        }
        if let Some(p) = m.snap.cpu_psi.as_mut() {
            p.some_total_us += (stall / 100.0 * 1_000_000.0) as u64;
        }
        e.tick(*t)
    }

    fn run_seconds(
        e: &mut Engine<Mock>,
        t: &mut u64,
        n: u32,
        wait: f64,
        run: f64,
        stall: f64,
    ) -> Vec<Event> {
        (0..n)
            .flat_map(|_| second(e, t, wait, run, stall))
            .collect()
    }

    /// A foreground (own session 1) and a listed culprit; `group` is the culprit's session.
    fn scene(group: u64) -> Mock {
        let mut m = Mock::new(vec![
            proc_(FG, "terminal", 0, 1),
            proc_(10, "ffmpeg", 0, group),
        ]);
        m.fg = Some(FG);
        m
    }

    #[test]
    fn nice_ladder_respects_the_floor() {
        assert_eq!(nice_for_level(1, 19), 5);
        assert_eq!(nice_for_level(4, 19), 19);
        assert_eq!(nice_for_level(4, 12), 12);
        assert_eq!(nice_for_level(2, 3), 3);
        assert_eq!(distinct_levels(19), 4);
        assert_eq!(distinct_levels(12), 3);
        assert_eq!(distinct_levels(5), 1);
    }

    #[test]
    fn a_calm_machine_is_left_alone_without_scanning() {
        let mut e = engine(cfg(), scene(2));
        let mut t = 0;
        for _ in 0..6 {
            assert!(second(&mut e, &mut t, 0.0, 10.0, 0.0).is_empty());
        }
        assert!(e.provider().set_calls.is_empty());
        assert!(e.provider().group_calls.is_empty());
    }

    #[test]
    fn a_busy_machine_without_a_victim_is_left_alone() {
        // The CPU is saturated and ffmpeg runs, but the foreground waits for nothing:
        // there is no victim, so there is no reason to touch anyone.
        let mut e = engine(cfg(), scene(2));
        let mut t = 0;
        let ev = run_seconds(&mut e, &mut t, 12, 1.0, 40.0, 60.0);
        assert!(!has(&ev, |x| matches!(x, Event::Engaged { .. })), "{ev:?}");
        assert!(e.provider().set_calls.is_empty() && e.provider().group_calls.is_empty());
        assert_eq!(e.level(), 0);
    }

    #[test]
    fn dry_run_reports_but_never_changes_anything() {
        let mut e = engine(
            Config {
                dry_run: true,
                ..cfg()
            },
            scene(2),
        );
        let mut t = 0;
        let ev = run_seconds(&mut e, &mut t, 6, 60.0, 40.0, 60.0);
        assert!(
            has(&ev, |x| matches!(x, Event::Engaged { level: 2, .. })),
            "{ev:?}"
        );
        let would: Vec<_> = ev
            .iter()
            .filter(|x| matches!(x, Event::WouldYield { .. }))
            .collect();
        assert_eq!(would.len(), 1, "announced once per culprit: {ev:?}");
        assert!(e.provider().set_calls.is_empty() && e.provider().group_calls.is_empty());
        assert!(e.journal().is_empty());
        assert_eq!(e.provider().by(10).nice, 0);
        assert!(
            !has(&ev, |x| matches!(x, Event::NoEffect { .. })),
            "nothing to judge in a dry run"
        );
    }

    #[test]
    fn a_victim_makes_it_lower_the_listed_culprit_in_the_same_session() {
        let mut m = scene(1); // same session as the foreground: only per-process nice is possible
        m.group_nice.insert(1, 0);
        let mut e = engine(cfg(), m);
        let mut t = 0;
        let ev = run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        assert!(
            has(&ev, |x| matches!(
                x,
                Event::Engaged {
                    source: Source::Foreground,
                    ..
                }
            )),
            "{ev:?}"
        );
        assert!(
            has(&ev, |x| matches!(
                x,
                Event::Yielded {
                    pid: 10,
                    from: 0,
                    group: false,
                    ..
                }
            )),
            "{ev:?}"
        );
        assert_eq!(e.provider().by(10).nice, 10, "first step is level 2");
        assert!(
            e.provider().group_calls.is_empty(),
            "the foreground's own session must not be touched"
        );
        assert!(e
            .journal()
            .contains(Key::Task(e.provider().by(10).identity())));
    }

    #[test]
    fn a_culprit_in_another_session_is_lowered_as_a_whole_session() {
        let mut e = engine(cfg(), scene(2));
        let mut t = 0;
        let ev = run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        assert!(
            has(&ev, |x| matches!(
                x,
                Event::Yielded {
                    pid: 10,
                    group: true,
                    ..
                }
            )),
            "{ev:?}"
        );
        assert_eq!(e.provider().gn(2), 10);
        assert_eq!(
            e.provider().gn(1),
            0,
            "the foreground's session is untouched"
        );
        assert!(
            e.provider().set_calls.is_empty(),
            "per-process nice is not needed then"
        );
        assert!(e.journal().contains(Key::Group(2)));
    }

    #[test]
    fn the_session_is_not_lowered_when_a_protected_program_lives_in_it() {
        // sshd shares the culprit's session: slowing the session would slow sshd. Fall back.
        let mut m = scene(2);
        m.procs.push(proc_(77, "sshd", 0, 2));
        let mut e = engine(cfg(), m);
        let mut t = 0;
        run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        assert!(e.provider().group_calls.is_empty());
        assert_eq!(e.provider().by(10).nice, 10);
        assert_eq!(e.provider().by(77).nice, 0);
    }

    #[test]
    fn autogroup_use_can_be_switched_off() {
        let mut e = engine(
            Config {
                use_autogroup: false,
                ..cfg()
            },
            scene(2),
        );
        let mut t = 0;
        run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        assert!(e.provider().group_calls.is_empty());
        assert_eq!(e.provider().by(10).nice, 10);
    }

    #[test]
    fn the_level_rises_while_the_victim_keeps_suffering() {
        let mut e = engine(
            Config {
                effect_check: false,
                ..cfg()
            },
            scene(2),
        );
        let mut t = 0;
        let ev = run_seconds(&mut e, &mut t, 6, 60.0, 40.0, 60.0);
        let levels: Vec<u8> = ev
            .iter()
            .filter_map(|x| match x {
                Event::Engaged { level, .. } | Event::LevelChanged { level, .. } => Some(*level),
                _ => None,
            })
            .collect();
        assert_eq!(levels, vec![2, 3, 4], "{ev:?}");
        assert_eq!(e.provider().gn(2), 19);
    }

    #[test]
    fn the_level_falls_back_when_the_suffering_is_gone_but_the_foreground_is_active() {
        let mut e = engine(
            Config {
                effect_check: false,
                ..cfg()
            },
            scene(2),
        );
        let mut t = 0;
        run_seconds(&mut e, &mut t, 5, 60.0, 40.0, 60.0);
        assert_eq!(e.provider().gn(2), 19);
        // It works: the foreground now runs smoothly (still busy, no waiting).
        let ev = run_seconds(&mut e, &mut t, 14, 1.0, 60.0, 30.0);
        assert!(
            has(&ev, |x| matches!(x, Event::LevelChanged { level: 3, .. })),
            "{ev:?}"
        );
        assert!(
            has(&ev, |x| matches!(x, Event::LevelChanged { level: 1, .. })),
            "{ev:?}"
        );
        assert_eq!(
            e.provider().gn(2),
            5,
            "background gets the most it can while the foreground is fine"
        );
        assert!(e.level() >= 1);
    }

    #[test]
    fn everything_is_given_back_when_the_foreground_goes_idle() {
        let mut e = engine(cfg(), scene(2));
        let mut t = 0;
        run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        assert_eq!(e.provider().gn(2), 10);
        let ev = run_seconds(&mut e, &mut t, 6, 0.0, 0.5, 5.0);
        assert!(has(&ev, |x| matches!(x, Event::Released)), "{ev:?}");
        assert!(
            has(&ev, |x| matches!(
                x,
                Event::Restored {
                    group: true,
                    nice: 0,
                    ..
                }
            )),
            "{ev:?}"
        );
        assert_eq!(e.provider().gn(2), 0);
        assert!(e.journal().is_empty());
        assert_eq!(e.level(), 0);
    }

    #[test]
    fn it_withdraws_and_says_so_when_lowering_did_not_help() {
        // The foreground keeps waiting no matter what: the culprit was not the cause.
        let mut e = engine(cfg(), scene(2));
        let mut t = 0;
        let ev = run_seconds(&mut e, &mut t, 20, 60.0, 40.0, 60.0);
        assert!(has(&ev, |x| matches!(x, Event::NoEffect { .. })), "{ev:?}");
        assert_eq!(e.provider().gn(2), 0, "withdrawn: back to the original");
        assert!(e.journal().is_empty());
        // And it does not immediately try again.
        let after = run_seconds(&mut e, &mut t, 10, 60.0, 40.0, 60.0);
        assert!(
            !has(&after, |x| matches!(x, Event::Engaged { .. })),
            "{after:?}"
        );
    }

    #[test]
    fn it_keeps_going_when_lowering_does_help() {
        let mut e = engine(cfg(), scene(2));
        let mut t = 0;
        run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        let ev = run_seconds(&mut e, &mut t, 20, 25.0, 60.0, 30.0); // improved, in the dead band
        assert!(!has(&ev, |x| matches!(x, Event::NoEffect { .. })), "{ev:?}");
        assert!(e.provider().gn(2) > 0);
    }

    #[test]
    fn switching_to_a_lowered_program_gives_it_back_immediately() {
        let mut m = scene(2);
        m.procs.push(proc_(20, "game", 0, 9));
        let mut e = engine(
            Config {
                use_autogroup: false,
                ..cfg()
            },
            m,
        );
        let mut t = 0;
        run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        assert_eq!(e.provider().by(10).nice, 10);
        e.provider_mut().fg = Some(10); // the user now works in ffmpeg itself
        let ev = second(&mut e, &mut t, 60.0, 40.0, 60.0);
        assert!(
            has(&ev, |x| matches!(x, Event::Restored { pid: 10, .. })),
            "{ev:?}"
        );
        assert_eq!(e.provider().by(10).nice, 0);
    }

    #[test]
    fn switching_into_a_lowered_session_gives_the_session_back() {
        let mut m = scene(2);
        m.procs.push(proc_(30, "editor", 0, 2)); // shares the session with ffmpeg
        m.waits.insert(30, 0);
        let mut e = engine(cfg(), m);
        let mut t = 0;
        // `editor` shares ffmpeg's session, so the session is not lowered in the first place
        // while it is merely a bystander? It is not protected yet, so it is lowered with it…
        run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        assert_eq!(e.provider().gn(2), 10);
        // …until the user moves into it.
        e.provider_mut().fg = Some(30);
        let ev = second(&mut e, &mut t, 60.0, 40.0, 60.0);
        assert!(
            has(&ev, |x| matches!(x, Event::Restored { group: true, .. })),
            "{ev:?}"
        );
        assert_eq!(e.provider().gn(2), 0);
    }

    #[test]
    fn a_culprit_that_exits_is_forgotten() {
        let mut e = engine(cfg(), scene(2));
        let mut t = 0;
        run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        assert!(!e.journal().is_empty());
        e.provider_mut().procs.retain(|p| p.pid != 10);
        let ev = run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        assert!(e.journal().is_empty(), "{ev:?}");
        assert!(!has(&ev, |x| matches!(x, Event::RestoreFailed { .. })));
    }

    #[test]
    fn a_value_changed_by_hand_is_not_overwritten_on_restore() {
        let mut e = engine(
            Config {
                use_autogroup: false,
                ..cfg()
            },
            scene(1),
        );
        let mut t = 0;
        run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        e.provider_mut()
            .procs
            .iter_mut()
            .find(|p| p.pid == 10)
            .unwrap()
            .nice = 3; // user ran renice
        let ev = run_seconds(&mut e, &mut t, 6, 0.0, 0.1, 0.0);
        assert!(has(&ev, |x| matches!(x, Event::Released)));
        assert_eq!(e.provider().by(10).nice, 3, "the user's value wins");
        assert!(e.journal().is_empty());
    }

    #[test]
    fn already_lowered_programs_are_not_made_worse_by_idle_ticks() {
        let mut e = engine(
            Config {
                use_autogroup: false,
                ..cfg()
            },
            scene(1),
        );
        let mut t = 0;
        run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        e.provider_mut()
            .procs
            .iter_mut()
            .find(|p| p.pid == 10)
            .unwrap()
            .nice = 4;
        // dead-band demand: no level change, so the user's 4 stays 4
        run_seconds(&mut e, &mut t, 4, 22.0, 78.0, 22.0);
        assert_eq!(e.provider().by(10).nice, 4);
    }

    #[test]
    fn a_failed_change_leaves_nothing_behind() {
        let mut m = scene(1);
        m.fail_set = true;
        let mut e = engine(
            Config {
                use_autogroup: false,
                ..cfg()
            },
            m,
        );
        let mut t = 0;
        let ev = run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        assert!(
            has(&ev, |x| matches!(x, Event::ApplyFailed { pid: 10, .. })),
            "{ev:?}"
        );
        assert!(e.journal().is_empty());
        assert_eq!(e.provider().by(10).nice, 0);
    }

    #[test]
    fn a_session_that_cannot_be_written_is_handled_per_process_instead() {
        // e.g. the service lacks the capability to write another user's /proc/<pid>/autogroup
        let mut m = scene(2);
        m.fail_group_set = true;
        let mut e = engine(cfg(), m);
        let mut t = 0;
        let ev = run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        assert!(
            has(&ev, |x| matches!(x, Event::ApplyFailed { pid: 10, .. })),
            "{ev:?}"
        );
        assert!(
            has(&ev, |x| matches!(
                x,
                Event::Yielded {
                    pid: 10,
                    group: false,
                    ..
                }
            )),
            "{ev:?}"
        );
        assert_eq!(e.provider().gn(2), 0, "the session itself is untouched");
        assert_eq!(e.provider().by(10).nice, 10, "but the process was lowered");
        // …and it does not keep failing on every sample.
        let later = run_seconds(&mut e, &mut t, 4, 60.0, 40.0, 60.0);
        assert!(
            !has(&later, |x| matches!(x, Event::ApplyFailed { .. })),
            "{later:?}"
        );
        // Everything is still undone properly.
        e.restore_everything();
        assert_eq!(e.provider().by(10).nice, 0);
        assert!(e.journal().is_empty());
    }

    #[test]
    fn if_the_journal_cannot_be_written_nothing_is_changed() {
        let j = Journal::open(
            std::path::Path::new("/proc/ramflow-nope/journal"),
            "test-boot",
        )
        .unwrap();
        let mut e = Engine::new(cfg(), scene(2), j);
        let mut t = 0;
        let ev = run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        assert!(has(&ev, |x| matches!(x, Event::JournalError(_))), "{ev:?}");
        assert_eq!(e.provider().gn(2), 0);
        assert!(e.provider().set_calls.is_empty() && e.provider().group_calls.is_empty());
    }

    #[test]
    fn restore_gives_up_after_repeated_failures() {
        let mut m = scene(1);
        m.fail_restore = true;
        let mut e = engine(
            Config {
                use_autogroup: false,
                ..cfg()
            },
            m,
        );
        let mut t = 0;
        run_seconds(&mut e, &mut t, 3, 60.0, 40.0, 60.0);
        // make the restore impossible but the value still ours
        e.provider_mut()
            .procs
            .iter_mut()
            .find(|p| p.pid == 10)
            .unwrap()
            .nice = 10;
        let mut failed = 0;
        for _ in 0..12 {
            let ev = second(&mut e, &mut t, 0.0, 0.0, 0.0);
            failed += ev
                .iter()
                .filter(|x| matches!(x, Event::RestoreFailed { .. }))
                .count();
        }
        assert!(failed >= 1, "failures are reported");
        assert!(failed <= MAX_RESTORE_FAILURES as usize, "…but not forever");
        assert!(e.journal().is_empty());
    }

    #[test]
    fn crash_recovery_restores_sessions_and_tasks() {
        let mut m = scene(2);
        m.procs.push(proc_(11, "make", 0, 1));
        m.group_nice.insert(2, 19);
        m.procs.iter_mut().find(|p| p.pid == 11).unwrap().nice = 15;
        let mut j = Journal::in_memory("test-boot");
        j.record(Entry::group(m.by(10).identity(), "ffmpeg", 2, 0, 19));
        j.record(Entry::task(m.by(11).identity(), "make", 0, 15));
        let mut e = Engine::new(cfg(), m, j);
        let ev = e.restore_everything();
        assert_eq!(e.provider().gn(2), 0);
        assert_eq!(e.provider().by(11).nice, 0);
        assert!(e.journal().is_empty());
        assert_eq!(
            ev.iter()
                .filter(|x| matches!(x, Event::Restored { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn a_change_interrupted_between_levels_is_still_recognised_as_ours() {
        // The program died after journaling "15 (was 10)" but before applying it: the process
        // is still at 10, and recovery must restore it rather than treat it as the user's.
        let m = scene(1);
        let id = m.by(10).identity();
        let mut m = m;
        m.procs.iter_mut().find(|p| p.pid == 10).unwrap().nice = 10;
        let mut j = Journal::in_memory("test-boot");
        j.record(Entry {
            applied_nice: 15,
            previous_nice: 10,
            ..Entry::task(id, "ffmpeg", 0, 15)
        });
        let mut e = Engine::new(cfg(), m, j);
        e.restore_everything();
        assert_eq!(e.provider().by(10).nice, 0);
        assert!(e.journal().is_empty());
    }

    #[test]
    fn a_session_whose_members_are_all_gone_is_simply_forgotten() {
        let mut m = scene(2);
        m.procs.retain(|p| p.pid != 10);
        let mut j = Journal::in_memory("test-boot");
        j.record(Entry::group(
            Identity {
                pid: 10,
                start: 5010,
            },
            "ffmpeg",
            2,
            0,
            19,
        ));
        let mut e = Engine::new(cfg(), m, j);
        let ev = e.restore_everything();
        assert!(e.journal().is_empty());
        assert!(!has(&ev, |x| matches!(x, Event::RestoreFailed { .. })));
    }

    #[test]
    fn a_reused_pid_is_never_touched() {
        let mut m = scene(1);
        m.procs.iter_mut().find(|p| p.pid == 10).unwrap().nice = 15;
        let mut j = Journal::in_memory("test-boot");
        j.record(Entry::task(Identity { pid: 10, start: 1 }, "old", 0, 15)); // different start time
        let mut e = Engine::new(cfg(), m, j);
        e.restore_everything();
        assert_eq!(e.provider().by(10).nice, 15);
        assert!(e.journal().is_empty());
    }

    #[test]
    fn without_a_foreground_the_machine_wide_stall_decides() {
        let mut m = scene(2);
        m.fg = None;
        let mut e = engine(cfg(), m);
        let mut t = 0;
        let ev = run_seconds(&mut e, &mut t, 2, 0.0, 0.0, 70.0);
        assert!(
            has(&ev, |x| matches!(
                x,
                Event::Engaged {
                    source: Source::System,
                    ..
                }
            )),
            "{ev:?}"
        );
        assert_eq!(e.provider().gn(2), 10);
    }

    #[test]
    fn memory_trouble_alone_never_lowers_priorities() {
        let mut m = scene(2);
        m.snap = starving_memory();
        let mut e = engine(cfg(), m);
        let mut t = 0;
        let ev = run_seconds(&mut e, &mut t, 8, 0.0, 5.0, 0.0);
        assert!(
            has(&ev, |x| matches!(x, Event::CriticalStarted { .. })),
            "{ev:?}"
        );
        assert!(e.provider().set_calls.is_empty() && e.provider().group_calls.is_empty());
        assert!(e.provider().paged.is_empty(), "trimming is opt-in");
    }

    // ---- automatic culprit detection ----

    fn auto_cfg() -> Config {
        Config {
            auto: true,
            yield_list: vec![],
            auto_hog_cpu: 60.0,
            auto_hog_secs: 3,
            ..cfg()
        }
    }

    /// A CPU hog running at `pct` percent of a core in session 2.
    fn hog_scene() -> Mock {
        let mut m = Mock::new(vec![
            proc_(FG, "terminal", 0, 1),
            proc_(10, "cruncher", 0, 2),
            proc_(11, "idler", 0, 3),
        ]);
        m.fg = Some(FG);
        m
    }

    fn hog_second(e: &mut Engine<Mock>, t: &mut u64, wait: f64, hog_pct: f64) -> Vec<Event> {
        let clk = sys::clock_ticks() as f64;
        for p in e.provider_mut().procs.iter_mut() {
            if p.pid == 10 {
                p.ticks += (hog_pct / 100.0 * clk).round() as u64;
            }
        }
        second(e, t, wait, 40.0, 60.0)
    }

    #[test]
    fn a_core_hog_is_found_without_any_list_and_lowered() {
        let mut e = engine(auto_cfg(), hog_scene());
        let mut t = 0;
        let mut ev = vec![];
        for _ in 0..10 {
            ev.extend(hog_second(&mut e, &mut t, 60.0, 100.0));
        }
        assert!(
            has(&ev, |x| matches!(x, Event::HogFound { pid: 10, .. })),
            "{ev:?}"
        );
        assert!(
            has(&ev, |x| matches!(x, Event::Yielded { pid: 10, .. })),
            "{ev:?}"
        );
        assert!(e.provider().gn(2) > 0);
        assert_eq!(
            e.provider().by(11).nice,
            0,
            "an idle bystander is never lowered"
        );
        assert_eq!(e.provider().gn(3), 0);
    }

    #[test]
    fn a_short_burst_is_not_a_hog() {
        let mut e = engine(auto_cfg(), hog_scene());
        let mut t = 0;
        let mut ev = vec![];
        for k in 0..12 {
            let pct = if k % 3 == 0 { 100.0 } else { 0.0 };
            ev.extend(hog_second(&mut e, &mut t, 60.0, pct));
        }
        assert!(!has(&ev, |x| matches!(x, Event::HogFound { .. })), "{ev:?}");
        assert_eq!(e.provider().gn(2), 0);
    }

    #[test]
    fn auto_mode_needs_a_known_foreground() {
        let mut m = hog_scene();
        m.fg = None;
        let mut e = engine(auto_cfg(), m);
        let mut t = 0;
        let mut ev = vec![];
        for _ in 0..12 {
            ev.extend(hog_second(&mut e, &mut t, 60.0, 100.0));
        }
        assert!(
            !has(&ev, |x| matches!(x, Event::HogFound { .. })),
            "without knowing what you work in, nothing is guessed: {ev:?}"
        );
        assert_eq!(e.provider().gn(2), 0);
    }

    #[test]
    fn auto_mode_is_off_by_default() {
        let mut e = engine(
            Config {
                auto: false,
                ..auto_cfg()
            },
            hog_scene(),
        );
        let mut t = 0;
        for _ in 0..12 {
            hog_second(&mut e, &mut t, 60.0, 100.0);
        }
        assert_eq!(e.provider().gn(2), 0);
    }

    #[test]
    fn system_accounts_and_protected_programs_are_never_hogs() {
        let mut m = hog_scene();
        m.procs.iter_mut().find(|p| p.pid == 10).unwrap().uid = 0;
        let mut e = engine(auto_cfg(), m);
        let mut t = 0;
        for _ in 0..12 {
            hog_second(&mut e, &mut t, 60.0, 100.0);
        }
        assert_eq!(e.provider().gn(2), 0, "uid below auto_min_uid");

        let mut m = hog_scene();
        m.procs.iter_mut().find(|p| p.pid == 10).unwrap().comm = "Xorg".into();
        let mut e = engine(auto_cfg(), m);
        let mut t = 0;
        for _ in 0..12 {
            hog_second(&mut e, &mut t, 60.0, 100.0);
        }
        assert_eq!(e.provider().gn(2), 0, "built-in protected set");
    }

    #[test]
    fn the_foreground_itself_is_never_a_hog() {
        let mut e = engine(auto_cfg(), hog_scene());
        let mut t = 0;
        for _ in 0..12 {
            // the foreground burns a full core and waits
            let clk = sys::clock_ticks();
            e.provider_mut()
                .procs
                .iter_mut()
                .find(|p| p.pid == FG)
                .unwrap()
                .ticks += clk;
            second(&mut e, &mut t, 60.0, 0.0, 60.0);
        }
        assert_eq!(e.provider().gn(1), 0);
        assert_eq!(e.provider().by(FG).nice, 0);
    }

    #[test]
    fn a_hog_that_got_lowered_stays_a_target_even_if_it_slows_down() {
        // After lowering, the hog's CPU use drops below the threshold. That must not read as
        // "no targets" and end the action by itself (which would make it flap).
        let mut e = engine(
            Config {
                effect_check: false,
                ..auto_cfg()
            },
            hog_scene(),
        );
        let mut t = 0;
        for _ in 0..8 {
            hog_second(&mut e, &mut t, 60.0, 100.0);
        }
        assert!(e.provider().gn(2) > 0);
        let mut ev = vec![];
        for _ in 0..4 {
            ev.extend(hog_second(&mut e, &mut t, 60.0, 20.0));
        }
        assert!(!has(&ev, |x| matches!(x, Event::Released)), "{ev:?}");
        assert!(e.provider().gn(2) > 0);
    }

    // ---- memory trimming (opt-in) ----

    fn trim_cfg() -> Config {
        Config {
            trim_idle: true,
            idle_secs: 100,
            trim_cooldown_secs: 300,
            trim_min_rss_mib: 100,
            ..cfg()
        }
    }

    fn trim_scene() -> Mock {
        let mut m = Mock::new(vec![proc_(FG, "terminal", 0, 1), proc_(40, "slack", 0, 4)]);
        m.procs.iter_mut().find(|p| p.pid == 40).unwrap().rss = 600 * MIB;
        m.fg = Some(FG);
        m.snap = starving_memory();
        m
    }

    #[test]
    fn idle_big_programs_are_trimmed_only_when_memory_is_critical_and_idle_long_enough() {
        let mut e = engine(trim_cfg(), trim_scene());
        let mut ev = vec![];
        for s in 0..200u64 {
            ev.extend(e.tick(s * 1000));
        }
        let trims = ev
            .iter()
            .filter(|x| matches!(x, Event::Trimmed { pid: 40, .. }))
            .count();
        assert_eq!(trims, 1, "once, then the cooldown applies: {ev:?}");
        assert_eq!(e.provider().paged, vec![40]);
    }

    #[test]
    fn trimming_needs_a_known_foreground() {
        let mut m = trim_scene();
        m.fg = None;
        let mut e = engine(trim_cfg(), m);
        for s in 0..300u64 {
            e.tick(s * 1000);
        }
        assert!(e.provider().paged.is_empty());
    }

    #[test]
    fn trimming_is_opt_in_and_dry_run_only_announces() {
        let mut e = engine(
            Config {
                trim_idle: false,
                ..trim_cfg()
            },
            trim_scene(),
        );
        for s in 0..300u64 {
            e.tick(s * 1000);
        }
        assert!(e.provider().paged.is_empty());

        let mut e = engine(
            Config {
                dry_run: true,
                ..trim_cfg()
            },
            trim_scene(),
        );
        let mut ev = vec![];
        for s in 0..300u64 {
            ev.extend(e.tick(s * 1000));
        }
        assert!(
            has(&ev, |x| matches!(x, Event::WouldTrim { pid: 40, .. })),
            "{ev:?}"
        );
        assert!(e.provider().paged.is_empty());
    }

    #[test]
    fn an_active_program_is_not_trimmed() {
        let mut e = engine(trim_cfg(), trim_scene());
        for s in 0..300u64 {
            e.provider_mut()
                .procs
                .iter_mut()
                .find(|p| p.pid == 40)
                .unwrap()
                .ticks += 1;
            e.tick(s * 1000);
        }
        assert!(e.provider().paged.is_empty());
    }

    // ---- protection ----

    #[test]
    fn protected_set_covers_the_foreground_its_peers_and_children() {
        let mut ps = vec![
            proc_(100, "firefox", 0, 1),
            proc_(101, "firefox", 0, 1),
            proc_(200, "other", 0, 1),
        ];
        ps[2].ppid = 100; // a child of the foreground
        ps.push(proc_(300, "unrelated", 0, 1));
        let set = protected_set(&ps, Some(100));
        assert!(set.contains(&100) && set.contains(&101) && set.contains(&200));
        assert!(!set.contains(&300));
        assert!(protected_set(&ps, None).is_empty());
    }

    #[test]
    fn the_built_in_protected_set_beats_the_yield_list() {
        let mut m = Mock::new(vec![proc_(FG, "terminal", 0, 1), proc_(60, "sshd", 0, 6)]);
        m.fg = Some(FG);
        m.exes.insert(60, "/usr/bin/ffmpeg".into()); // even if it claims to be a listed program
        let mut e = engine(cfg(), m);
        let mut t = 0;
        run_seconds(&mut e, &mut t, 4, 60.0, 40.0, 60.0);
        assert_eq!(e.provider().gn(6), 0);
        assert_eq!(e.provider().by(60).nice, 0);
    }

    #[test]
    fn events_are_readable_korean() {
        let e = Event::Engaged {
            level: 2,
            nice: 10,
            demand: 63.0,
            source: Source::Foreground,
        };
        let s = e.to_string();
        assert!(
            s.contains("단계 2") && s.contains("63%") && s.contains("기다리는"),
            "{s}"
        );
        let y = Event::Yielded {
            pid: 5,
            comm: "x".into(),
            from: 0,
            to: 10,
            group: true,
        }
        .to_string();
        assert!(y.contains("세션 전체"), "{y}");
        assert!(Event::NoEffect {
            demand: 50.0,
            retry_secs: 120
        }
        .to_string()
        .contains("효과 없음"));
    }
}
