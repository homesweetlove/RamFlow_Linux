# RamFlow for Linux

[![English](https://img.shields.io/badge/Language-English-2F81F7)](README.md)
[![한국어](https://img.shields.io/badge/언어-한국어-2F81F7)](README.ko.md)

A **foreground-first resource orchestrator** for Linux. It is the Linux counterpart of
[RamFlow for Windows](https://github.com/homesweetlove/RamFlow), re-designed around what Linux actually offers.

When the machine gets busy (CPU saturated, or memory stalling), RamFlow lowers the priority of the background jobs **you listed** (video encodes, compiles, …) so the app you are using stays responsive — and puts **everything it changed back** when the pressure ends.

> It does not "free RAM". Linux already uses spare memory as cache, and forcing it out only makes things slower.
> What RamFlow does is decide **who gets the resources first**.

- **Light** — 540 KiB binary, ~2.4 MiB resident when idle, 0 % CPU, one thread, **zero third-party crates**.
- **Safe by construction** — dry-run by default, undo record written *before* every change, restore on pressure end / shutdown / crash.
- **Pressure-aware** — uses the kernel's PSI (pressure stall information), so "memory is low" and "work is actually stalling" are not confused.

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
ramflow status               # one-shot view of memory / CPU pressure
ramflow doctor               # feature check
ramflow run                  # watch in dry-run mode: reports, changes nothing
sudo ramflow run --apply     # actually apply
sudo ramflow restore         # undo anything a crashed run left behind
```

`/etc/ramflow/ramflow.conf` ([full example](packaging/ramflow.conf.example)):

```ini
dry_run = true                  # watch the log first
yield = /usr/bin/ffmpeg         # jobs allowed to yield (absolute path, or comm:name)
yield = comm:cc1plus
protect = comm:obs              # never touch these
```

Only what you list is ever touched.

## Safety rules

| Situation | Behavior |
|---|---|
| First start | Dry-run. Needs `--apply` or `dry_run = false` to change anything |
| Before a change | Original value is written to disk atomically **first**; if that fails, nothing is changed |
| Foreground app (X11) | Its process, everything it spawned, and same-named peers are never lowered |
| User switches to a lowered job | Restored immediately, even while the machine is still busy |
| Pressure ends / `SIGTERM` | Everything is restored |
| Crash (`kill -9`, power loss) | Next start, or `ramflow restore`, replays the journal |
| User changed the value meanwhile | Their value stands; it is never overwritten |
| PID reuse | Processes are identified by pid **and** start time |
| Reboot | Journal of the previous boot is discarded |
| No privilege to undo | `--apply` is refused (lowering priority is easy to do and impossible to undo without `CAP_SYS_NICE`) |
| Built-in protected set | systemd, sshd, Xorg, gnome-shell, pipewire, … even if listed |

Linux keeps nice values **per thread**. RamFlow changes every thread of a job and, on restore, only the threads still at the value it set.

## Optional: trim idle big apps (`trim_idle`, off by default)

Under *critical* memory pressure, asks the kernel to reclaim the idle anonymous memory of large, long-idle, non-foreground user apps (`process_madvise(MADV_PAGEOUT)`, Linux 5.10+).
Needs X11 (to know the foreground; **Wayland hides other clients' windows by design** — when the foreground is unknown this feature simply stays off) and benefits from swap/zram. One process per sample, with a cooldown.

## Measured overhead

Release build, measured on the 2-core / 8 GB / 68-process Linux box this was developed on:

| | |
|---|---|
| Binary | 540 KiB |
| Idle (2 s interval, 20 s) | 2.4 MiB resident · 0 CPU ticks · 1 thread |
| Under 100 % CPU load, yielding (0.5 s interval, 20 s) | 2.4 MiB resident · 0.15 % CPU |

## Does it actually help?

[`scripts/latency-bench.py`](scripts/latency-bench.py) measures a "foreground" Python probe while 4 CPU-bound background jobs (2× the cores) compete, alternating *without* / *with* RamFlow for 5 rounds each ([raw output](docs/evidence/2026-11-18-linux-latency/run.txt)). Medians:

| Foreground work | Without | With | |
|---|---:|---:|---|
| Long job (≈ a build/render step, one slice) | 408 ms | 157 ms | **−62 %**, faster in all 5 rounds |
| Short bursts (1.5 ms every 10 ms), p95 | 2.89 ms | 2.59 ms | within noise (single rounds ranged 2.5–5.5 ms) |
| Short bursts, p50 | 1.50 ms | 1.51 ms | no change |

So the benefit is real for **sustained foreground work that would otherwise share the CPU with background jobs**. For tiny, mostly-sleeping interactive tasks the Linux scheduler already favours sleepers, so there is little to gain — an earlier 3-round run showed −49 % on the short-burst p95 and this 5-round run −10 %, which is why that row is called noise. Everything was restored on `SIGTERM` in every round.

Caveats: one 2-core VM, kernel 6.18, an extreme synthetic load, a Python probe — not a real browser or editor. Background jobs lose some throughput by design (1.74 → 1.61 of 2 cores were busy in the background). The 45–49 % figures in the Windows README were measured on Windows and do not carry over.

## Verification

`cargo test` runs 96 tests: 80 unit tests (incl. a fake `/proc` tree) and 16 tests with **real processes and the real binary** — all threads lowered and restored, crash recovery from the journal alone, foreground never touched, a user-reniced thread left alone, restore on `SIGTERM`, recovery after `kill -9`, dry-run changing nothing. `cargo fmt --check`, `clippy -D warnings`, `systemd-analyze verify` and an install/uninstall round trip were run locally and are also set up in GitHub Actions.

## Limitations

- No GUI / tray yet — command line and service only.
- Foreground protection is X11-only; on Wayland the explicit `yield` list is the safety net (hence "only what you list").
- No equivalents yet of fullscreen detection, efficiency-core placement or pagefile management.
- Tested on x86_64, kernel 6.18 only. Other architectures/distros untested. The unit file is syntax-checked but was not booted under a real systemd here.
- `process_madvise` was verified to succeed on a real process, but this machine has no swap, so actual RAM reduction was not observed.

Design notes (Korean): [docs/DESIGN.ko.md](docs/DESIGN.ko.md)

## License

MIT — see [LICENSE](LICENSE).
