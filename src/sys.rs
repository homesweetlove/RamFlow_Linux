//! Minimal, dependency-free bindings for the handful of libc calls RamFlow needs.

use std::ffi::{c_int, c_long, c_uint, c_void};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

const PRIO_PROCESS: c_int = 0;
const SC_CLK_TCK: c_int = 2;
const SC_PAGESIZE: c_int = 30;
const SIGINT: c_int = 2;
const SIGTERM: c_int = 15;
const SIGHUP: c_int = 1;
const MADV_PAGEOUT: c_long = 21;
// Same number on every architecture that has them (unified syscall table).
const SYS_PIDFD_OPEN: c_long = 434;
const SYS_PROCESS_MADVISE: c_long = 440;
/// Linux `CAP_SYS_NICE`: needed to lower a nice value again (and for `process_madvise`).
pub const CAP_SYS_NICE: u32 = 23;

#[repr(C)]
struct IoVec {
    base: *mut c_void,
    len: usize,
}

extern "C" {
    fn getpriority(which: c_int, who: c_uint) -> c_int;
    fn setpriority(which: c_int, who: c_uint, prio: c_int) -> c_int;
    fn geteuid() -> c_uint;
    fn sysconf(name: c_int) -> c_long;
    fn syscall(num: c_long, ...) -> c_long;
    fn close(fd: c_int) -> c_int;
    fn signal(signum: c_int, handler: usize) -> usize;
    fn __errno_location() -> *mut c_int;
}

/// Read the nice value of `pid`.
pub fn get_nice(pid: i32) -> io::Result<i32> {
    // getpriority() can legitimately return -1, so errno must be cleared first.
    unsafe {
        *__errno_location() = 0;
        let r = getpriority(PRIO_PROCESS, pid as c_uint);
        if r == -1 && *__errno_location() != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(r)
    }
}

/// Set the nice value of `pid` (process-level; the main thread's id).
pub fn set_nice(pid: i32, nice: i32) -> io::Result<()> {
    let r = unsafe { setpriority(PRIO_PROCESS, pid as c_uint, nice) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub fn effective_uid() -> u32 {
    unsafe { geteuid() }
}

/// Clock ticks per second (`_SC_CLK_TCK`), used to interpret `/proc/<pid>/stat` times.
pub fn clock_ticks() -> u64 {
    let v = unsafe { sysconf(SC_CLK_TCK) };
    if v > 0 {
        v as u64
    } else {
        100
    }
}

pub fn page_size() -> u64 {
    let v = unsafe { sysconf(SC_PAGESIZE) };
    if v > 0 {
        v as u64
    } else {
        4096
    }
}

/// Is capability `bit` set in the `CapEff:` line of a `/proc/<pid>/status` text?
pub fn cap_effective_has(status: &str, bit: u32) -> bool {
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        .map(|mask| bit < 64 && mask & (1u64 << bit) != 0)
        .unwrap_or(false)
}

/// Is the kernel's scheduler autogroup (per-session CPU grouping) switched on?
pub fn autogroup_enabled() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/sched_autogroup_enabled")
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// Set the nice value of the whole autogroup `pid` belongs to (affects every process in that
/// session). The kernel may answer `EAGAIN` when writes come too fast; retry a few times.
pub fn set_autogroup_nice(pid: i32, nice: i32) -> io::Result<()> {
    use std::io::Write;
    let path = format!("/proc/{pid}/autogroup");
    let mut last = io::Error::other("unreachable");
    for attempt in 0..4 {
        let r = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|mut f| f.write_all(format!("{nice}\n").as_bytes()));
        match r {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(11) && attempt < 3 => {
                last = e;
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// Can this process lower a nice value again (i.e. can it undo what it does)?
pub fn can_restore_priority() -> bool {
    if effective_uid() == 0 {
        return true;
    }
    std::fs::read_to_string("/proc/self/status")
        .map(|s| cap_effective_has(&s, CAP_SYS_NICE))
        .unwrap_or(false)
}

/// Ask the kernel to page out the given private anonymous ranges of `pid`
/// (`process_madvise(MADV_PAGEOUT)`, Linux 5.10+, needs `CAP_SYS_NICE`).
/// Returns the number of bytes the kernel accepted for reclaim.
pub fn page_out_ranges(pid: i32, ranges: &[(usize, usize)]) -> io::Result<u64> {
    if ranges.is_empty() {
        return Ok(0);
    }
    unsafe {
        let fd = syscall(SYS_PIDFD_OPEN, pid as c_long, 0 as c_long);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = fd as c_int;
        let mut total: u64 = 0;
        let mut result = Ok(());
        // UIO_MAXIOV is 1024; stay well below it.
        for chunk in ranges.chunks(512) {
            let iov: Vec<IoVec> = chunk
                .iter()
                .map(|&(start, end)| IoVec {
                    base: start as *mut c_void,
                    len: end - start,
                })
                .collect();
            let r = syscall(
                SYS_PROCESS_MADVISE,
                fd as c_long,
                iov.as_ptr() as c_long,
                iov.len() as c_long,
                MADV_PAGEOUT,
                0 as c_long,
            );
            if r < 0 {
                result = Err(io::Error::last_os_error());
                break;
            }
            total += r as u64;
        }
        close(fd);
        result.map(|()| total)
    }
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: c_int) {
    // Only an atomic store: async-signal-safe.
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Route SIGINT/SIGTERM/SIGHUP to a flag so the daemon can restore before exiting.
pub fn install_shutdown_handlers() {
    let handler = on_signal as extern "C" fn(c_int) as usize;
    unsafe {
        signal(SIGINT, handler);
        signal(SIGTERM, handler);
        signal(SIGHUP, handler);
    }
}

/// For one-shot commands whose output may be cut short (`ramflow blame | head`): end quietly
/// when the reader goes away. Not used by the daemon, which must survive and clean up.
pub fn quiet_on_closed_pipe() {
    const SIGPIPE: c_int = 13;
    const SIG_DFL: usize = 0;
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_bits_are_parsed() {
        let root = "Name:\tx\nCapEff:\t000001ffffffffff\n";
        assert!(cap_effective_has(root, CAP_SYS_NICE));
        let none = "CapEff:\t0000000000000000\n";
        assert!(!cap_effective_has(none, CAP_SYS_NICE));
        let only_net = "CapEff:\t0000000000003000\n";
        assert!(!cap_effective_has(only_net, CAP_SYS_NICE));
        let nice = "CapEff:\t0000000000800000\n";
        assert!(cap_effective_has(nice, CAP_SYS_NICE));
        assert!(!cap_effective_has("nothing here", CAP_SYS_NICE));
    }

    #[test]
    fn own_nice_is_readable() {
        let n = get_nice(std::process::id() as i32).unwrap();
        assert!((-20..=19).contains(&n));
    }

    #[test]
    fn units_are_sane() {
        assert!(clock_ticks() >= 1);
        assert!(page_size().is_power_of_two());
    }
}
