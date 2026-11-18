//! Readers and parsers for `/proc`. All parsers are pure functions over text so they can be
//! tested against fixtures; [`Proc`] adds the file access on top and can be pointed at a fake
//! root directory in tests.

use crate::sys;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl MemInfo {
    pub fn available_pct(&self) -> f64 {
        if self.total == 0 {
            return 100.0;
        }
        self.available as f64 * 100.0 / self.total as f64
    }

    /// `None` when the machine has no swap at all.
    pub fn swap_used_pct(&self) -> Option<f64> {
        if self.swap_total == 0 {
            return None;
        }
        let used = self.swap_total.saturating_sub(self.swap_free);
        Some(used as f64 * 100.0 / self.swap_total as f64)
    }
}

/// Parse `/proc/meminfo`. Requires `MemTotal` and `MemAvailable` (kernel 3.14+).
pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut m = MemInfo::default();
    let (mut has_total, mut has_avail) = (false, false);
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let kib: u64 = match rest.split_whitespace().next().and_then(|v| v.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        let bytes = kib.saturating_mul(1024);
        match key {
            "MemTotal" => {
                m.total = bytes;
                has_total = true;
            }
            "MemAvailable" => {
                m.available = bytes;
                has_avail = true;
            }
            "SwapTotal" => m.swap_total = bytes,
            "SwapFree" => m.swap_free = bytes,
            _ => {}
        }
    }
    (has_total && has_avail).then_some(m)
}

/// Pressure Stall Information: share of time tasks were stalled (percent, averaged).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Psi {
    pub some_avg10: f64,
    pub some_avg60: f64,
    pub full_avg10: f64,
    pub full_avg60: f64,
}

/// Parse `/proc/pressure/{memory,cpu,io}`. The `full` line is optional (older kernels, cpu).
pub fn parse_psi(text: &str) -> Option<Psi> {
    let mut psi = Psi::default();
    let mut has_some = false;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let kind = parts.next()?;
        let (mut a10, mut a60) = (None, None);
        for p in parts {
            if let Some(v) = p.strip_prefix("avg10=") {
                a10 = v.parse::<f64>().ok();
            } else if let Some(v) = p.strip_prefix("avg60=") {
                a60 = v.parse::<f64>().ok();
            }
        }
        match (kind, a10, a60) {
            ("some", Some(a), Some(b)) => {
                psi.some_avg10 = a;
                psi.some_avg60 = b;
                has_some = true;
            }
            ("full", Some(a), Some(b)) => {
                psi.full_avg10 = a;
                psi.full_avg60 = b;
            }
            _ => {}
        }
    }
    has_some.then_some(psi)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

/// Parse the aggregate `cpu ` line of `/proc/stat`.
pub fn parse_cpu_times(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8) // user nice system idle iowait irq softirq steal (guest is already in user)
        .filter_map(|x| x.parse().ok())
        .collect();
    if v.len() < 4 {
        return None;
    }
    let total: u64 = v.iter().sum();
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    Some(CpuTimes {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// Busy percentage between two samples. 0 when no time has passed.
pub fn cpu_percent(prev: CpuTimes, cur: CpuTimes) -> f64 {
    let dt = cur.total.saturating_sub(prev.total);
    if dt == 0 {
        return 0.0;
    }
    let db = cur.busy.saturating_sub(prev.busy);
    (db as f64 * 100.0 / dt as f64).clamp(0.0, 100.0)
}

/// Process identity: a pid alone can be reused, pid + start time cannot (within one boot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity {
    pub pid: i32,
    pub start: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessSample {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub state: char,
    pub nice: i32,
    /// Start time in clock ticks since boot.
    pub start: u64,
    /// utime + stime in clock ticks.
    pub ticks: u64,
    /// Resident set size in bytes.
    pub rss: u64,
    pub uid: u32,
}

impl ProcessSample {
    pub fn identity(&self) -> Identity {
        Identity {
            pid: self.pid,
            start: self.start,
        }
    }

    pub fn is_kernel_thread(&self) -> bool {
        self.pid == 2 || self.ppid == 2
    }
}

/// Parse `/proc/<pid>/stat`. `comm` may contain spaces and parentheses, so the fields after it
/// are located from the *last* `)`.
pub fn parse_pid_stat(text: &str, page_size: u64) -> Option<ProcessSample> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close < open {
        return None;
    }
    let pid: i32 = text[..open].trim().parse().ok()?;
    let comm = text[open + 1..close].to_string();
    let rest: Vec<&str> = text[close + 1..].split_whitespace().collect();
    // rest[0] is field 3 (state); field n is rest[n - 3].
    let field = |n: usize| rest.get(n - 3).copied();
    let state = field(3)?.chars().next()?;
    let ppid: i32 = field(4)?.parse().ok()?;
    let utime: u64 = field(14)?.parse().ok()?;
    let stime: u64 = field(15)?.parse().ok()?;
    let nice: i32 = field(19)?.parse().ok()?;
    let start: u64 = field(22)?.parse().ok()?;
    let rss_pages: i64 = field(24)?.parse().ok()?;
    Some(ProcessSample {
        pid,
        ppid,
        comm,
        state,
        nice,
        start,
        ticks: utime + stime,
        rss: rss_pages.max(0) as u64 * page_size,
        uid: 0,
    })
}

/// Parse `/proc/<pid>/maps` and return the private anonymous read-write ranges
/// (heap and anonymous mmaps) — the memory that is safe and useful to page out.
pub fn parse_pageout_ranges(maps: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for line in maps.lines() {
        let mut f = line.split_whitespace();
        let (Some(range), Some(perms), Some(_off), Some(_dev), Some(inode)) =
            (f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        let name = f.next(); // None for anonymous mappings
        if perms != "rw-p" || inode != "0" {
            continue;
        }
        if let Some(n) = name {
            if n != "[heap]" {
                continue; // [stack], [vvar], [vdso], files...
            }
        }
        let Some((a, b)) = range.split_once('-') else {
            continue;
        };
        let (Ok(a), Ok(b)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16)) else {
            continue;
        };
        if b > a {
            out.push((a, b));
        }
    }
    out
}

/// Access to a `/proc` tree (the real one, or a fixture directory in tests).
#[derive(Debug, Clone)]
pub struct Proc {
    root: PathBuf,
    page_size: u64,
}

impl Proc {
    pub fn new() -> Self {
        Self::at("/proc")
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Proc {
            root: root.into(),
            page_size: sys::page_size(),
        }
    }

    fn read(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.root.join(rel)).ok()
    }

    pub fn meminfo(&self) -> Option<MemInfo> {
        parse_meminfo(&self.read("meminfo")?)
    }

    /// `kind` is "memory", "cpu" or "io". `None` when PSI is not available on this kernel.
    pub fn psi(&self, kind: &str) -> Option<Psi> {
        parse_psi(&self.read(&format!("pressure/{kind}"))?)
    }

    pub fn cpu_times(&self) -> Option<CpuTimes> {
        parse_cpu_times(&self.read("stat")?)
    }

    pub fn boot_id(&self) -> String {
        self.read("sys/kernel/random/boot_id")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    pub fn pids(&self) -> Vec<i32> {
        let Ok(rd) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut v: Vec<i32> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()))
            .collect();
        v.sort_unstable();
        v
    }

    pub fn process(&self, pid: i32) -> Option<ProcessSample> {
        let dir = self.root.join(pid.to_string());
        let text = fs::read_to_string(dir.join("stat")).ok()?;
        let mut s = parse_pid_stat(&text, self.page_size)?;
        if s.pid != pid {
            return None;
        }
        s.uid = fs::metadata(&dir).map(|m| m.uid()).unwrap_or(u32::MAX);
        Some(s)
    }

    /// Absolute path of the executable, if readable (kernel threads and other users'
    /// processes without privileges return `None`).
    pub fn exe(&self, pid: i32) -> Option<String> {
        let p = fs::read_link(self.root.join(pid.to_string()).join("exe")).ok()?;
        let s = p.to_string_lossy().into_owned();
        Some(
            s.strip_suffix(" (deleted)")
                .map(str::to_string)
                .unwrap_or(s),
        )
    }

    /// Thread ids of a process. Linux keeps the nice value per *thread*, so changing only the
    /// main thread would leave a multi-threaded job almost untouched.
    pub fn tids(&self, pid: i32) -> Vec<i32> {
        let Ok(rd) = fs::read_dir(self.root.join(pid.to_string()).join("task")) else {
            return Vec::new();
        };
        let mut v: Vec<i32> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()))
            .collect();
        v.sort_unstable();
        v
    }

    pub fn maps(&self, pid: i32) -> Option<String> {
        self.read(&format!("{pid}/maps"))
    }
}

impl Default for Proc {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "MemTotal:       16384000 kB\nMemFree:         1000000 kB\nMemAvailable:    8192000 kB\nSwapTotal:       2048000 kB\nSwapFree:        1024000 kB\nHugePages_Total:       0\n";

    #[test]
    fn meminfo_is_parsed_in_bytes() {
        let m = parse_meminfo(MEMINFO).unwrap();
        assert_eq!(m.total, 16_384_000 * 1024);
        assert_eq!(m.available, 8_192_000 * 1024);
        assert!((m.available_pct() - 50.0).abs() < 1e-9);
        assert!((m.swap_used_pct().unwrap() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn meminfo_without_swap_has_no_swap_percentage() {
        let m = parse_meminfo(
            "MemTotal: 100 kB\nMemAvailable: 50 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        )
        .unwrap();
        assert_eq!(m.swap_used_pct(), None);
    }

    #[test]
    fn meminfo_requires_available() {
        assert!(parse_meminfo("MemTotal: 100 kB\n").is_none());
        assert!(parse_meminfo("").is_none());
    }

    #[test]
    fn psi_parses_some_and_full() {
        let t = "some avg10=1.50 avg60=0.75 avg300=0.10 total=12345\nfull avg10=0.25 avg60=0.10 avg300=0.00 total=99\n";
        let p = parse_psi(t).unwrap();
        assert_eq!(p.some_avg10, 1.5);
        assert_eq!(p.some_avg60, 0.75);
        assert_eq!(p.full_avg10, 0.25);
    }

    #[test]
    fn psi_cpu_without_full_line_is_ok() {
        let p = parse_psi("some avg10=3.00 avg60=2.00 avg300=1.00 total=5\n").unwrap();
        assert_eq!(p.some_avg10, 3.0);
        assert_eq!(p.full_avg10, 0.0);
    }

    #[test]
    fn psi_garbage_is_rejected() {
        assert!(parse_psi("").is_none());
        assert!(parse_psi("hello world").is_none());
    }

    #[test]
    fn cpu_percent_between_samples() {
        let a = parse_cpu_times("cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 1 1 1 1\n").unwrap();
        let b = parse_cpu_times("cpu  200 0 200 1600 0 0 0 0 0 0\n").unwrap();
        assert_eq!(a.total, 1000);
        assert_eq!(a.busy, 200);
        assert!((cpu_percent(a, b) - 20.0).abs() < 1e-9);
        assert_eq!(cpu_percent(a, a), 0.0);
    }

    #[test]
    fn cpu_iowait_counts_as_idle() {
        let t = parse_cpu_times("cpu 10 0 10 70 10 0 0 0\n").unwrap();
        assert_eq!(t.total, 100);
        assert_eq!(t.busy, 20);
    }

    fn stat_line(comm: &str) -> String {
        // 52 fields, mimicking a real kernel line.
        format!(
            "4242 ({comm}) S 1 4242 4242 0 -1 4194304 1000 0 0 0 150 50 0 0 20 5 3 0 987654 123456789 2500 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n"
        )
    }

    #[test]
    fn pid_stat_fields_are_located_correctly() {
        let s = parse_pid_stat(&stat_line("ffmpeg"), 4096).unwrap();
        assert_eq!(s.pid, 4242);
        assert_eq!(s.comm, "ffmpeg");
        assert_eq!(s.state, 'S');
        assert_eq!(s.ppid, 1);
        assert_eq!(s.ticks, 200);
        assert_eq!(s.nice, 5);
        assert_eq!(s.start, 987_654);
        assert_eq!(s.rss, 2500 * 4096);
    }

    #[test]
    fn comm_with_spaces_and_parens_is_handled() {
        let s = parse_pid_stat(&stat_line("Web Content (x)"), 4096).unwrap();
        assert_eq!(s.comm, "Web Content (x)");
        assert_eq!(s.nice, 5);
        assert_eq!(s.start, 987_654);
    }

    #[test]
    fn truncated_stat_is_rejected() {
        assert!(parse_pid_stat("42 (x) S 1", 4096).is_none());
        assert!(parse_pid_stat("", 4096).is_none());
        assert!(parse_pid_stat("garbage", 4096).is_none());
    }

    #[test]
    fn negative_nice_is_supported() {
        let line = stat_line("rt").replace(" 20 5 3 ", " 0 -10 3 ");
        assert_eq!(parse_pid_stat(&line, 4096).unwrap().nice, -10);
    }

    #[test]
    fn pageout_ranges_pick_only_private_anonymous_memory() {
        let maps = "\
55d0c0a00000-55d0c0a21000 r--p 00000000 08:01 1234 /usr/bin/app
55d0c1000000-55d0c1100000 rw-p 00000000 00:00 0 [heap]
7f0000000000-7f0000100000 rw-p 00000000 00:00 0
7f0000200000-7f0000300000 rw-s 00000000 00:00 0
7f0000400000-7f0000500000 rw-p 00000000 08:01 99 /usr/lib/libx.so
7ffc00000000-7ffc00021000 rw-p 00000000 00:00 0 [stack]
7ffc00100000-7ffc00104000 r--p 00000000 00:00 0 [vvar]
";
        let r = parse_pageout_ranges(maps);
        assert_eq!(
            r,
            vec![
                (0x55d0c1000000, 0x55d0c1100000),
                (0x7f0000000000, 0x7f0000100000)
            ]
        );
    }

    #[test]
    fn fake_proc_tree_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ramflow-fakeproc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("4242")).unwrap();
        fs::create_dir_all(dir.join("self")).unwrap();
        fs::create_dir_all(dir.join("pressure")).unwrap();
        fs::write(dir.join("4242/stat"), stat_line("worker")).unwrap();
        fs::write(dir.join("meminfo"), MEMINFO).unwrap();
        fs::write(dir.join("stat"), "cpu 1 0 1 8 0 0 0 0\n").unwrap();
        fs::write(dir.join("pressure/memory"), "some avg10=2.00 avg60=1.00 avg300=0.50 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n").unwrap();
        let p = Proc::at(&dir);
        assert_eq!(p.pids(), vec![4242]);
        assert_eq!(p.process(4242).unwrap().comm, "worker");
        assert!(p.process(1).is_none());
        assert!(p.meminfo().is_some());
        assert_eq!(p.psi("memory").unwrap().some_avg10, 2.0);
        assert!(p.psi("io").is_none());
        assert_eq!(p.cpu_times().unwrap().total, 10);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_proc_is_readable() {
        let p = Proc::new();
        let me = std::process::id() as i32;
        let s = p.process(me).expect("own process must be readable");
        assert_eq!(s.pid, me);
        assert!(s.rss > 0);
        assert!(p.meminfo().is_some());
        assert!(p.cpu_times().is_some());
        assert!(p.exe(me).is_some());
    }
}
