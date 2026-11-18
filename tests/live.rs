//! Tests against real processes: the engine drives the real `LinuxProvider`. Only the
//! measurements are forced (the machine-wide stall counter and the foreground's wait counter),
//! so the tests are deterministic. They need root / CAP_SYS_NICE and python3, and skip
//! themselves otherwise.

mod common;

use common::*;
use ramflow::config::{Config, Matcher};
use ramflow::engine::{Engine, Event};
use ramflow::journal::Journal;
use ramflow::proc::{MemInfo, Proc, ProcessSample};
use ramflow::provider::{LinuxProvider, Provider, Snapshot};
use ramflow::sys;
use std::io;
use std::time::Duration;

/// The real provider, with forced pressure readings and a forced foreground.
struct Forced {
    inner: LinuxProvider,
    stall_us: u64,
    fake_wait_ns: u64,
    fg: Option<i32>,
}

impl Forced {
    /// One second passes: the machine stalled `stall`% of it, the foreground waited `wait`%.
    fn second(&mut self, stall: f64, wait: f64) {
        self.stall_us += (stall / 100.0 * 1e6) as u64;
        self.fake_wait_ns += (wait / 100.0 * 1e9) as u64;
    }
}

impl Provider for Forced {
    fn snapshot(&mut self) -> io::Result<Snapshot> {
        let mut s = self.inner.snapshot()?;
        s.mem = MemInfo {
            total: 8 << 30,
            available: 6 << 30,
            swap_total: 0,
            swap_free: 0,
        };
        s.mem_psi = None;
        let mut psi = s.cpu_psi.unwrap_or(ramflow::proc::Psi {
            some_avg10: 0.0,
            some_avg60: 0.0,
            full_avg10: 0.0,
            full_avg60: 0.0,
            some_total_us: 0,
        });
        psi.some_total_us = self.stall_us;
        s.cpu_psi = Some(psi);
        Ok(s)
    }
    fn scan(&mut self) -> Vec<ProcessSample> {
        self.inner.scan()
    }
    fn read(&self, pid: i32) -> Option<ProcessSample> {
        self.inner.read(pid)
    }
    fn exe(&self, pid: i32) -> Option<String> {
        self.inner.exe(pid)
    }
    fn foreground(&mut self) -> Option<i32> {
        self.fg
    }
    fn set_nice(&mut self, pid: i32, nice: i32) -> io::Result<()> {
        self.inner.set_nice(pid, nice)
    }
    fn restore_nice(&mut self, pid: i32, a: i32, o: i32) -> io::Result<()> {
        self.inner.restore_nice(pid, a, o)
    }
    fn autogroup(&self, pid: i32) -> Option<(u64, i32)> {
        self.inner.autogroup(pid)
    }
    fn set_autogroup_nice(&mut self, pid: i32, nice: i32) -> io::Result<()> {
        self.inner.set_autogroup_nice(pid, nice)
    }
    fn wait_ns(&self, pid: i32) -> Option<u64> {
        // Only the foreground's wait is made up; everything else is real.
        if Some(pid) == self.fg {
            Some(self.fake_wait_ns)
        } else {
            self.inner.wait_ns(pid)
        }
    }
    fn page_out(&mut self, pid: i32) -> io::Result<u64> {
        self.inner.page_out(pid)
    }
    fn boot_id(&self) -> String {
        self.inner.boot_id()
    }
}

fn cfg_for(comm: &str, state: &std::path::Path) -> Config {
    Config {
        dry_run: false,
        confirm: 1,
        interval_ms: 1000,
        release_secs: 2,
        yield_list: vec![Matcher::Comm(comm.into())],
        state_dir: Some(state.to_path_buf()),
        ..Config::default()
    }
}

fn engine_for(cfg: &Config, fg: Option<i32>) -> Engine<Forced> {
    let provider = Forced {
        inner: LinuxProvider::new(cfg),
        stall_us: 0,
        fake_wait_ns: 0,
        fg,
    };
    let journal = Journal::open(
        &cfg.state_dir.clone().unwrap().join("journal"),
        &provider.boot_id(),
    )
    .unwrap();
    Engine::new(cfg.clone(), provider, journal)
}

/// Advance `n` seconds with the given suffering; returns all events.
fn seconds(e: &mut Engine<Forced>, t: &mut u64, n: u32, stall: f64, wait: f64) -> Vec<Event> {
    let mut all = vec![];
    for _ in 0..n {
        e.provider_mut().second(stall, wait);
        *t += 1000;
        all.extend(e.tick(*t));
    }
    all
}

/// A sleeping stand-in for "the program the user is working in".
fn foreground() -> Spawned {
    spawn_python("import time\ntime.sleep(300)\n")
}

fn autogroup_nice(pid: i32) -> Option<i32> {
    ramflow::proc::parse_autogroup(&std::fs::read_to_string(format!("/proc/{pid}/autogroup")).ok()?)
        .map(|x| x.1)
}

fn thread_nices(pid: i32) -> Vec<i32> {
    Proc::new()
        .tids(pid)
        .into_iter()
        .map(|t| sys::get_nice(t).unwrap())
        .collect()
}

fn wait_for_threads(pid: i32, n: usize) {
    assert!(
        wait_until(Duration::from_secs(10), || Proc::new().tids(pid).len() >= n),
        "child never reached {n} threads"
    );
}

macro_rules! need_live {
    () => {
        if !can_run_live() {
            eprintln!("skipped: needs root/CAP_SYS_NICE and python3");
            return;
        }
    };
}

#[test]
fn a_suffering_foreground_makes_it_lower_every_thread_and_calm_gives_them_back() {
    need_live!();
    let fg = foreground();
    let target = spawn_python(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live1");
    let cfg = cfg_for(&target.comm, &state);
    assert_eq!(thread_nices(target.pid()), vec![0, 0, 0, 0]);

    let mut e = engine_for(&cfg, Some(fg.pid()));
    let mut t = 0;
    let ev = seconds(&mut e, &mut t, 3, 70.0, 70.0);
    assert!(
        ev.iter()
            .any(|x| matches!(x, Event::Yielded { pid, .. } if *pid == target.pid())),
        "{ev:?}"
    );
    let nices = thread_nices(target.pid());
    assert!(
        nices.len() >= 4 && nices.iter().all(|&n| n == 10),
        "every thread must be lowered, got {nices:?}"
    );
    assert!(state.join("journal").exists(), "the undo record is on disk");
    assert_eq!(
        sys::get_nice(fg.pid()).unwrap(),
        0,
        "the foreground is never touched"
    );

    // The suffering ends and the foreground goes quiet → everything goes back.
    let ev = seconds(&mut e, &mut t, 6, 0.0, 0.0);
    assert!(
        ev.iter().any(|x| matches!(x, Event::Restored { .. })),
        "{ev:?}"
    );
    let nices = thread_nices(target.pid());
    assert!(
        nices.iter().all(|&n| n == 0),
        "every thread restored, got {nices:?}"
    );
    assert!(
        !state.join("journal").exists(),
        "journal is removed once nothing is left to undo"
    );
}

#[test]
fn a_busy_machine_without_a_suffering_foreground_changes_nothing() {
    need_live!();
    let fg = foreground();
    let target = spawn_python(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live1b");
    let cfg = cfg_for(&target.comm, &state);
    let mut e = engine_for(&cfg, Some(fg.pid()));
    let mut t = 0;
    // The machine is stalled a lot, but the foreground itself waits for nothing.
    let ev = seconds(&mut e, &mut t, 10, 80.0, 0.0);
    assert!(
        !ev.iter()
            .any(|x| matches!(x, Event::Engaged { .. } | Event::Yielded { .. })),
        "{ev:?}"
    );
    assert!(thread_nices(target.pid()).iter().all(|&n| n == 0));
    assert!(e.journal().is_empty());
}

#[test]
fn a_job_in_another_session_is_lowered_as_a_session_and_given_back() {
    need_live!();
    if !sys::autogroup_enabled() {
        eprintln!("skipped: this kernel has no scheduler autogroup");
        return;
    }
    let fg = foreground();
    let target = spawn_python_session(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live2g");
    let cfg = cfg_for(&target.comm, &state);
    assert_eq!(autogroup_nice(target.pid()), Some(0));

    let mut e = engine_for(&cfg, Some(fg.pid()));
    let mut t = 0;
    let ev = seconds(&mut e, &mut t, 3, 70.0, 70.0);
    assert!(
        ev.iter()
            .any(|x| matches!(x, Event::Yielded { group: true, .. })),
        "{ev:?}"
    );
    assert_eq!(
        autogroup_nice(target.pid()),
        Some(10),
        "the whole session is lowered"
    );
    assert_eq!(
        autogroup_nice(fg.pid()),
        Some(0).or(autogroup_nice(fg.pid())),
        "someone else's session stays as it was"
    );
    assert_ne!(autogroup_nice(fg.pid()), Some(10));
    assert!(
        thread_nices(target.pid()).iter().all(|&n| n == 0),
        "per-process nice is not needed then"
    );

    let ev = seconds(&mut e, &mut t, 6, 0.0, 0.0);
    assert!(
        ev.iter()
            .any(|x| matches!(x, Event::Restored { group: true, .. })),
        "{ev:?}"
    );
    assert_eq!(autogroup_nice(target.pid()), Some(0));
    assert!(!state.join("journal").exists());
}

#[test]
fn recovers_after_a_crash_from_the_journal_alone() {
    need_live!();
    let fg = foreground();
    let target = spawn_python(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live2");
    let cfg = cfg_for(&target.comm, &state);

    let mut first = engine_for(&cfg, Some(fg.pid()));
    let mut t = 0;
    seconds(&mut first, &mut t, 3, 70.0, 70.0);
    assert!(thread_nices(target.pid()).iter().all(|&n| n == 10));
    drop(first); // "kill -9": no shutdown path runs, only the file survives

    let mut second = engine_for(&cfg, Some(fg.pid()));
    assert!(
        !second.journal().is_empty(),
        "the new process finds the old journal"
    );
    let ev = second.restore_everything();
    assert!(
        ev.iter().any(|x| matches!(x, Event::Restored { .. })),
        "{ev:?}"
    );
    assert!(thread_nices(target.pid()).iter().all(|&n| n == 0));
    assert!(!state.join("journal").exists());
}

#[test]
fn a_session_is_recovered_after_a_crash_too() {
    need_live!();
    if !sys::autogroup_enabled() {
        eprintln!("skipped: this kernel has no scheduler autogroup");
        return;
    }
    let fg = foreground();
    let target = spawn_python_session(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live2h");
    let cfg = cfg_for(&target.comm, &state);
    let mut first = engine_for(&cfg, Some(fg.pid()));
    let mut t = 0;
    seconds(&mut first, &mut t, 3, 70.0, 70.0);
    assert_eq!(autogroup_nice(target.pid()), Some(10));
    drop(first);
    let mut second = engine_for(&cfg, Some(fg.pid()));
    second.restore_everything();
    assert_eq!(autogroup_nice(target.pid()), Some(0));
    assert!(!state.join("journal").exists());
}

#[test]
fn the_foreground_process_is_never_lowered_even_if_listed() {
    need_live!();
    let target = spawn_python(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live3");
    let cfg = cfg_for(&target.comm, &state);
    let mut e = engine_for(&cfg, Some(target.pid()));
    let mut t = 0;
    seconds(&mut e, &mut t, 6, 70.0, 70.0);
    assert!(thread_nices(target.pid()).iter().all(|&n| n == 0));
    assert!(e.journal().is_empty());
}

#[test]
fn a_thread_the_user_reniced_keeps_its_own_value() {
    need_live!();
    let fg = foreground();
    let target = spawn_python(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live4");
    let cfg = cfg_for(&target.comm, &state);
    let mut e = engine_for(&cfg, Some(fg.pid()));
    let mut t = 0;
    seconds(&mut e, &mut t, 3, 70.0, 70.0);
    // Somebody sets one worker thread to a value of their own while we have the job lowered.
    let tids = Proc::new().tids(target.pid());
    let odd = *tids.last().unwrap();
    assert_ne!(odd, target.pid());
    sys::set_nice(odd, 7).unwrap();

    seconds(&mut e, &mut t, 6, 0.0, 0.0);
    assert_eq!(sys::get_nice(odd).unwrap(), 7, "not ours to change");
    assert_eq!(sys::get_nice(target.pid()).unwrap(), 0);
}

#[test]
fn auto_mode_finds_a_real_hog_without_a_list() {
    need_live!();
    let fg = foreground();
    let hog = spawn_python(BUSY_LOOP);
    let idle = spawn_python(THREADED_SLEEPER);
    wait_for_threads(idle.pid(), 4);
    let state = tmpdir("live6");
    let cfg = Config {
        auto: true,
        auto_min_uid: 0,
        auto_hog_secs: 2,
        yield_list: vec![],
        use_autogroup: false,
        ..cfg_for("nothing", &state)
    };
    let mut e = engine_for(&cfg, Some(fg.pid()));
    let mut t = 0;
    let mut ev = vec![];
    // Real time must pass for the hog's CPU use to be measured.
    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(1000));
        ev.extend(seconds(&mut e, &mut t, 1, 70.0, 70.0));
    }
    assert!(
        ev.iter()
            .any(|x| matches!(x, Event::HogFound { pid, .. } if *pid == hog.pid())),
        "{ev:?}"
    );
    assert!(sys::get_nice(hog.pid()).unwrap() > 0, "the hog was lowered");
    assert!(
        thread_nices(idle.pid()).iter().all(|&n| n == 0),
        "a sleeping process is not a hog"
    );
    assert_eq!(sys::get_nice(fg.pid()).unwrap(), 0);
    e.restore_everything();
    assert_eq!(sys::get_nice(hog.pid()).unwrap(), 0);
}

#[test]
fn dry_run_changes_nothing_on_a_real_process() {
    need_live!();
    let fg = foreground();
    let target = spawn_python(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live5");
    let cfg = Config {
        dry_run: true,
        ..cfg_for(&target.comm, &state)
    };
    let mut e = engine_for(&cfg, Some(fg.pid()));
    let mut t = 0;
    let ev = seconds(&mut e, &mut t, 5, 70.0, 70.0);
    assert!(
        ev.iter().any(|x| matches!(x, Event::WouldYield { .. })),
        "{ev:?}"
    );
    assert!(thread_nices(target.pid()).iter().all(|&n| n == 0));
    assert!(!state.join("journal").exists());
}

#[test]
fn page_out_runs_against_a_real_process() {
    if !can_run_live() {
        eprintln!("skipped: needs root/CAP_SYS_NICE and python3");
        return;
    }
    // Hold ~64 MiB of anonymous memory, then ask the kernel to reclaim it.
    let hog =
        spawn_python("import time\nb = bytearray(b'x' * (64 * 1024 * 1024))\ntime.sleep(300)\n");
    assert!(wait_until(Duration::from_secs(10), || {
        Proc::new()
            .process(hog.pid())
            .map(|p| p.rss > 60 << 20)
            .unwrap_or(false)
    }));
    let mut p = LinuxProvider::new(&Config::default());
    match p.page_out(hog.pid()) {
        Ok(bytes) => assert!(bytes > 0, "the kernel accepted some ranges"),
        // Kernels older than 5.10, or sandboxes that forbid it: not a failure of RamFlow.
        Err(e) => eprintln!("process_madvise unavailable here: {e}"),
    }
    // The target must survive and still be readable afterwards.
    assert!(Proc::new().process(hog.pid()).is_some());
}
