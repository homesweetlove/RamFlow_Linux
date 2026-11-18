//! The seam between the policy engine and the operating system.
//!
//! The engine only talks to [`Provider`]; [`LinuxProvider`] implements it on the real system
//! and tests implement it with an in-memory fake.

use crate::config::Config;
use crate::foreground::X11Foreground;
use crate::proc::{cpu_percent, parse_pageout_ranges, CpuTimes, MemInfo, Proc, ProcessSample, Psi};
use crate::sys;
use std::io;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub mem: MemInfo,
    pub mem_psi: Option<Psi>,
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
    /// Ask the kernel to reclaim the process's idle anonymous memory. Returns bytes accepted.
    fn page_out(&mut self, pid: i32) -> io::Result<u64>;
    fn boot_id(&self) -> String;
}

pub struct LinuxProvider {
    proc: Proc,
    prev_cpu: Option<CpuTimes>,
    x11: Option<X11Foreground>,
}

impl LinuxProvider {
    pub fn new(cfg: &Config) -> Self {
        let proc = Proc::new();
        let prev_cpu = proc.cpu_times();
        LinuxProvider {
            proc,
            prev_cpu,
            x11: X11Foreground::detect(cfg.x_display.as_deref(), cfg.x_authority.as_deref()),
        }
    }

    pub fn has_foreground_source(&self) -> bool {
        self.x11.is_some()
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
            cpu_pct,
        })
    }

    fn scan(&mut self) -> Vec<ProcessSample> {
        self.proc
            .pids()
            .into_iter()
            .filter_map(|p| self.proc.process(p))
            .collect()
    }

    fn read(&self, pid: i32) -> Option<ProcessSample> {
        self.proc.process(pid)
    }

    fn exe(&self, pid: i32) -> Option<String> {
        self.proc.exe(pid)
    }

    fn foreground(&mut self) -> Option<i32> {
        self.x11.as_ref()?.active_pid()
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
