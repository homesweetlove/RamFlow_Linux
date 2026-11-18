//! Tests against real processes: the engine drives the real `LinuxProvider`, only the pressure
//! reading is forced. They need root / CAP_SYS_NICE and python3, and skip themselves otherwise.

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

/// The real provider with a forced CPU reading (and optionally a forced foreground).
struct Forced {
    inner: LinuxProvider,
    cpu: f64,
    fg: Option<i32>,
}

impl Provider for Forced {
    fn snapshot(&mut self) -> io::Result<Snapshot> {
        let mut s = self.inner.snapshot()?;
        s.cpu_pct = self.cpu;
        s.mem = MemInfo {
            total: 8 << 30,
            available: 6 << 30,
            swap_total: 0,
            swap_free: 0,
        };
        s.mem_psi = None;
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
        yield_list: vec![Matcher::Comm(comm.into())],
        state_dir: Some(state.to_path_buf()),
        ..Config::default()
    }
}

fn engine_for(cfg: &Config, cpu: f64, fg: Option<i32>) -> Engine<Forced> {
    let provider = Forced {
        inner: LinuxProvider::new(cfg),
        cpu,
        fg,
    };
    let journal = Journal::open(
        &cfg.state_dir.clone().unwrap().join("journal"),
        &provider.boot_id(),
    )
    .unwrap();
    Engine::new(cfg.clone(), provider, journal)
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

#[test]
fn yields_every_thread_and_restores_them_all() {
    if !can_run_live() {
        eprintln!("skipped: needs root/CAP_SYS_NICE and python3");
        return;
    }
    let target = spawn_python(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live1");
    let cfg = cfg_for(&target.comm, &state);
    assert_eq!(thread_nices(target.pid()), vec![0, 0, 0, 0]);

    let mut e = engine_for(&cfg, 99.0, None);
    let ev = e.tick(0);
    assert!(
        ev.iter()
            .any(|x| matches!(x, Event::Yielded { pid, .. } if *pid == target.pid())),
        "{ev:?}"
    );
    let nices = thread_nices(target.pid());
    assert!(
        nices.len() >= 4 && nices.iter().all(|&n| n == 15),
        "every thread must be lowered, got {nices:?}"
    );
    assert!(state.join("journal").exists(), "the undo record is on disk");

    // Pressure ends → everything goes back.
    e.provider_mut().cpu = 3.0;
    let mut restored = false;
    for t in 1..12 {
        restored |= e
            .tick(t * 1000)
            .iter()
            .any(|x| matches!(x, Event::Restored { .. }));
    }
    assert!(restored);
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
fn recovers_after_a_crash_from_the_journal_alone() {
    if !can_run_live() {
        eprintln!("skipped: needs root/CAP_SYS_NICE and python3");
        return;
    }
    let target = spawn_python(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live2");
    let cfg = cfg_for(&target.comm, &state);

    let mut first = engine_for(&cfg, 99.0, None);
    first.tick(0);
    assert!(thread_nices(target.pid()).iter().all(|&n| n == 15));
    drop(first); // "kill -9": no shutdown path runs, only the file survives

    let mut second = engine_for(&cfg, 3.0, None);
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
fn the_foreground_process_is_never_lowered() {
    if !can_run_live() {
        eprintln!("skipped: needs root/CAP_SYS_NICE and python3");
        return;
    }
    let target = spawn_python(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live3");
    let cfg = cfg_for(&target.comm, &state);
    let mut e = engine_for(&cfg, 99.0, Some(target.pid()));
    for t in 0..5 {
        e.tick(t * 1000);
    }
    assert!(thread_nices(target.pid()).iter().all(|&n| n == 0));
    assert!(e.journal().is_empty());
}

#[test]
fn a_thread_the_user_reniced_keeps_its_own_value() {
    if !can_run_live() {
        eprintln!("skipped: needs root/CAP_SYS_NICE and python3");
        return;
    }
    let target = spawn_python(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live4");
    let cfg = cfg_for(&target.comm, &state);
    let mut e = engine_for(&cfg, 99.0, None);
    e.tick(0);
    // Somebody sets one worker thread to a value of their own while we have the job lowered.
    let tids = Proc::new().tids(target.pid());
    let odd = *tids.last().unwrap();
    assert_ne!(odd, target.pid());
    sys::set_nice(odd, 7).unwrap();

    e.provider_mut().cpu = 3.0;
    for t in 1..12 {
        e.tick(t * 1000);
    }
    assert_eq!(sys::get_nice(odd).unwrap(), 7, "not ours to change");
    assert_eq!(sys::get_nice(target.pid()).unwrap(), 0);
}

#[test]
fn dry_run_changes_nothing_on_a_real_process() {
    if !can_run_live() {
        eprintln!("skipped: needs root/CAP_SYS_NICE and python3");
        return;
    }
    let target = spawn_python(THREADED_SLEEPER);
    wait_for_threads(target.pid(), 4);
    let state = tmpdir("live5");
    let cfg = Config {
        dry_run: true,
        ..cfg_for(&target.comm, &state)
    };
    let mut e = engine_for(&cfg, 99.0, None);
    let ev = e.tick(0);
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
