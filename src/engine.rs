//! The decision engine: one [`Engine::tick`] per sample.
//!
//! Safety rules, in order of importance:
//! 1. Nothing is changed in dry-run mode.
//! 2. The original value is journaled to disk *before* a change; if that fails, no change is made.
//! 3. Only processes the user listed (`yield`) are ever lowered, never the foreground window's
//!    process, its children, or same-named peers, and never the built-in protected set.
//! 4. A change is undone as soon as the pressure ends, the process becomes the foreground, or
//!    the daemon stops — but never over a value the user changed by hand in the meantime.

use crate::config::{trunc_comm, Config};
use crate::journal::{Entry, Journal};
use crate::policy::{Pressure, PressureTracker, Signals};
use crate::proc::{Identity, ProcessSample};
use crate::provider::Provider;
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

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    ContentionStarted {
        cpu: f64,
        memory: f64,
    },
    ContentionEnded,
    CriticalStarted {
        memory: f64,
    },
    CriticalEnded,
    Yielded {
        pid: i32,
        comm: String,
        from: i32,
        to: i32,
    },
    WouldYield {
        pid: i32,
        comm: String,
        from: i32,
        to: i32,
    },
    Restored {
        pid: i32,
        comm: String,
        nice: i32,
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

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::fmt::bytes;
        match self {
            Event::ContentionStarted { cpu, memory } => {
                write!(
                    f,
                    "자원 경쟁 시작 (CPU {cpu:.0}%, 메모리 압박 {memory:.0}/100)"
                )
            }
            Event::ContentionEnded => write!(f, "자원 경쟁 끝"),
            Event::CriticalStarted { memory } => {
                write!(f, "메모리 위기 시작 (압박 {memory:.0}/100)")
            }
            Event::CriticalEnded => write!(f, "메모리 위기 끝"),
            Event::Yielded {
                pid,
                comm,
                from,
                to,
            } => {
                write!(f, "양보: {comm}({pid}) 우선순위값 {from} → {to}")
            }
            Event::WouldYield {
                pid,
                comm,
                from,
                to,
            } => {
                write!(
                    f,
                    "[예행연습] 양보 예정: {comm}({pid}) 우선순위값 {from} → {to}"
                )
            }
            Event::Restored { pid, comm, nice } => {
                write!(f, "원복: {comm}({pid}) 우선순위값 {nice}로 되돌림")
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
    /// The process exited, or the pid now belongs to a different process.
    Gone,
    /// The value is no longer what we set: somebody else changed it, leave it alone.
    ChangedByUser,
    Failed(String),
}

/// Undo one journaled change, if (and only if) it is still ours to undo.
pub fn restore_one<P: Provider>(provider: &mut P, e: &Entry) -> Outcome {
    let pid = e.id.pid;
    match provider.read(pid) {
        None => return Outcome::Gone,
        Some(cur) if cur.start != e.id.start => return Outcome::Gone,
        Some(cur) if cur.nice != e.applied_nice => return Outcome::ChangedByUser,
        Some(_) => {}
    }
    match provider.restore_nice(pid, e.applied_nice, e.original_nice) {
        Ok(()) => match provider.read(pid) {
            Some(cur) if cur.start == e.id.start && cur.nice != e.original_nice => {
                Outcome::Failed(format!("값이 {}로 남아 있음", cur.nice))
            }
            _ => Outcome::Restored,
        },
        Err(err) => match provider.read(pid) {
            Some(cur) if cur.start == e.id.start => Outcome::Failed(err.to_string()),
            _ => Outcome::Gone, // it exited while we were working
        },
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

pub struct Engine<P: Provider> {
    cfg: Config,
    provider: P,
    tracker: PressureTracker,
    journal: Journal,
    announced: HashSet<Identity>,
    /// identity -> (cpu ticks last seen, time of the last change in ms)
    activity: HashMap<Identity, (u64, u64)>,
    last_trim: HashMap<Identity, u64>,
    restore_failures: HashMap<Identity, u32>,
    was_contention: bool,
    was_critical: bool,
    me: i32,
}

impl<P: Provider> Engine<P> {
    pub fn new(cfg: Config, provider: P, journal: Journal) -> Self {
        let tracker = PressureTracker::new(&cfg);
        Engine {
            cfg,
            provider,
            tracker,
            journal,
            announced: HashSet::new(),
            activity: HashMap::new(),
            last_trim: HashMap::new(),
            restore_failures: HashMap::new(),
            was_contention: false,
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

    /// Undo everything still in the journal (crash recovery at start, and at shutdown).
    pub fn restore_everything(&mut self) -> Vec<Event> {
        let mut ev = Vec::new();
        for _ in 0..3 {
            self.restore_entries(&|_| true, &mut ev);
            if self.journal.is_empty() {
                break;
            }
        }
        ev
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
        let p = self.tracker.update(snap.cpu_pct, &signals);
        self.report_transitions(&p, &mut ev);

        let contention = p.contention();
        if !contention && self.journal.is_empty() {
            // Calm and nothing to undo: stay cheap, do not even look at processes.
            self.announced.clear();
            self.activity.clear();
            return ev;
        }

        let procs = self.provider.scan();
        let foreground = self.provider.foreground();
        let protected = protected_set(&procs, foreground);

        if !contention {
            self.restore_entries(&|_| true, &mut ev);
            return ev;
        }

        // The user switched to something we had lowered: give it back right away.
        self.restore_entries(&|e| protected.contains(&e.id.pid), &mut ev);
        self.yield_background(&procs, &protected, &mut ev);

        if p.critical && self.cfg.trim_idle {
            self.update_activity(&procs, now_ms);
            if foreground.is_some() {
                self.trim_idle(&procs, &protected, now_ms, &mut ev);
            }
        }
        ev
    }

    fn report_transitions(&mut self, p: &Pressure, ev: &mut Vec<Event>) {
        let c = p.contention();
        if c != self.was_contention {
            ev.push(if c {
                Event::ContentionStarted {
                    cpu: p.cpu,
                    memory: p.memory,
                }
            } else {
                Event::ContentionEnded
            });
            self.was_contention = c;
        }
        if p.critical != self.was_critical {
            ev.push(if p.critical {
                Event::CriticalStarted { memory: p.memory }
            } else {
                Event::CriticalEnded
            });
            self.was_critical = p.critical;
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

    fn yield_background(
        &mut self,
        procs: &[ProcessSample],
        protected: &HashSet<i32>,
        ev: &mut Vec<Event>,
    ) {
        for s in procs {
            if protected.contains(&s.pid) || s.nice >= self.cfg.nice_target {
                continue;
            }
            if self.never_touch(s) || !self.is_yield_target(s) {
                continue;
            }
            let id = s.identity();
            if self.journal.contains(id) {
                continue;
            }
            let (from, to) = (s.nice, self.cfg.nice_target);

            if self.cfg.dry_run {
                if self.announced.insert(id) {
                    ev.push(Event::WouldYield {
                        pid: s.pid,
                        comm: s.comm.clone(),
                        from,
                        to,
                    });
                }
                continue;
            }

            // 1) Make the undo possible first. If it cannot be recorded, do not change anything.
            self.journal.record(Entry {
                id,
                comm: s.comm.clone(),
                original_nice: from,
                applied_nice: to,
            });
            if let Err(e) = self.journal.save() {
                self.journal.remove(id);
                ev.push(Event::JournalError(e.to_string()));
                continue;
            }
            // 2) Re-check identity right before acting, then 3) change.
            let still_ours = matches!(self.provider.read(s.pid), Some(c) if c.start == s.start);
            let result = if still_ours {
                self.provider.set_nice(s.pid, to).map_err(|e| e.to_string())
            } else {
                Err("프로세스가 그 사이에 바뀌었습니다".to_string())
            };
            match result {
                Ok(()) => ev.push(Event::Yielded {
                    pid: s.pid,
                    comm: s.comm.clone(),
                    from,
                    to,
                }),
                Err(reason) => {
                    // Undo whatever part may have been applied, then forget the entry.
                    if still_ours {
                        let _ = self.provider.restore_nice(s.pid, to, from);
                    }
                    self.journal.remove(id);
                    if let Err(e) = self.journal.save() {
                        ev.push(Event::JournalError(e.to_string()));
                    }
                    ev.push(Event::ApplyFailed {
                        pid: s.pid,
                        comm: s.comm.clone(),
                        reason,
                    });
                }
            }
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
            match restore_one(&mut self.provider, &e) {
                Outcome::Restored => {
                    self.restore_failures.remove(&e.id);
                    self.journal.remove(e.id);
                    ev.push(Event::Restored {
                        pid: e.id.pid,
                        comm: e.comm.clone(),
                        nice: e.original_nice,
                    });
                }
                Outcome::Gone | Outcome::ChangedByUser => {
                    self.restore_failures.remove(&e.id);
                    self.journal.remove(e.id);
                }
                Outcome::Failed(reason) => {
                    let n = self.restore_failures.entry(e.id).or_insert(0);
                    *n += 1;
                    let give_up = *n >= MAX_RESTORE_FAILURES;
                    ev.push(Event::RestoreFailed {
                        pid: e.id.pid,
                        comm: e.comm.clone(),
                        reason,
                    });
                    if give_up {
                        self.restore_failures.remove(&e.id);
                        self.journal.remove(e.id);
                    }
                }
            }
        }
        if let Err(err) = self.journal.save() {
            ev.push(Event::JournalError(err.to_string()));
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

    struct Mock {
        snap: Snapshot,
        procs: Vec<ProcessSample>,
        exes: HashMap<i32, String>,
        fg: Option<i32>,
        fail_set: bool,
        fail_restore: bool,
        set_calls: Vec<(i32, i32)>,
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
            cpu_pct: 5.0,
        }
    }

    fn busy() -> Snapshot {
        Snapshot {
            cpu_pct: 97.0,
            ..calm()
        }
    }

    fn starving() -> Snapshot {
        Snapshot {
            mem: MemInfo {
                total: 8192 * MIB,
                available: 80 * MIB,
                swap_total: 4096 * MIB,
                swap_free: 100 * MIB,
            },
            mem_psi: Some(crate::proc::Psi {
                some_avg10: 60.0,
                some_avg60: 40.0,
                full_avg10: 30.0,
                full_avg60: 20.0,
            }),
            cpu_pct: 20.0,
        }
    }

    fn proc_(pid: i32, comm: &str, nice: i32) -> ProcessSample {
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
        }
    }

    impl Mock {
        fn new(procs: Vec<ProcessSample>) -> Self {
            let exes = procs
                .iter()
                .map(|p| (p.pid, format!("/usr/bin/{}", p.comm)))
                .collect();
            Mock {
                snap: calm(),
                procs,
                exes,
                fg: None,
                fail_set: false,
                fail_restore: false,
                set_calls: vec![],
                paged: vec![],
            }
        }
        fn by(&self, pid: i32) -> &ProcessSample {
            self.procs.iter().find(|p| p.pid == pid).unwrap()
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

    #[test]
    fn calm_machine_does_nothing_and_does_not_scan() {
        let mut e = engine(cfg(), Mock::new(vec![proc_(10, "ffmpeg", 0)]));
        for t in 0..5 {
            assert!(e.tick(t * 1000).is_empty());
        }
        assert!(e.provider().set_calls.is_empty());
    }

    #[test]
    fn dry_run_reports_but_never_changes_anything() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 0)]);
        m.snap = busy();
        let mut e = engine(
            Config {
                dry_run: true,
                ..cfg()
            },
            m,
        );
        let first = e.tick(0);
        assert!(has(&first, |x| matches!(
            x,
            Event::WouldYield {
                pid: 10,
                from: 0,
                to: 15,
                ..
            }
        )));
        let second = e.tick(1000);
        assert!(
            !has(&second, |x| matches!(x, Event::WouldYield { .. })),
            "announced only once"
        );
        assert!(e.provider().set_calls.is_empty());
        assert!(e.journal().is_empty());
        assert_eq!(e.provider().by(10).nice, 0);
    }

    #[test]
    fn yields_under_contention_and_restores_when_it_ends() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 0), proc_(11, "firefox", 0)]);
        m.snap = busy();
        let mut e = engine(cfg(), m);
        let ev = e.tick(0);
        assert!(has(&ev, |x| matches!(
            x,
            Event::Yielded {
                pid: 10,
                from: 0,
                to: 15,
                ..
            }
        )));
        assert_eq!(e.provider().by(10).nice, 15);
        assert_eq!(
            e.provider().by(11).nice,
            0,
            "unlisted processes are never touched"
        );
        assert_eq!(e.journal().entries().len(), 1);

        e.provider_mut().snap = calm();
        let mut restored = false;
        for t in 1..10 {
            restored |= has(&e.tick(t * 1000), |x| {
                matches!(
                    x,
                    Event::Restored {
                        pid: 10,
                        nice: 0,
                        ..
                    }
                )
            });
        }
        assert!(restored);
        assert_eq!(e.provider().by(10).nice, 0);
        assert!(e.journal().is_empty());
    }

    #[test]
    fn memory_starvation_also_triggers_yielding() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 0)]);
        m.snap = starving();
        let mut e = engine(cfg(), m);
        let ev = e.tick(0);
        assert!(has(&ev, |x| matches!(x, Event::CriticalStarted { .. })));
        assert!(has(&ev, |x| matches!(x, Event::Yielded { pid: 10, .. })));
    }

    #[test]
    fn a_brief_spike_is_ignored_when_confirmation_is_required() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 0)]);
        m.snap = busy();
        let mut e = engine(
            Config {
                confirm: 3,
                ..cfg()
            },
            m,
        );
        assert!(e.tick(0).is_empty());
        e.provider_mut().snap = calm();
        for t in 1..6 {
            assert!(e
                .tick(t * 1000)
                .iter()
                .all(|x| !matches!(x, Event::Yielded { .. })));
        }
        assert!(e.provider().set_calls.is_empty());
    }

    #[test]
    fn foreground_process_its_children_and_peers_are_protected() {
        let mut procs = vec![
            proc_(20, "ffmpeg", 0),
            proc_(21, "ffmpeg", 0),
            proc_(22, "ffmpeg", 0),
            proc_(30, "gnome-terminal", 0),
        ];
        procs[0].ppid = 30; // child of the focused terminal
        procs[1].ppid = 1; // peer: same program as pid 20 → also protected via same-comm
        procs[2].ppid = 1;
        let mut m = Mock::new(procs);
        m.snap = busy();
        m.fg = Some(30);
        let mut e = engine(cfg(), m);
        let ev = e.tick(0);
        // 20 is a child of the foreground terminal → protected. 21 and 22 are unrelated → yield.
        assert!(!has(&ev, |x| matches!(x, Event::Yielded { pid: 20, .. })));
        assert!(has(&ev, |x| matches!(x, Event::Yielded { pid: 21, .. })));
        assert!(has(&ev, |x| matches!(x, Event::Yielded { pid: 22, .. })));
    }

    #[test]
    fn same_program_as_the_foreground_window_is_protected() {
        let mut m = Mock::new(vec![proc_(20, "ffmpeg", 0), proc_(21, "ffmpeg", 0)]);
        m.snap = busy();
        m.fg = Some(20);
        let mut e = engine(cfg(), m);
        let ev = e.tick(0);
        assert!(!has(&ev, |x| matches!(x, Event::Yielded { .. })), "{ev:?}");
    }

    #[test]
    fn switching_to_a_lowered_process_restores_it_immediately() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 0)]);
        m.snap = busy();
        let mut e = engine(cfg(), m);
        e.tick(0);
        assert_eq!(e.provider().by(10).nice, 15);
        e.provider_mut().fg = Some(10); // user alt-tabs to it while the machine is still busy
        let ev = e.tick(1000);
        assert!(has(&ev, |x| matches!(x, Event::Restored { pid: 10, .. })));
        assert_eq!(e.provider().by(10).nice, 0);
        assert!(!has(&ev, |x| matches!(x, Event::Yielded { .. })));
    }

    #[test]
    fn path_matcher_requires_the_exact_executable() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 0)]);
        m.exes.insert(10, "/tmp/other/ffmpeg".into());
        m.snap = busy();
        let mut e = engine(cfg(), m);
        assert!(e
            .tick(0)
            .iter()
            .all(|x| !matches!(x, Event::Yielded { .. })));
    }

    #[test]
    fn builtin_protected_processes_are_never_touched_even_if_listed() {
        let mut m = Mock::new(vec![
            proc_(10, "Xorg", 0),
            proc_(11, "sshd", 0),
            proc_(12, "systemd", 0),
        ]);
        m.snap = busy();
        let c = Config {
            yield_list: vec![
                Matcher::Comm("Xorg".into()),
                Matcher::Comm("sshd".into()),
                Matcher::Comm("systemd".into()),
            ],
            ..cfg()
        };
        let mut e = engine(c, m);
        assert!(e
            .tick(0)
            .iter()
            .all(|x| !matches!(x, Event::Yielded { .. })));
        assert!(e.provider().set_calls.is_empty());
    }

    #[test]
    fn user_protect_list_wins_over_yield_list() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 0)]);
        m.snap = busy();
        let c = Config {
            protect: vec![Matcher::Comm("ffmpeg".into())],
            ..cfg()
        };
        let mut e = engine(c, m);
        assert!(e
            .tick(0)
            .iter()
            .all(|x| !matches!(x, Event::Yielded { .. })));
    }

    #[test]
    fn init_kernel_threads_zombies_and_self_are_skipped() {
        let me = std::process::id() as i32;
        let mut z = proc_(40, "ffmpeg", 0);
        z.state = 'Z';
        let mut k = proc_(41, "ffmpeg", 0);
        k.ppid = 2;
        let mut procs = vec![proc_(1, "ffmpeg", 0), z, k, proc_(me, "ffmpeg", 0)];
        procs[0].ppid = 0;
        let mut m = Mock::new(procs);
        m.snap = busy();
        let mut e = engine(
            Config {
                yield_list: vec![Matcher::Comm("ffmpeg".into())],
                ..cfg()
            },
            m,
        );
        assert!(e
            .tick(0)
            .iter()
            .all(|x| !matches!(x, Event::Yielded { .. })));
    }

    #[test]
    fn already_low_priority_processes_are_left_alone() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 19)]);
        m.snap = busy();
        let mut e = engine(cfg(), m);
        assert!(e
            .tick(0)
            .iter()
            .all(|x| !matches!(x, Event::Yielded { .. })));
        assert!(e.journal().is_empty());
    }

    #[test]
    fn a_value_changed_by_the_user_is_not_overwritten_on_restore() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 0)]);
        m.snap = busy();
        let mut e = engine(cfg(), m);
        e.tick(0);
        // The user renices it by hand while we have it lowered.
        e.provider_mut().procs[0].nice = 5;
        e.provider_mut().snap = calm();
        for t in 1..10 {
            e.tick(t * 1000);
        }
        assert_eq!(e.provider().by(10).nice, 5, "their choice stands");
        assert!(e.journal().is_empty());
    }

    #[test]
    fn a_reused_pid_is_never_restored_or_touched() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 0)]);
        m.snap = busy();
        let mut e = engine(cfg(), m);
        e.tick(0);
        // The process exits and an unrelated program gets the same pid, at nice 15 by coincidence.
        let mut other = proc_(10, "other", 15);
        other.start = 999_999;
        e.provider_mut().procs = vec![other];
        e.provider_mut().snap = calm();
        for t in 1..10 {
            e.tick(t * 1000);
        }
        assert_eq!(
            e.provider().by(10).nice,
            15,
            "the newcomer must be left exactly as it is"
        );
        assert!(e.journal().is_empty());
    }

    #[test]
    fn failed_apply_leaves_no_trace() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 0)]);
        m.snap = busy();
        m.fail_set = true;
        let mut e = engine(cfg(), m);
        let ev = e.tick(0);
        assert!(has(&ev, |x| matches!(
            x,
            Event::ApplyFailed { pid: 10, .. }
        )));
        assert!(e.journal().is_empty());
        assert_eq!(e.provider().by(10).nice, 0);
    }

    #[test]
    fn nothing_is_changed_when_the_journal_cannot_be_written() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 0)]);
        m.snap = busy();
        let journal = Journal::open(
            std::path::Path::new("/proc/ramflow-no-such-dir/journal"),
            "test-boot",
        )
        .unwrap();
        let mut e = Engine::new(cfg(), m, journal);
        let ev = e.tick(0);
        assert!(has(&ev, |x| matches!(x, Event::JournalError(_))));
        assert!(e.provider().set_calls.is_empty(), "no journal, no change");
        assert_eq!(e.provider().by(10).nice, 0);
    }

    #[test]
    fn restore_failures_are_retried_then_given_up() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 0)]);
        m.snap = busy();
        let mut e = engine(cfg(), m);
        e.tick(0);
        e.provider_mut().fail_restore = true;
        e.provider_mut().snap = calm();
        let mut failures = 0;
        for t in 1..30 {
            failures += e
                .tick(t * 1000)
                .iter()
                .filter(|x| matches!(x, Event::RestoreFailed { .. }))
                .count();
        }
        assert_eq!(failures as u32, MAX_RESTORE_FAILURES);
        assert!(
            e.journal().is_empty(),
            "gave up instead of retrying forever"
        );
    }

    #[test]
    fn restore_everything_recovers_a_previous_run() {
        // Simulates the daemon starting after a crash: the journal already has an entry.
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 15)]);
        let id = m.by(10).identity();
        let mut journal = Journal::in_memory("test-boot");
        journal.record(Entry {
            id,
            comm: "ffmpeg".into(),
            original_nice: 3,
            applied_nice: 15,
        });
        m.snap = calm();
        let mut e = Engine::new(cfg(), m, journal);
        let ev = e.restore_everything();
        assert!(has(&ev, |x| matches!(
            x,
            Event::Restored {
                pid: 10,
                nice: 3,
                ..
            }
        )));
        assert_eq!(e.provider().by(10).nice, 3);
        assert!(e.journal().is_empty());
    }

    #[test]
    fn leftover_journal_is_cleaned_up_by_normal_ticks_too() {
        let mut m = Mock::new(vec![proc_(10, "ffmpeg", 15)]);
        let id = m.by(10).identity();
        let mut journal = Journal::in_memory("test-boot");
        journal.record(Entry {
            id,
            comm: "ffmpeg".into(),
            original_nice: 0,
            applied_nice: 15,
        });
        m.snap = calm();
        let mut e = Engine::new(cfg(), m, journal);
        e.tick(0);
        assert_eq!(e.provider().by(10).nice, 0);
    }

    #[test]
    fn sensor_failure_is_reported_not_fatal() {
        struct Broken;
        impl Provider for Broken {
            fn snapshot(&mut self) -> io::Result<Snapshot> {
                Err(io::Error::other("boom"))
            }
            fn scan(&mut self) -> Vec<ProcessSample> {
                vec![]
            }
            fn read(&self, _: i32) -> Option<ProcessSample> {
                None
            }
            fn exe(&self, _: i32) -> Option<String> {
                None
            }
            fn foreground(&mut self) -> Option<i32> {
                None
            }
            fn set_nice(&mut self, _: i32, _: i32) -> io::Result<()> {
                unreachable!()
            }
            fn restore_nice(&mut self, _: i32, _: i32, _: i32) -> io::Result<()> {
                unreachable!()
            }
            fn page_out(&mut self, _: i32) -> io::Result<u64> {
                unreachable!()
            }
            fn boot_id(&self) -> String {
                String::new()
            }
        }
        let mut e = Engine::new(cfg(), Broken, Journal::in_memory(""));
        let ev = e.tick(0);
        assert!(matches!(ev.as_slice(), [Event::SensorError(m)] if m == "boom"));
    }

    // ---- idle trimming -------------------------------------------------------------------

    fn trim_cfg() -> Config {
        Config {
            trim_idle: true,
            idle_secs: 60,
            trim_min_rss_mib: 100,
            trim_cooldown_secs: 300,
            ..cfg()
        }
    }

    fn big(pid: i32, comm: &str, rss_mib: u64) -> ProcessSample {
        ProcessSample {
            rss: rss_mib * MIB,
            ..proc_(pid, comm, 0)
        }
    }

    fn run_until(e: &mut Engine<Mock>, from_ms: u64, to_ms: u64) -> Vec<Event> {
        let mut all = vec![];
        let mut t = from_ms;
        while t <= to_ms {
            all.extend(e.tick(t));
            t += 5000;
        }
        all
    }

    #[test]
    fn trimming_is_off_unless_enabled() {
        let mut m = Mock::new(vec![big(50, "slackish", 900), big(51, "browser", 400)]);
        m.snap = starving();
        m.fg = Some(51);
        let mut e = engine(
            Config {
                trim_idle: false,
                ..trim_cfg()
            },
            m,
        );
        let ev = run_until(&mut e, 0, 600_000);
        assert!(e.provider().paged.is_empty());
        assert!(!has(&ev, |x| matches!(
            x,
            Event::Trimmed { .. } | Event::WouldTrim { .. }
        )));
    }

    #[test]
    fn trimming_needs_a_known_foreground() {
        let mut m = Mock::new(vec![big(50, "slackish", 900)]);
        m.snap = starving();
        m.fg = None;
        let mut e = engine(trim_cfg(), m);
        run_until(&mut e, 0, 600_000);
        assert!(
            e.provider().paged.is_empty(),
            "unknown foreground must never be read as 'nothing is in use'"
        );
    }

    #[test]
    fn idle_big_background_app_is_trimmed_under_critical_pressure() {
        let mut m = Mock::new(vec![
            big(50, "slackish", 900),
            big(51, "browser", 400),
            big(52, "tiny", 20),
        ]);
        m.snap = starving();
        m.fg = Some(51);
        let mut e = engine(trim_cfg(), m);
        let ev = run_until(&mut e, 0, 200_000);
        assert_eq!(
            e.provider().paged,
            vec![50],
            "only the idle, large, non-foreground app"
        );
        assert!(has(&ev, |x| matches!(x, Event::Trimmed { pid: 50, .. })));
    }

    #[test]
    fn app_must_be_idle_for_the_whole_period() {
        let mut m = Mock::new(vec![big(50, "slackish", 900), big(51, "browser", 400)]);
        m.snap = starving();
        m.fg = Some(51);
        let mut e = engine(trim_cfg(), m);
        // Keep the app busy: ticks change every sample.
        let mut t = 0;
        while t <= 200_000 {
            e.provider_mut().procs[0].ticks += 100;
            e.tick(t);
            t += 5000;
        }
        assert!(e.provider().paged.is_empty());
    }

    #[test]
    fn trim_respects_cooldown() {
        let mut m = Mock::new(vec![big(50, "slackish", 900), big(51, "browser", 400)]);
        m.snap = starving();
        m.fg = Some(51);
        let mut e = engine(trim_cfg(), m);
        run_until(&mut e, 0, 250_000); // idle for 60 s at t=60 s → trimmed once; cooldown 300 s
        assert_eq!(e.provider().paged.len(), 1);
        run_until(&mut e, 255_000, 355_000);
        assert_eq!(
            e.provider().paged.len(),
            1,
            "still cooling down until t=360 s"
        );
        run_until(&mut e, 360_000, 420_000);
        assert_eq!(e.provider().paged.len(), 2, "cooldown elapsed");
    }

    #[test]
    fn system_users_and_protected_programs_are_not_trimmed() {
        let mut sys_user = big(60, "postgres", 2000);
        sys_user.uid = 110;
        let mut m = Mock::new(vec![
            sys_user,
            big(61, "gnome-shell", 1500),
            big(62, "browser", 400),
        ]);
        m.snap = starving();
        m.fg = Some(62);
        let mut e = engine(trim_cfg(), m);
        run_until(&mut e, 0, 300_000);
        assert!(e.provider().paged.is_empty());
    }

    #[test]
    fn foreground_group_is_not_trimmed() {
        let mut helper = big(71, "browser", 800); // helper of the same program as the focused window
        helper.ppid = 1;
        let mut m = Mock::new(vec![big(70, "browser", 400), helper]);
        m.snap = starving();
        m.fg = Some(70);
        let mut e = engine(trim_cfg(), m);
        run_until(&mut e, 0, 300_000);
        assert!(e.provider().paged.is_empty());
    }

    #[test]
    fn trim_dry_run_only_reports() {
        let mut m = Mock::new(vec![big(50, "slackish", 900), big(51, "browser", 400)]);
        m.snap = starving();
        m.fg = Some(51);
        let mut e = engine(
            Config {
                dry_run: true,
                ..trim_cfg()
            },
            m,
        );
        let ev = run_until(&mut e, 0, 200_000);
        assert!(e.provider().paged.is_empty());
        assert!(has(&ev, |x| matches!(x, Event::WouldTrim { pid: 50, .. })));
    }

    #[test]
    fn protected_set_covers_descendants_and_peers() {
        let mut a = proc_(2000, "term", 0);
        a.ppid = 1;
        let mut b = proc_(2001, "bash", 0);
        b.ppid = 2000;
        let mut c = proc_(2002, "make", 0);
        c.ppid = 2001;
        let mut d = proc_(2003, "term", 0);
        d.ppid = 1; // second window of the same program
        let mut e_ = proc_(2004, "unrelated", 0);
        e_.ppid = 1;
        let set = protected_set(&[a, b, c, d, e_], Some(2000));
        assert!(
            set.contains(&2000)
                && set.contains(&2001)
                && set.contains(&2002)
                && set.contains(&2003)
        );
        assert!(!set.contains(&2004));
        assert!(protected_set(&[], None).is_empty());
    }
}
