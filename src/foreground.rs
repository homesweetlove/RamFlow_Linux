//! Finding the window the user is looking at.
//!
//! Linux has no single answer: X11 exposes it through `_NET_ACTIVE_WINDOW`, while Wayland
//! deliberately hides other clients' windows from every program. RamFlow therefore asks X11
//! (through the `xprop` tool, so there is no X library to link) and otherwise reports
//! "unknown". "Unknown" is never treated as "nothing is in the foreground": features that need
//! to know (idle trimming) stay off, and only jobs the user explicitly listed are touched.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Parse `xprop -root _NET_ACTIVE_WINDOW` → window id such as `0x4a00007`.
pub fn parse_active_window(out: &str) -> Option<String> {
    let id = out.split('#').nth(1)?.trim().split(',').next()?.trim();
    let hex = id.strip_prefix("0x")?;
    if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if u64::from_str_radix(hex, 16).ok()? == 0 {
        return None; // no active window
    }
    Some(id.to_string())
}

/// Parse `xprop -id <w> _NET_WM_PID` → pid.
pub fn parse_wm_pid(out: &str) -> Option<i32> {
    let pid: i32 = out.split('=').nth(1)?.trim().parse().ok()?;
    (pid > 0).then_some(pid)
}

fn run(cmd: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut s = String::new();
                child.stdout.take()?.read_to_string(&mut s).ok()?;
                return Some(s);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct X11Foreground {
    display: String,
    authority: Option<String>,
}

impl X11Foreground {
    /// Build from the config values, falling back to the environment. `None` when there is
    /// no X display to ask (for example a system service without `x_display`).
    pub fn detect(display: Option<&str>, authority: Option<&str>) -> Option<Self> {
        let display = display
            .map(str::to_string)
            .or_else(|| std::env::var("DISPLAY").ok())?;
        if display.is_empty() {
            return None;
        }
        let authority = authority
            .map(str::to_string)
            .or_else(|| std::env::var("XAUTHORITY").ok());
        Some(X11Foreground { display, authority })
    }

    fn xprop(&self, args: &[&str]) -> Option<String> {
        let mut c = Command::new("xprop");
        c.args(args).env("DISPLAY", &self.display);
        if let Some(a) = &self.authority {
            c.env("XAUTHORITY", a);
        }
        run(&mut c, Duration::from_millis(500))
    }

    /// Pid of the process that owns the active window.
    pub fn active_pid(&self) -> Option<i32> {
        let id = parse_active_window(&self.xprop(&["-root", "_NET_ACTIVE_WINDOW"])?)?;
        parse_wm_pid(&self.xprop(&["-id", &id, "_NET_WM_PID"])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_window_id_is_extracted() {
        assert_eq!(
            parse_active_window("_NET_ACTIVE_WINDOW(WINDOW): window id # 0x4a00007\n").as_deref(),
            Some("0x4a00007")
        );
        // Some window managers append extra ids.
        assert_eq!(
            parse_active_window("_NET_ACTIVE_WINDOW(WINDOW): window id # 0x1c00003, 0x0\n")
                .as_deref(),
            Some("0x1c00003")
        );
    }

    #[test]
    fn no_active_window_is_none() {
        assert_eq!(
            parse_active_window("_NET_ACTIVE_WINDOW(WINDOW): window id # 0x0\n"),
            None
        );
        assert_eq!(parse_active_window(""), None);
        assert_eq!(
            parse_active_window("_NET_ACTIVE_WINDOW:  not found.\n"),
            None
        );
        assert_eq!(parse_active_window("window id # banana"), None);
    }

    #[test]
    fn wm_pid_is_extracted() {
        assert_eq!(parse_wm_pid("_NET_WM_PID(CARDINAL) = 12345\n"), Some(12345));
        assert_eq!(parse_wm_pid("_NET_WM_PID:  not found.\n"), None);
        assert_eq!(parse_wm_pid("_NET_WM_PID(CARDINAL) = 0\n"), None);
        assert_eq!(parse_wm_pid("_NET_WM_PID(CARDINAL) = -3\n"), None);
    }

    #[test]
    fn run_returns_output_and_times_out() {
        let ok = run(Command::new("echo").arg("hi"), Duration::from_secs(2)).unwrap();
        assert_eq!(ok.trim(), "hi");
        let t = Instant::now();
        assert!(run(Command::new("sleep").arg("5"), Duration::from_millis(100)).is_none());
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "a hung X server must not hang the daemon"
        );
        assert!(run(
            &mut Command::new("definitely-not-a-real-binary-xyz"),
            Duration::from_millis(100)
        )
        .is_none());
    }
}
