# RamFlow for Linux

[![English](https://img.shields.io/badge/Language-English-2F81F7)](README.md)
[![한국어](https://img.shields.io/badge/언어-한국어-2F81F7)](README.ko.md)

A **foreground-first resource orchestrator** for Linux — the Linux counterpart of
[RamFlow for Windows](https://github.com/homesweetlove/RamFlow), redesigned around what Linux actually offers.

> It does not "free RAM". Linux already uses spare memory as cache, and forcing it out only makes things slower.
> RamFlow decides **who gets the CPU first** — and only when somebody is actually being hurt.

## The idea: find the victim first, then the culprit

Most tools lower priorities when the *machine* is busy. A busy machine is not a problem; a program you are
working in that is **waiting for a CPU** is. RamFlow therefore works backwards:

1. **Victim first.** Every sample it measures how much of the CPU time the foreground program *wanted* it spent
   *waiting in the run queue* (`/proc/<pid>/schedstat`). A 100 % busy machine with a calm foreground is left alone.
2. **Then the culprit.** Only when the victim suffers does it look for who to slow down: the jobs you listed
   (`yield`) — or, with `auto = true`, **any process that has been hogging a core**, no list needed.
3. **Graded, with feedback.** It starts gently (nice 10), steps up (15, 19) while the victim keeps suffering,
   steps back down when the suffering is gone (so background jobs get as much CPU as they can), and releases
   everything when the foreground goes quiet.
4. **It checks its own work.** If, at the strongest step, the victim's waiting did not drop, the action did nothing
   useful: RamFlow **puts everything back, says so, and pauses** instead of keeping a pointless change.
5. **It knows how Linux really schedules.** With scheduler *autogroups* (default on desktop kernels) the CPU is
   shared per login session, so lowering one process of another session barely helps. RamFlow lowers the **whole
   session** of a culprit instead — but only if no protected program lives in that session.

I have not done a literature search, so I do not claim nobody has ever combined these; I only know of no tool
that does. What I can show is that it works — see [the measurements](#does-it-actually-help).

## Install

```bash
git clone https://github.com/homesweetlove/RamFlow_Linux && cd RamFlow_Linux
cargo build --release
sudo ./install.sh            # add --enable to start the service right away
ramflow doctor               # shows which features work on this machine
```

Uninstall: `sudo ./uninstall.sh [--purge]` (stopping the service restores everything first).

## Use

```bash
ramflow blame                # measure 3 s: who is waiting, who is hogging, what RamFlow would do
ramflow status               # one-shot view of memory / CPU pressure
ramflow doctor               # feature check
ramflow run                  # watch in dry-run mode: reports, changes nothing
sudo ramflow run --apply     # actually apply
sudo ramflow restore         # undo anything a crashed run left behind
```

`/etc/ramflow/ramflow.conf` ([full example](packaging/ramflow.conf.example)):

```ini
dry_run = true                  # watch the log first
auto = true                     # find CPU hogs by itself (needs to know the foreground)
yield = /usr/bin/ffmpeg         # …and/or name jobs that may always yield
protect = comm:obs              # never touch these
foreground_cmd = hyprctl activewindow -j | jq .pid   # Wayland: print the focused window's pid
```

On X11 the foreground is found automatically (`xprop`). On Wayland, give `foreground_cmd` a command that prints
the focused window's pid. If the foreground is unknown, RamFlow still works with the `yield` list, judging by the
machine-wide CPU stall rate (PSI) instead; `auto` needs a known foreground and stays off without one.

## Safety rules

| Situation | Behavior |
|---|---|
| First start | Dry-run. Needs `--apply` or `dry_run = false` to change anything |
| Nobody is suffering | Nothing is touched, however busy the machine is |
| Before a change | Original value is written to disk atomically **first**; if that fails, nothing is changed |
| Foreground app | Its process, everything it spawned, same-named peers — and any session they live in — are never lowered |
| A session shared with a protected program (sshd, the shell you work in, …) | The session is not lowered as a whole; only the single process is |
| User switches to a lowered job | Restored immediately, even while the machine is still busy |
| Lowering did not help | Everything is put back, an event is logged, and RamFlow pauses before trying again |
| Pressure ends / `SIGTERM` | Everything is restored |
| Crash (`kill -9`, power loss) | Next start, or `ramflow restore`, replays the journal — also when the crash came between two steps |
| User changed the value meanwhile | Their value stands; it is never overwritten |
| PID reuse | Processes are identified by pid **and** start time |
| Reboot | Journal of the previous boot is discarded |
| No privilege to undo | `--apply` is refused (lowering priority is easy and impossible to undo without `CAP_SYS_NICE`) |
| Built-in protected set | systemd, sshd, Xorg, gnome-shell, pipewire, … even if listed or "hogging" |
| `auto` | Off by default; skips system accounts (uid < 1000); needs a known foreground |

Linux keeps nice values **per thread**. RamFlow changes every thread of a job and, on restore, only the threads still at the value it set.

## Optional: trim idle big apps (`trim_idle`, off by default)

Under *critical* memory pressure, asks the kernel to reclaim the idle anonymous memory of large, long-idle, non-foreground user apps (`process_madvise(MADV_PAGEOUT)`, Linux 5.10+).
Needs a known foreground and benefits from swap/zram. One process per sample, with a cooldown.
Memory trouble alone never lowers any priority: there is no evidence that nice helps with memory.

## Does it actually help?

**New research evidence (2026-11-26):** [Cgroup-aware scheduling scalability study](docs/evidence/2026-11-26-cgroup-scaling/README.md) — 2–8 vCPUs, 8 environments, 38,400 operation measurements. Results correspond to the research revision linked in the report.

[`scripts/latency-bench.py`](scripts/latency-bench.py) runs a **victim** (a CPU-bound Python process standing in
for "the program you work in") against 4 CPU-bound **culprits** (2× the cores), with real processes and the real
daemon, nothing simulated. Four arms — *none*, *task* (single processes only), *group* (whole sessions, the
default) and *auto* (no list) — are interleaved over 3 rounds, in three layouts. Medians; "long job" is one
≈150 ms slice of the victim's work; alone, with no culprits, it takes 154–177 ms.
Raw output: [`docs/evidence/2026-11-18-v0.2-effect/`](docs/evidence/2026-11-18-v0.2-effect/).

Real Chrome study (5 paired blocks, search/sort p95 about 75–80 % lower, all raw data published; Korean): [`docs/REAL_CHROME_STUDY.ko.md`](docs/REAL_CHROME_STUDY.ko.md).

**Culprits scattered over many sessions** (several unrelated apps or services — the case where plain `nice` fails)

| Arm | Long job | vs. none | Short-burst p95 | Culprits' CPU |
|---|---:|---:|---:|---:|
| none | 378 ms | — | 2.80 ms | 1.73 / 2 cores |
| task only (what `nice` alone does) | 400 ms | **+6 %** (no help — and RamFlow withdrew it 3 times, as designed) | 2.83 ms | 1.74 |
| **group** (whole sessions) | **157 ms** | **−59 %** | 2.43 ms | 1.62 (−6 %) |
| **auto** (no list at all) | **154 ms** | **−59 %** | 2.57 ms | 1.58 (−9 %) |

**Culprits in the same session as the foreground** (e.g. everything started from one terminal)

| Arm | Long job | vs. none | Culprits' CPU |
|---|---:|---:|---:|
| none | 397 ms | — | 1.73 |
| task | 192 ms | −52 % | 1.58 (−8 %) |
| group (falls back to task: the session is shared) | 173 ms | −57 % | 1.61 (−7 %) |
| auto | 160 ms | −60 % | 1.56 (−10 %) |

**All culprits together in one *other* session** — the kernel's autogroups already protect the foreground
(167 ms vs. 154 ms alone, i.e. barely suffering). RamFlow's demand stayed under its threshold and it
correctly did (almost) nothing; the differences in that layout are within noise and are **not** claimed.

What this shows, and what it does not:

- With a real victim, the long job runs **about as fast as when the machine is otherwise idle**, at the cost of
  roughly 6–10 % of the culprits' throughput (priority is work-conserving: they still use the leftover CPU).
- The measurements justify the two non-obvious design points: **whole-session lowering** (task-only is useless
  across sessions) and **self-verification** (the useless action was detected and undone).
- Short interactive bursts (1.5 ms every 10 ms) gain little: Linux already favours sleepers (p95 differences of
  ±10 % between arms are noise; single rounds ranged 1.6–6.3 ms).
- Caveats: one 2-core VM, kernel 6.18, an extreme synthetic load, a Python probe — not a real browser or editor.
  The `auto` hog threshold (35 % of a core for 5 s, 15 % while the whole machine is saturated) was chosen on a 2-core VM with synthetic culprits. cgroup v2
  `cpu.weight` hosts, other architectures and Wayland compositors were not tested. The numbers in the Windows
  README were measured on Windows and do not carry over.

## Measured overhead

Release build, 2-core / 8 GB Linux box:

| | |
|---|---|
| Binary | 597 KiB |
| Idle (2 s interval, 12 s) | 2.4 MiB resident · 0 CPU ticks · 1 thread |
| While engaged (0.5 s interval) | 2.4 MiB resident, a fraction of a percent of one core |

## Verification

`cargo test` runs 131 tests: 111 unit tests (controller, engine against a fake operating system, journal
format and crash windows, a fake `/proc` tree) and 20 tests with **real processes and the real binary** — all
threads lowered and restored, a real session lowered and restored, crash recovery from the journal alone,
a busy machine without a victim left alone, the foreground never touched, a user-reniced thread left alone,
automatic hog detection, restore on `SIGTERM`, recovery after `kill -9`, dry-run changing nothing. They were
repeated 5 times without a failure. `cargo fmt --check` and `clippy -D warnings` are clean.

## Limitations

- No GUI / tray yet — command line and service only.
- Without a known foreground (Wayland without `foreground_cmd`) RamFlow judges by the machine-wide stall rate
  and cannot tell which program you are working in; `auto` and `trim_idle` stay off.
- Lowering a whole session also slows its other members (a shell in that session, say). Sessions that contain
  protected programs are never lowered as a whole.
- Only nice/autogroup priorities are used; no cgroup `cpu.weight`, no efficiency-core placement, no pagefile logic.
- Tested on x86_64, kernel 6.18 only. The unit file is syntax-checked but was not booted under a real systemd here.
- `process_madvise` was verified to succeed on a real process, but this machine has no swap, so actual RAM reduction was not observed.

Design notes (Korean): [docs/DESIGN.ko.md](docs/DESIGN.ko.md)

## License

MIT — see [LICENSE](LICENSE).
