//! The seam between the policy engine and the operating system.
//!
//! The engine only talks to [`Provider`]; [`LinuxProvider`] implements it on the real system
//! and tests implement it with an in-memory fake.

use crate::config::Config;
use crate::foreground::{pid_from_command, X11Foreground};
use crate::proc::{cpu_percent, parse_pageout_ranges, CpuTimes, MemInfo, Proc, ProcessSample, Psi};
use crate::sys;
use std::io;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub mem: MemInfo,
    pub mem_psi: Option<Psi>,
    /// System-wide CPU pressure. `some_total_us` is a cumulative counter.
    pub cpu_psi: Option<Psi>,
    pub cpu_pct: f64,
}

pub trait Provider {
    fn snapshot(&mut self) -> io::Result<Snapshot>;
    /// All visible processes.
    fn scan(&mut self) -> Vec<ProcessSample>;
    /// One process, fresh from the system.
    fn read(&self, pid: i32) -> Option<ProcessSample>;
    fn exe(&self, pid: i32) -> Option<String>;
    /// Pid of the process owning the focused window, or `None` when unknown.
    fn foreground(&mut self) -> Option<i32>;
    /// Set the nice value of every thread of the process.
    fn set_nice(&mut self, pid: i32, nice: i32) -> io::Result<()>;
    /// Undo [`Provider::set_nice`]: move every thread that is still at `applied` back to
    /// `original`, leaving threads that were changed by someone else alone.
    fn restore_nice(&mut self, pid: i32, applied: i32, original: i32) -> io::Result<()>;
    /// (autogroup id, autogroup nice) of the session `pid` belongs to; `None` when the kernel
    /// has no (enabled) autogroup.
    fn autogroup(&self, pid: i32) -> Option<(u64, i32)>;
    /// Set the nice value of the whole session `pid` belongs to.
    fn set_autogroup_nice(&mut self, pid: i32, nice: i32) -> io::Result<()>;
    /// Total time (ns) the process's threads spent runnable but waiting for a CPU.
    fn wait_ns(&self, pid: i32) -> Option<u64>;
    /// Ask the kernel to reclaim the process's idle anonymous memory. Returns bytes accepted.
    fn page_out(&mut self, pid: i32) -> io::Result<u64>;
    fn boot_id(&self) -> String;
}

enum ForegroundSource {
    X11(X11Foreground),
    Command(String),
}

pub struct LinuxProvider {
    proc: Proc,
    prev_cpu: Option<CpuTimes>,
    fg: Option<ForegroundSource>,
    autogroup_on: bool,
}

impl LinuxProvider {
    pub fn new(cfg: &Config) -> Self {
        let proc = Proc::new();
        let prev_cpu = proc.cpu_times();
        let fg = match &cfg.foreground_cmd {
            Some(cmd) => Some(ForegroundSource::Command(cmd.clone())),
            None => X11Foreground::detect(cfg.x_display.as_deref(), cfg.x_authority.as_deref())
                .map(ForegroundSource::X11),
        };
        LinuxProvider {
            proc,
            prev_cpu,
            fg,
            autogroup_on: sys::autogroup_enabled(),
        }
    }

    pub fn has_foreground_source(&self) -> bool {
        self.fg.is_some()
    }

    pub fn autogroup_enabled(&self) -> bool {
        self.autogroup_on
    }

    pub fn proc(&self) -> &Proc {
        &self.proc
    }
}

impl Provider for LinuxProvider {
    fn snapshot(&mut self) -> io::Result<Snapshot> {
        let mem = self
            .proc
            .meminfo()
            .ok_or_else(|| io::Error::other("/proc/meminfo 를 읽을 수 없습니다"))?;
        let now = self
            .proc
            .cpu_times()
            .ok_or_else(|| io::Error::other("/proc/stat 를 읽을 수 없습니다"))?;
        let cpu_pct = self.prev_cpu.map(|p| cpu_percent(p, now)).unwrap_or(0.0);
        self.prev_cpu = Some(now);
        Ok(Snapshot {
            mem,
            mem_psi: self.proc.psi("memory"),
            cpu_psi: self.proc.psi("cpu"),
            cpu_pct,
        })
    }

    fn scan(&mut self) -> Vec<ProcessSample> {
        let on = self.autogroup_on;
        self.proc
            .pids()
            .into_iter()
            .filter_map(|p| self.proc.process(p))
            .map(|mut s| {
                if !on {
                    s.autogroup = 0;
                }
                s
            })
            .collect()
    }

    fn read(&self, pid: i32) -> Option<ProcessSample> {
        let mut s = self.proc.process(pid)?;
        if !self.autogroup_on {
            s.autogroup = 0;
        }
        Some(s)
    }

    fn exe(&self, pid: i32) -> Option<String> {
        self.proc.exe(pid)
    }

    fn foreground(&mut self) -> Option<i32> {
        match self.fg.as_ref()? {
            ForegroundSource::X11(x) => x.active_pid(),
            ForegroundSource::Command(c) => pid_from_command(c),
        }
    }

    fn set_nice(&mut self, pid: i32, nice: i32) -> io::Result<()> {
        // The main thread decides success; other threads may legitimately exit meanwhile.
        sys::set_nice(pid, nice)?;
        for tid in self.proc.tids(pid) {
            if tid != pid {
                let _ = sys::set_nice(tid, nice);
            }
        }
        Ok(())
    }

    fn restore_nice(&mut self, pid: i32, applied: i32, original: i32) -> io::Result<()> {
        let mut main_result = Ok(());
        let mut tids = self.proc.tids(pid);
        if !tids.contains(&pid) {
            tids.push(pid);
        }
        for tid in tids {
            if sys::get_nice(tid).map(|n| n == applied).unwrap_or(false) {
                let r = sys::set_nice(tid, original);
                if tid == pid {
                    main_result = r;
                }
            }
        }
        main_result
    }

    fn autogroup(&self, pid: i32) -> Option<(u64, i32)> {
        if !self.autogroup_on {
            return None;
        }
        self.proc.autogroup(pid)
    }

    fn set_autogroup_nice(&mut self, pid: i32, nice: i32) -> io::Result<()> {
        sys::set_autogroup_nice(pid, nice)
    }

    fn wait_ns(&self, pid: i32) -> Option<u64> {
        self.proc.wait_ns(pid)
    }

    fn page_out(&mut self, pid: i32) -> io::Result<u64> {
        let maps = self.proc.maps(pid).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "메모리 맵을 읽을 수 없습니다")
        })?;
        sys::page_out_ranges(pid, &parse_pageout_ranges(&maps))
    }

    fn boot_id(&self) -> String {
        self.proc.boot_id()
    }
}
