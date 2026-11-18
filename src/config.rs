//! Configuration: a tiny `key = value` file (repeatable keys for lists, `#` comments).

use std::fmt;
use std::path::PathBuf;

/// Kernel `comm` names are truncated to 15 bytes.
const COMM_MAX: usize = 15;

pub(crate) fn trunc_comm(s: &str) -> &str {
    if s.len() <= COMM_MAX {
        return s;
    }
    let mut end = COMM_MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// How a process is named in the config: by absolute executable path, or by `comm:` name.
#[derive(Debug, Clone, PartialEq)]
pub enum Matcher {
    Path(String),
    Comm(String),
}

impl Matcher {
    pub fn parse(s: &str) -> Result<Matcher, String> {
        let s = s.trim();
        if let Some(name) = s.strip_prefix("comm:") {
            let name = name.trim();
            if name.is_empty() || name.contains('/') {
                return Err(format!("잘못된 comm 이름: '{s}'"));
            }
            return Ok(Matcher::Comm(name.to_string()));
        }
        if s.starts_with('/') && s.len() > 1 && !s.ends_with('/') {
            return Ok(Matcher::Path(s.to_string()));
        }
        Err(format!(
            "'{s}' — 실행 파일 전체 경로(/usr/bin/ffmpeg) 또는 comm:이름 형식이어야 합니다"
        ))
    }

    /// Cheap pre-filter on the kernel's truncated `comm`, so the executable path only has to
    /// be resolved for processes that could possibly match.
    pub fn matches_comm(&self, comm: &str) -> bool {
        match self {
            Matcher::Comm(n) => trunc_comm(n) == comm,
            Matcher::Path(p) => {
                let base = p.rsplit('/').next().unwrap_or(p);
                trunc_comm(base) == comm
            }
        }
    }

    /// Full match: `comm` pre-filter plus, for paths, the resolved executable.
    pub fn matches(&self, comm: &str, exe: impl FnOnce() -> Option<String>) -> bool {
        if !self.matches_comm(comm) {
            return false;
        }
        match self {
            Matcher::Comm(_) => true,
            Matcher::Path(p) => exe().as_deref() == Some(p.as_str()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Observe and report only; change nothing. This is the default.
    pub dry_run: bool,
    pub interval_ms: u64,
    /// Background jobs allowed to yield under contention.
    pub yield_list: Vec<Matcher>,
    /// Extra processes that must never be touched (on top of the built-in list).
    pub protect: Vec<Matcher>,
    /// Nice value background jobs are moved to (1..=19).
    pub nice_target: i32,
    pub cpu_enter: f64,
    pub cpu_leave: f64,
    pub mem_enter: f64,
    pub mem_leave: f64,
    pub critical_enter: f64,
    pub critical_leave: f64,
    /// Consecutive samples required to enter or leave a state.
    pub confirm: u32,
    pub trim_idle: bool,
    pub idle_secs: u64,
    pub trim_min_rss_mib: u64,
    pub trim_min_uid: u32,
    pub trim_cooldown_secs: u64,
    pub x_display: Option<String>,
    pub x_authority: Option<String>,
    pub state_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            dry_run: true,
            interval_ms: 2000,
            yield_list: Vec::new(),
            protect: Vec::new(),
            nice_target: 15,
            cpu_enter: 80.0,
            cpu_leave: 60.0,
            mem_enter: 55.0,
            mem_leave: 35.0,
            critical_enter: 80.0,
            critical_leave: 60.0,
            confirm: 3,
            trim_idle: false,
            idle_secs: 1800,
            trim_min_rss_mib: 200,
            trim_min_uid: 1000,
            trim_cooldown_secs: 600,
            x_display: None,
            x_authority: None,
            state_dir: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigError {
    /// 1-based line number, or 0 for whole-file validation errors.
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line > 0 {
            write!(f, "설정 {}번째 줄: {}", self.line, self.message)
        } else {
            write!(f, "설정 오류: {}", self.message)
        }
    }
}

impl std::error::Error for ConfigError {}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn strip_comment(line: &str) -> &str {
    let line = line.trim();
    if line.starts_with('#') {
        return "";
    }
    match line.find(" #") {
        Some(i) => line[..i].trim_end(),
        None => line,
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let mut c = Config::default();
        for (idx, raw) in text.lines().enumerate() {
            let line_no = idx + 1;
            let line = strip_comment(raw);
            if line.is_empty() {
                continue;
            }
            let err = |m: String| ConfigError {
                line: line_no,
                message: m,
            };
            let Some((key, value)) = line.split_once('=') else {
                return Err(err(format!("'키 = 값' 형식이 아닙니다: '{line}'")));
            };
            let (key, value) = (key.trim(), value.trim());
            let num = |name: &str| -> Result<f64, ConfigError> {
                value
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| err(format!("{name}: 숫자가 필요합니다 ('{value}')")))
            };
            let int = |name: &str| -> Result<u64, ConfigError> {
                value
                    .parse::<u64>()
                    .map_err(|_| err(format!("{name}: 0 이상의 정수가 필요합니다 ('{value}')")))
            };
            match key {
                "dry_run" => {
                    c.dry_run = parse_bool(value).ok_or_else(|| {
                        err(format!("dry_run: true/false 가 필요합니다 ('{value}')"))
                    })?
                }
                "trim_idle" => {
                    c.trim_idle = parse_bool(value).ok_or_else(|| {
                        err(format!("trim_idle: true/false 가 필요합니다 ('{value}')"))
                    })?
                }
                "interval_ms" => c.interval_ms = int("interval_ms")?,
                "nice_target" => {
                    c.nice_target = value
                        .parse::<i32>()
                        .map_err(|_| err(format!("nice_target: 정수가 필요합니다 ('{value}')")))?
                }
                "cpu_enter" => c.cpu_enter = num("cpu_enter")?,
                "cpu_leave" => c.cpu_leave = num("cpu_leave")?,
                "mem_enter" => c.mem_enter = num("mem_enter")?,
                "mem_leave" => c.mem_leave = num("mem_leave")?,
                "critical_enter" => c.critical_enter = num("critical_enter")?,
                "critical_leave" => c.critical_leave = num("critical_leave")?,
                "confirm" => c.confirm = int("confirm")?.min(u32::MAX as u64) as u32,
                "idle_secs" => c.idle_secs = int("idle_secs")?,
                "trim_min_rss_mib" => c.trim_min_rss_mib = int("trim_min_rss_mib")?,
                "trim_min_uid" => c.trim_min_uid = int("trim_min_uid")?.min(u32::MAX as u64) as u32,
                "trim_cooldown_secs" => c.trim_cooldown_secs = int("trim_cooldown_secs")?,
                "yield" => c.yield_list.push(Matcher::parse(value).map_err(err)?),
                "protect" => c.protect.push(Matcher::parse(value).map_err(err)?),
                "x_display" => c.x_display = Some(value.to_string()),
                "x_authority" => c.x_authority = Some(value.to_string()),
                "state_dir" => c.state_dir = Some(PathBuf::from(value)),
                other => return Err(err(format!("알 수 없는 설정 항목: '{other}'"))),
            }
        }
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |m: &str| {
            Err(ConfigError {
                line: 0,
                message: m.to_string(),
            })
        };
        if !(200..=60_000).contains(&self.interval_ms) {
            return bad("interval_ms 는 200~60000 사이여야 합니다");
        }
        if !(1..=19).contains(&self.nice_target) {
            return bad("nice_target 은 1~19 사이여야 합니다");
        }
        if self.confirm == 0 {
            return bad("confirm 은 1 이상이어야 합니다");
        }
        for (name, enter, leave) in [
            ("cpu", self.cpu_enter, self.cpu_leave),
            ("mem", self.mem_enter, self.mem_leave),
            ("critical", self.critical_enter, self.critical_leave),
        ] {
            if !(0.0..=100.0).contains(&enter) || !(0.0..=100.0).contains(&leave) {
                return bad(&format!(
                    "{name}_enter/{name}_leave 는 0~100 사이여야 합니다"
                ));
            }
            if leave >= enter {
                return bad(&format!("{name}_leave 는 {name}_enter 보다 작아야 합니다 (진입·해제 사이에 여유가 있어야 흔들리지 않습니다)"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_safe() {
        let c = Config::default();
        assert!(c.validate().is_ok());
        assert!(c.dry_run, "must default to observe-only");
        assert!(!c.trim_idle, "trimming must be opt-in");
    }

    #[test]
    fn parses_full_file() {
        let text = "\
# comment
dry_run = false
interval_ms = 1000   # trailing comment
nice_target = 12
yield = /usr/bin/ffmpeg
yield = comm:make
protect = comm:obs
cpu_enter = 85
cpu_leave = 50
trim_idle = yes
x_display = :0
state_dir = /tmp/rf
";
        let c = Config::parse(text).unwrap();
        assert!(!c.dry_run);
        assert_eq!(c.interval_ms, 1000);
        assert_eq!(c.nice_target, 12);
        assert_eq!(
            c.yield_list,
            vec![
                Matcher::Path("/usr/bin/ffmpeg".into()),
                Matcher::Comm("make".into())
            ]
        );
        assert_eq!(c.protect, vec![Matcher::Comm("obs".into())]);
        assert_eq!(c.cpu_enter, 85.0);
        assert!(c.trim_idle);
        assert_eq!(c.x_display.as_deref(), Some(":0"));
        assert_eq!(c.state_dir, Some(PathBuf::from("/tmp/rf")));
    }

    #[test]
    fn rejects_unknown_key_with_line_number() {
        let e = Config::parse("dry_run = true\nbogus = 1\n").unwrap_err();
        assert_eq!(e.line, 2);
        assert!(e.message.contains("bogus"));
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Config::parse("interval_ms = fast").is_err());
        assert!(Config::parse("dry_run = maybe").is_err());
        assert!(Config::parse("nice_target = 25").is_err());
        assert!(Config::parse("nice_target = 0").is_err());
        assert!(Config::parse("interval_ms = 10").is_err());
        assert!(Config::parse("confirm = 0").is_err());
        assert!(Config::parse("just a line").is_err());
        assert!(Config::parse("cpu_enter = 50\ncpu_leave = 60").is_err());
        assert!(Config::parse("cpu_enter = 150").is_err());
        assert!(Config::parse("cpu_enter = NaN").is_err());
    }

    #[test]
    fn yield_requires_absolute_path_or_comm() {
        assert!(Config::parse("yield = ffmpeg").is_err());
        assert!(Config::parse("yield = ./ffmpeg").is_err());
        assert!(Config::parse("yield = comm:").is_err());
        assert!(Config::parse("yield = comm:a/b").is_err());
        assert!(Config::parse("yield = /").is_err());
        assert!(Config::parse("yield = /usr/bin/").is_err());
    }

    #[test]
    fn matcher_handles_comm_truncation() {
        let m = Matcher::parse("/usr/bin/a-very-long-program-name").unwrap();
        assert!(m.matches_comm("a-very-long-pro"));
        assert!(!m.matches_comm("a-very-long-pro2"));
        let c = Matcher::parse("comm:firefox").unwrap();
        assert!(c.matches_comm("firefox"));
        assert!(!c.matches_comm("firefox-bin"));
    }

    #[test]
    fn path_matcher_checks_resolved_exe() {
        let m = Matcher::parse("/usr/bin/ffmpeg").unwrap();
        assert!(m.matches("ffmpeg", || Some("/usr/bin/ffmpeg".into())));
        // Same name, different binary: must not match.
        assert!(!m.matches("ffmpeg", || Some("/tmp/evil/ffmpeg".into())));
        assert!(!m.matches("ffmpeg", || None));
        // Pre-filter avoids resolving the path at all for other names.
        assert!(!m.matches("bash", || panic!("must not resolve")));
    }

    #[test]
    fn multibyte_comm_truncation_does_not_panic() {
        let m = Matcher::parse("comm:한글프로그램이름이길어요").unwrap();
        let _ = m.matches_comm("한글프로그램");
    }
}
