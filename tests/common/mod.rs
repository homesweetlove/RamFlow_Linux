//! Helpers for tests that run against real processes on the real system.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub struct Spawned {
    pub child: Child,
    /// Unique kernel `comm` name, so a test can target exactly this process.
    pub comm: String,
    dir: PathBuf,
}

impl Spawned {
    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var("PATH")
        .ok()?
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

/// Can the tests that change priorities run here at all?
pub fn can_run_live() -> bool {
    ramflow::sys::can_restore_priority() && find_in_path("python3").is_some()
}

/// Start `python3 -c code` under a unique name (a symlink to the interpreter), so its kernel
/// `comm` is unique and nothing else on the machine can match it.
pub fn spawn_python(code: &str) -> Spawned {
    spawn_python_in(code, false)
}

/// Like [`spawn_python`], but the child starts its own session (setsid), which on a kernel with
/// scheduler autogroups also gives it its own autogroup — like a job started from another
/// terminal tab.
pub fn spawn_python_session(code: &str) -> Spawned {
    spawn_python_in(code, true)
}

extern "C" {
    fn setsid() -> i32;
}

fn spawn_python_in(code: &str, new_session: bool) -> Spawned {
    use std::os::unix::process::CommandExt;
    let py = std::fs::canonicalize(find_in_path("python3").expect("python3")).unwrap();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let comm = format!("rfl{}x{}", std::process::id() % 100000, n);
    assert!(comm.len() <= 15);
    let dir = std::env::temp_dir().join(format!("ramflow-live-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let link = dir.join(&comm);
    std::os::unix::fs::symlink(&py, &link).unwrap();
    let mut cmd = Command::new(&link);
    cmd.arg("-c")
        .arg(code)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if new_session {
        // SAFETY: setsid is async-signal-safe and touches no memory of ours.
        unsafe {
            cmd.pre_exec(|| {
                setsid();
                Ok(())
            });
        }
    }
    let child = cmd.spawn().expect("spawn");
    Spawned { child, comm, dir }
}

pub const THREADED_SLEEPER: &str = "import threading, time\nfor _ in range(3):\n    threading.Thread(target=time.sleep, args=(300,), daemon=True).start()\ntime.sleep(300)\n";
pub const BUSY_LOOP: &str = "while True:\n    pass\n";

pub fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

pub fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ramflow-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
