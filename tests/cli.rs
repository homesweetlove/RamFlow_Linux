//! End-to-end tests of the real binary on the real system.

mod common;

use common::*;
use ramflow::proc::Proc;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ramflow"))
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn help_and_version() {
    let o = bin().arg("help").output().unwrap();
    assert!(o.status.success());
    assert!(text(&o).contains("ramflow run"));
    let o = bin().arg("version").output().unwrap();
    assert!(text(&o).starts_with("ramflow "));
    let o = bin().output().unwrap();
    assert!(o.status.success(), "no arguments shows help");
}

#[test]
fn unknown_command_and_option_fail() {
    let o = bin().arg("frobnicate").output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    let o = bin().args(["run", "--bogus"]).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).contains("알 수 없는 옵션"));
    let o = bin().args(["run", "--config"]).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn status_reports_this_machine() {
    let o = bin().arg("status").output().unwrap();
    assert!(o.status.success(), "{}", text(&o));
    let t = text(&o);
    assert!(
        t.contains("메모리") && t.contains("CPU") && t.contains("압박"),
        "{t}"
    );
}

#[test]
fn doctor_runs() {
    let o = bin().arg("doctor").output().unwrap();
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("PSI"));
}

#[test]
fn bad_config_is_reported_with_its_line() {
    let d = tmpdir("badcfg");
    let f = d.join("bad.conf");
    std::fs::write(&f, "dry_run = true\nnice_target = 99\n").unwrap();
    let o = bin()
        .args(["run", "--once", "--config"])
        .arg(&f)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).contains("nice_target"), "{}", text(&o));
    let missing = bin()
        .args(["run", "--once", "--config", "/nonexistent/x.conf"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
}

#[test]
fn restore_with_nothing_to_do() {
    let d = tmpdir("emptyrestore");
    let o = bin()
        .args(["restore", "--state-dir"])
        .arg(&d)
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(text(&o).contains("되돌릴 변경이 없습니다"));
}

#[test]
fn dry_run_once_exits_cleanly() {
    let d = tmpdir("dryonce");
    let o = bin()
        .args(["run", "--once", "--state-dir"])
        .arg(&d)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o));
    let t = text(&o);
    assert!(t.contains("예행연습") && t.contains("종료"), "{t}");
}

fn nice_of(pid: i32) -> Option<i32> {
    Proc::new().process(pid).map(|p| p.nice)
}

fn write_config(dir: &Path, targets: &[&Spawned]) -> std::path::PathBuf {
    let mut c = String::from("dry_run = false\ninterval_ms = 200\nconfirm = 2\ncpu_enter = 55\ncpu_leave = 30\nnice_target = 15\n");
    for t in targets {
        c.push_str(&format!("yield = comm:{}\n", t.comm));
    }
    let f = dir.join("test.conf");
    std::fs::write(&f, c).unwrap();
    f
}

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_daemon(conf: &Path, state: &Path) -> Daemon {
    Daemon(
        bin()
            .args(["run", "--apply", "--config"])
            .arg(conf)
            .arg("--state-dir")
            .arg(state)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}

fn saturate() -> Vec<Spawned> {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    (0..n).map(|_| spawn_python(BUSY_LOOP)).collect()
}

#[test]
fn real_load_gets_lowered_and_sigterm_restores_everything() {
    if !can_run_live() {
        eprintln!("skipped: needs root/CAP_SYS_NICE and python3");
        return;
    }
    let jobs = saturate();
    let refs: Vec<&Spawned> = jobs.iter().collect();
    let d = tmpdir("e2e-term");
    let conf = write_config(&d, &refs);
    let mut daemon = start_daemon(&conf, &d);

    assert!(
        wait_until(Duration::from_secs(30), || jobs.iter().all(|j| nice_of(
            j.pid()
        ) == Some(
            15
        ))),
        "background jobs hogging the CPU should be lowered to nice 15"
    );
    assert!(d.join("journal").exists());

    // Graceful stop: SIGTERM must put everything back before exiting.
    Command::new("kill")
        .args(["-TERM", &daemon.0.id().to_string()])
        .status()
        .unwrap();
    let status = daemon.0.wait().unwrap();
    assert_eq!(status.code(), Some(0), "clean exit");
    for j in &jobs {
        assert_eq!(nice_of(j.pid()), Some(0), "restored on shutdown");
    }
    assert!(!d.join("journal").exists());
}

#[test]
fn kill_dash_nine_is_recovered_by_restore() {
    if !can_run_live() {
        eprintln!("skipped: needs root/CAP_SYS_NICE and python3");
        return;
    }
    let jobs = saturate();
    let refs: Vec<&Spawned> = jobs.iter().collect();
    let d = tmpdir("e2e-kill");
    let conf = write_config(&d, &refs);
    let mut daemon = start_daemon(&conf, &d);
    assert!(wait_until(Duration::from_secs(30), || jobs
        .iter()
        .all(|j| nice_of(j.pid()) == Some(15))));

    // Hard kill: nothing gets a chance to clean up.
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    assert!(
        jobs.iter().all(|j| nice_of(j.pid()) == Some(15)),
        "left lowered by the crash"
    );
    assert!(d.join("journal").exists());

    let o = bin()
        .args(["restore", "--state-dir"])
        .arg(&d)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o));
    for j in &jobs {
        assert_eq!(nice_of(j.pid()), Some(0));
    }
    assert!(!d.join("journal").exists());
}

#[test]
fn dry_run_daemon_leaves_real_load_alone() {
    if !can_run_live() {
        eprintln!("skipped: needs root/CAP_SYS_NICE and python3");
        return;
    }
    let jobs = saturate();
    let refs: Vec<&Spawned> = jobs.iter().collect();
    let d = tmpdir("e2e-dry");
    let conf = write_config(&d, &refs);
    let text_conf = std::fs::read_to_string(&conf)
        .unwrap()
        .replace("dry_run = false", "dry_run = true");
    std::fs::write(&conf, text_conf).unwrap();
    let mut daemon = Daemon(
        bin()
            .args(["run", "--config"])
            .arg(&conf)
            .arg("--state-dir")
            .arg(&d)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_secs(4));
    Command::new("kill")
        .args(["-TERM", &daemon.0.id().to_string()])
        .status()
        .unwrap();
    let out = daemon.0.wait_with_output_compat();
    assert!(
        out.contains("예정"),
        "it should announce what it would do: {out}"
    );
    for j in &jobs {
        assert_eq!(
            nice_of(j.pid()),
            Some(0),
            "dry-run must not change anything"
        );
    }
}

trait WaitOut {
    fn wait_with_output_compat(&mut self) -> String;
}
impl WaitOut for Child {
    fn wait_with_output_compat(&mut self) -> String {
        use std::io::Read;
        let mut s = String::new();
        if let Some(mut o) = self.stdout.take() {
            let _ = o.read_to_string(&mut s);
        }
        let _ = self.wait();
        s
    }
}
