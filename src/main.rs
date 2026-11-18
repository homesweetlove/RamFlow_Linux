use ramflow::config::Config;
use ramflow::engine::{Engine, Event};
use ramflow::fmt::bytes;
use ramflow::journal::Journal;
use ramflow::policy::{memory_pressure, Signals};
use ramflow::provider::{LinuxProvider, Provider};
use ramflow::sys;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_CONFIG: &str = "/etc/ramflow/ramflow.conf";

const HELP: &str = "\
RamFlow (리눅스) — 지금 쓰는 작업을 먼저 지켜주는 자원 관리 도구

사용법:
  ramflow status                     현재 메모리·CPU 압박 상태를 한 번 보여줍니다
  ramflow blame [--secs N]           지금 누가 누구를 느리게 하는지 N초(기본 3) 재서 보여줍니다
  ramflow doctor                     이 컴퓨터에서 어떤 기능을 쓸 수 있는지 점검합니다
  ramflow run [옵션]                 감시를 시작합니다 (기본은 예행연습: 아무것도 바꾸지 않음)
  ramflow restore [옵션]             저널에 남은 변경을 모두 원래대로 되돌립니다
  ramflow version                    버전을 보여줍니다

run / restore 옵션:
  --config <파일>        설정 파일 (기본: /etc/ramflow/ramflow.conf, 없으면 기본값)
  --state-dir <폴더>     저널을 둘 폴더 (기본: root는 /var/lib/ramflow)
  --apply                (run) 예행연습을 끄고 실제로 적용합니다
  --once                 (run) 한 번만 측정하고 끝냅니다

실제 적용은 root 또는 CAP_SYS_NICE 권한이 있을 때만 허용됩니다.
되돌릴 수 없는 상태로 만들지 않기 위해서입니다.
";

struct Opts {
    config: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    apply: bool,
    once: bool,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        config: None,
        state_dir: None,
        apply: false,
        once: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => {
                o.config = Some(PathBuf::from(
                    it.next().ok_or("--config 뒤에 파일 경로가 필요합니다")?,
                ))
            }
            "--state-dir" => {
                o.state_dir = Some(PathBuf::from(
                    it.next().ok_or("--state-dir 뒤에 폴더 경로가 필요합니다")?,
                ))
            }
            "--apply" => o.apply = true,
            "--once" => o.once = true,
            other => return Err(format!("알 수 없는 옵션: {other}")),
        }
    }
    Ok(o)
}

fn load_config(o: &Opts) -> Result<Config, String> {
    let (path, required) = match &o.config {
        Some(p) => (p.clone(), true),
        None => (PathBuf::from(DEFAULT_CONFIG), false),
    };
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(text) => Config::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Config::default(),
        Err(e) => return Err(format!("{} 를 읽을 수 없습니다: {e}", path.display())),
    };
    if o.apply {
        cfg.dry_run = false;
    }
    if o.state_dir.is_some() {
        cfg.state_dir = o.state_dir.clone();
    }
    Ok(cfg)
}

fn state_dir(cfg: &Config) -> PathBuf {
    if let Some(d) = &cfg.state_dir {
        return d.clone();
    }
    if sys::effective_uid() == 0 {
        return PathBuf::from("/var/lib/ramflow");
    }
    if let Ok(x) = std::env::var("XDG_STATE_HOME") {
        if !x.is_empty() {
            return Path::new(&x).join("ramflow");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    Path::new(&home).join(".local/state/ramflow")
}

fn open_journal(cfg: &Config, boot: &str) -> Journal {
    let path = state_dir(cfg).join("journal");
    match Journal::open(&path, boot) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("경고: 저널({})을 열 수 없습니다: {e}", path.display());
            if cfg.dry_run {
                Journal::in_memory(boot)
            } else {
                eprintln!("저널 없이는 안전하게 되돌릴 수 없으므로 중단합니다.");
                std::process::exit(1);
            }
        }
    }
}

fn print_events(events: &[Event]) {
    for e in events {
        match e {
            Event::RestoreFailed { .. }
            | Event::ApplyFailed { .. }
            | Event::TrimFailed { .. }
            | Event::JournalError(_)
            | Event::SensorError(_) => eprintln!("경고: {e}"),
            _ => println!("{e}"),
        }
    }
}

fn sleep_until(deadline: Instant) {
    while !sys::shutdown_requested() {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(100)));
    }
}

fn cmd_run(args: &[String]) -> i32 {
    let opts = match parse_opts(args) {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    let cfg = match load_config(&opts) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    if !cfg.dry_run && !sys::can_restore_priority() {
        return fail(
            "실제 적용은 root 또는 CAP_SYS_NICE 권한이 필요합니다. 낮춘 우선순위를 다시 되돌릴 수 없으면 \
             안전하지 않기 때문입니다. sudo 로 실행하거나 systemd 서비스를 사용하세요.",
        );
    }
    if cfg.yield_list.is_empty() && !cfg.trim_idle && !cfg.auto {
        eprintln!(
            "안내: 설정에 양보할 작업(yield)이 없고 자동 분류(auto)와 메모리 정리도 꺼져 있어, 관찰만 합니다."
        );
    }

    let provider = LinuxProvider::new(&cfg);
    let boot = provider.boot_id();
    let journal = open_journal(&cfg, &boot);
    let interval = Duration::from_millis(cfg.interval_ms);
    let dry_run = cfg.dry_run;
    let has_fg = provider.has_foreground_source();
    let trim = cfg.trim_idle;
    let auto = cfg.auto;
    let mut engine = Engine::new(cfg, provider, journal);

    sys::install_shutdown_handlers();
    println!(
        "RamFlow {} 시작 — {}",
        env!("CARGO_PKG_VERSION"),
        if dry_run {
            "예행연습 모드 (아무것도 바꾸지 않습니다)"
        } else {
            "적용 모드"
        }
    );
    if trim && !has_fg {
        eprintln!("안내: 앞에 있는 창을 알 수 없어 메모리 정리는 동작하지 않습니다.");
    }
    if !has_fg {
        eprintln!(
            "안내: 앞에 있는 창을 알 수 없습니다 (X11 없음, foreground_cmd 없음). 이 경우 \
             시스템 전체의 CPU 정체율만 보고 판단하며, 자동 분류(auto)는 동작하지 않습니다."
        );
    } else if auto {
        println!(
            "자동 분류 켜짐: 목록에 없어도 코어를 오래 독차지하는 프로그램을 찾아 양보시킵니다."
        );
    }
    // Crash recovery: undo whatever an earlier run left behind.
    print_events(&engine.restore_everything());

    let start = Instant::now();
    let mut next = start
        + if opts.once {
            Duration::from_millis(300)
        } else {
            interval
        };
    sleep_until(next);
    while !sys::shutdown_requested() {
        let events = engine.tick(start.elapsed().as_millis() as u64);
        print_events(&events);
        if opts.once {
            break;
        }
        next += interval;
        if next < Instant::now() {
            next = Instant::now() + interval; // we fell behind (suspend, heavy load): do not burst
        }
        sleep_until(next);
    }

    let events = engine.restore_everything();
    print_events(&events);
    if !engine.journal().is_empty() {
        eprintln!("경고: 일부 변경을 되돌리지 못했습니다. 'ramflow restore' 로 다시 시도하세요.");
        return 2;
    }
    println!("RamFlow 종료 — 바꾼 것은 모두 원래대로 돌려놓았습니다.");
    0
}

fn cmd_restore(args: &[String]) -> i32 {
    let opts = match parse_opts(args) {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    let mut cfg = match load_config(&opts) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    cfg.dry_run = false;
    let provider = LinuxProvider::new(&cfg);
    let journal = match Journal::open(&state_dir(&cfg).join("journal"), &provider.boot_id()) {
        Ok(j) => j,
        Err(e) => return fail(&format!("저널을 열 수 없습니다: {e}")),
    };
    if journal.is_empty() {
        println!("되돌릴 변경이 없습니다.");
        return 0;
    }
    let mut engine = Engine::new(cfg, provider, journal);
    print_events(&engine.restore_everything());
    if engine.journal().is_empty() {
        println!("모두 원래대로 돌려놓았습니다.");
        0
    } else {
        eprintln!("일부는 되돌리지 못했습니다. root 권한으로 다시 실행해 보세요.");
        2
    }
}

fn cmd_blame(args: &[String]) -> i32 {
    let mut secs = 3u64;
    let mut cfg_path: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--secs" => match it.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(n) if (1..=30).contains(&n) => secs = n,
                _ => return fail("--secs 뒤에는 1~30 사이의 숫자가 필요합니다"),
            },
            "--config" => match it.next() {
                Some(v) => cfg_path = Some(PathBuf::from(v)),
                None => return fail("--config 뒤에 파일 경로가 필요합니다"),
            },
            other => return fail(&format!("알 수 없는 옵션: {other}")),
        }
    }
    let cfg = match load_config(&Opts {
        config: cfg_path,
        state_dir: None,
        apply: false,
        once: false,
    }) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let mut p = LinuxProvider::new(&cfg);
    let clk = sys::clock_ticks().max(1) as f64;
    let first_procs = p.scan();
    let fg = p.foreground();
    let fg_wait0 = fg.and_then(|f| p.wait_ns(f));
    let snap0 = p.snapshot().ok();
    let t0 = Instant::now();
    println!("RamFlow 범인 찾기 — {secs}초 동안 재는 중...");
    std::thread::sleep(Duration::from_secs(secs));
    let dt = t0.elapsed().as_secs_f64();
    let procs = p.scan();
    let snap1 = p.snapshot().ok();

    let stall = match (
        snap0.as_ref().and_then(|s| s.cpu_psi),
        snap1.as_ref().and_then(|s| s.cpu_psi),
    ) {
        (Some(a), Some(b)) => Some(
            (b.some_total_us.saturating_sub(a.some_total_us) as f64 / (dt * 1e6) * 100.0)
                .clamp(0.0, 100.0),
        ),
        _ => None,
    };
    let fg_sample = fg.and_then(|f| procs.iter().find(|s| s.pid == f));
    let mut victim_suffers = false;
    match (fg_sample, fg_wait0, fg.and_then(|f| p.wait_ns(f))) {
        (Some(s), Some(w0), Some(w1)) => {
            let before = first_procs.iter().find(|x| x.identity() == s.identity());
            let wait = w1.saturating_sub(w0) as f64 / (dt * 1e9) * 100.0;
            let run = before
                .map(|b| s.ticks.saturating_sub(b.ticks) as f64 / clk / dt * 100.0)
                .unwrap_or(0.0);
            let activity = wait + run;
            println!("\n앞에 있는 프로그램: {}({})", s.comm, s.pid);
            if activity >= 5.0 {
                let ratio = wait / activity * 100.0;
                println!("  CPU를 쓰려 한 시간 중 {ratio:.0}% 를 차례를 기다리느라 보냈습니다 (CPU 사용 {run:.0}%, 기다림 {wait:.0}%)");
                victim_suffers = ratio >= cfg.demand_enter;
            } else {
                println!(
                    "  지금은 거의 일하지 않습니다 (CPU 사용 {run:.0}%) — 느려질 일이 없습니다"
                );
            }
        }
        _ => println!(
            "\n앞에 있는 프로그램을 알 수 없습니다 (X11이 아니거나 foreground_cmd 가 없음)."
        ),
    }
    if let Some(s) = stall {
        println!("시스템 CPU 정체율: {s:.0}% (일하려는 작업 중 하나라도 기다린 시간의 비율)");
    }

    let protected = ramflow::engine::protected_set(&procs, fg);
    let mut rows: Vec<(f64, &ramflow::proc::ProcessSample)> = procs
        .iter()
        .filter_map(|s| {
            let b = first_procs.iter().find(|x| x.identity() == s.identity())?;
            let pct = s.ticks.saturating_sub(b.ticks) as f64 / clk / dt * 100.0;
            (pct >= 5.0).then_some((pct, s))
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("\nCPU를 많이 쓰는 프로그램 (코어 1개 = 100%)");
    if rows.is_empty() {
        println!("  없음");
    }
    let fg_group = fg_sample.map(|s| s.autogroup).unwrap_or(0);
    for (pct, s) in rows.iter().take(8) {
        let tag = if protected.contains(&s.pid) {
            "앞에서 쓰는 중 — 건드리지 않음".to_string()
        } else if s.autogroup != 0 && s.autogroup != fg_group {
            format!("다른 세션({}) — 세션 전체를 낮출 수 있음", s.autogroup)
        } else {
            "같은 세션 — 프로세스 단위로만 낮출 수 있음".to_string()
        };
        let listed = cfg
            .yield_list
            .iter()
            .any(|m| m.matches(&s.comm, || p.exe(s.pid)));
        println!(
            "  {pct:>4.0}%  {}({})  {}{}",
            s.comm,
            s.pid,
            tag,
            if listed { "  [yield 목록]" } else { "" }
        );
    }
    println!();
    if victim_suffers {
        println!("판단: 앞의 프로그램이 실제로 기다리고 있습니다. 위 목록의 앞쪽 프로그램이 범인 후보입니다.");
    } else if fg_sample.is_some() {
        println!("판단: 지금 앞의 프로그램은 괴로워하지 않습니다. 이때는 RamFlow 가 아무도 건드리지 않습니다.");
    } else {
        println!("판단: 앞의 프로그램을 몰라 누가 피해자인지 알 수 없습니다.");
    }
    0
}

fn cmd_status() -> i32 {
    let cfg = Config::default();
    let mut p = LinuxProvider::new(&cfg);
    std::thread::sleep(Duration::from_millis(250)); // CPU use needs two readings
    let snap = match p.snapshot() {
        Ok(s) => s,
        Err(e) => return fail(&e.to_string()),
    };
    let m = &snap.mem;
    println!("RamFlow 상태");
    println!(
        "  메모리   사용 가능 {} / 전체 {} ({:.0}%)",
        bytes(m.available),
        bytes(m.total),
        m.available_pct()
    );
    match m.swap_used_pct() {
        Some(pct) => println!(
            "  스왑     {} 사용 / {} ({pct:.0}%)",
            bytes(m.swap_total - m.swap_free),
            bytes(m.swap_total)
        ),
        None => println!("  스왑     없음"),
    }
    println!("  CPU      {:.0}% 사용 중", snap.cpu_pct);
    match &snap.mem_psi {
        Some(psi) => println!(
            "  압박(PSI) 일부 지연 {:.2}% · 전체 지연 {:.2}%  (최근 10초 평균)",
            psi.some_avg10, psi.full_avg10
        ),
        None => {
            println!("  압박(PSI) 이 커널에서는 제공되지 않음 (사용 가능 메모리만으로 판단합니다)")
        }
    }
    let score = memory_pressure(&Signals::from_readings(m, snap.mem_psi.as_ref()));
    let label = match score {
        s if s < 20.0 => "여유",
        s if s < 55.0 => "보통",
        s if s < 80.0 => "빠듯함",
        _ => "위험",
    };
    println!("  메모리 압박 점수  {score:.0}/100 ({label})");

    let mut procs = p.scan();
    procs.sort_by_key(|s| std::cmp::Reverse(s.rss));
    println!("\n메모리를 많이 쓰는 프로세스 (상위 5개)");
    for s in procs.iter().take(5) {
        println!(
            "  {:>9}  {}({}) 우선순위값 {}",
            bytes(s.rss),
            s.comm,
            s.pid,
            s.nice
        );
    }
    0
}

fn mark(ok: Option<bool>) -> &'static str {
    match ok {
        Some(true) => "[확인]",
        Some(false) => "[불가]",
        None => "[주의]",
    }
}

fn cmd_doctor() -> i32 {
    let cfg = Config::default();
    let p = LinuxProvider::new(&cfg);
    println!("RamFlow 점검\n");

    let psi = p.proc().psi("memory").is_some();
    println!(
        "{} 메모리 압박 정보(PSI) — {}",
        mark(Some(psi)),
        if psi {
            "정확한 압박 판단이 가능합니다"
        } else {
            "없으면 사용 가능 메모리만으로 판단합니다 (커널 4.20 이상, psi=1 필요할 수 있음)"
        }
    );
    let cpu_psi = p.proc().psi("cpu").is_some();
    println!(
        "{} CPU 정체 정보(PSI) — {}",
        mark(Some(cpu_psi)),
        if cpu_psi {
            "시스템이 얼마나 CPU를 기다리는지 바로 잴 수 있습니다"
        } else {
            "없으면 CPU 사용률이 90% 이상일 때만 개입을 검토합니다"
        }
    );
    let ag = p.autogroup_enabled();
    println!(
        "{} 세션 단위 스케줄링(autogroup) — {}",
        mark(Some(ag)),
        if ag {
            "켜져 있음: 다른 세션의 작업은 세션 전체를 낮춰야 실제로 효과가 납니다 (use_autogroup = true)"
        } else {
            "꺼져 있음: 프로세스 단위 우선순위만으로 충분히 효과가 납니다"
        }
    );
    let priv_ok = sys::can_restore_priority();
    println!(
        "{} 우선순위를 되돌릴 권한 — {}",
        mark(Some(priv_ok)),
        if priv_ok {
            "실제 적용이 가능합니다"
        } else {
            "root 또는 CAP_SYS_NICE 가 없어 예행연습만 가능합니다"
        }
    );
    let swap = p
        .proc()
        .meminfo()
        .map(|m| m.swap_total > 0)
        .unwrap_or(false);
    println!(
        "{} 스왑/zram — {}",
        mark(if swap { Some(true) } else { None }),
        if swap {
            "유휴 메모리 정리(trim_idle)가 실제로 효과를 냅니다"
        } else {
            "스왑이 없으면 메모리 정리는 파일 캐시만 회수할 수 있어 효과가 작습니다"
        }
    );
    let fg = p.has_foreground_source();
    println!(
        "{} 앞에 있는 창 알아내기 — {}",
        mark(if fg { Some(true) } else { None }),
        if fg {
            "X11 (xprop)로 확인합니다"
        } else {
            "DISPLAY 가 없습니다. Wayland 에서는 설정의 foreground_cmd 에 '앞 창의 pid 를 출력하는 명령'을 적어 주세요 (없어도 시스템 전체 정체율로 양보는 동작합니다)"
        }
    );
    let xprop = std::process::Command::new("xprop")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    println!(
        "{} xprop 도구 — {}",
        mark(if xprop { Some(true) } else { None }),
        if xprop {
            "설치됨"
        } else {
            "없음 (x11-utils / xorg-xprop 패키지)"
        }
    );
    let madv = {
        let me = std::process::id() as i32;
        let r = p
            .proc()
            .maps(me)
            .map(|m| ramflow::proc::parse_pageout_ranges(&m))
            .unwrap_or_default();
        // Probe on one of our own anonymous ranges (a harmless hint to the kernel).
        sys::page_out_ranges(me, &r[..r.len().min(1)]).is_ok()
    };
    println!(
        "{} 프로세스 메모리 회수 호출(process_madvise) — {}",
        mark(Some(madv)),
        if madv {
            "사용 가능"
        } else {
            "사용 불가 (커널 5.10 미만이거나 권한 부족)"
        }
    );
    let dir = state_dir(&cfg);
    let writable = std::fs::create_dir_all(&dir).is_ok()
        && std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(".probe"))
            .is_ok();
    let _ = std::fs::remove_file(dir.join(".probe"));
    println!(
        "{} 저널 저장 폴더 {} — {}",
        mark(Some(writable)),
        dir.display(),
        if writable {
            "쓸 수 있습니다"
        } else {
            "쓸 수 없습니다 (--state-dir 로 바꾸세요)"
        }
    );
    let cfg_exists = Path::new(DEFAULT_CONFIG).exists();
    println!(
        "{} 설정 파일 {} — {}",
        mark(if cfg_exists { Some(true) } else { None }),
        DEFAULT_CONFIG,
        if cfg_exists {
            "있음"
        } else {
            "없음 (기본값 사용: 예행연습, 양보 대상 없음)"
        }
    );
    0
}

fn fail(msg: &str) -> i32 {
    eprintln!("오류: {msg}");
    1
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("run") {
        sys::quiet_on_closed_pipe();
    }
    let code = match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("restore") => cmd_restore(&args[1..]),
        Some("status") => cmd_status(),
        Some("blame") => cmd_blame(&args[1..]),
        Some("doctor") => cmd_doctor(),
        Some("version") | Some("--version") | Some("-V") => {
            println!("ramflow {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("help") | Some("--help") | Some("-h") | None => {
            print!("{HELP}");
            0
        }
        Some(other) => {
            eprintln!("알 수 없는 명령: {other}\n");
            eprint!("{HELP}");
            1
        }
    };
    std::process::exit(code);
}
