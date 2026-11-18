#!/usr/bin/env python3
"""Does RamFlow actually help the program you are working in?  (stdlib only, needs root)

The "foreground" is a separate process (the victim) that does a fixed amount of CPU work over
and over and records how long each piece takes. Meanwhile 2x-cores busy loops (the culprits)
compete for the CPU. Everything is measured, nothing is simulated.

Three layouts, because they behave very differently on Linux (scheduler "autogroups" give
every login session its own fair share of the CPU):
  same       culprits run in the same session as the foreground (e.g. same terminal)
  separate   all culprits share ONE other session (e.g. one other terminal tab)
  scattered  every culprit has its own session (e.g. several unrelated apps / services)

Choose layouts with RF_LAYOUTS=same,scattered

Four arms:
  none   RamFlow is not running
  task   RamFlow lowers single processes only      (use_autogroup = false)
  group  RamFlow may lower whole sessions          (the default)
  auto   as group, but no list: it finds the hogs itself (auto = true)

Arms are interleaved round by round so slow drift hits all of them equally.

usage: sudo python3 scripts/latency-bench.py [path/to/ramflow] [rounds]
"""
import json, os, shutil, signal, statistics, subprocess, sys, tempfile, time

BIN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/release/ramflow")
ROUNDS = int(sys.argv[2]) if len(sys.argv) > 2 else 3
CORES = os.cpu_count() or 2
NBG = CORES * 2
PY = os.path.realpath(shutil.which("python3"))
TICK = os.sysconf("SC_CLK_TCK")
LAYOUTS = tuple(os.environ.get("RF_LAYOUTS", "same,separate,scattered").split(","))
ARMS = ("none", "task", "group", "auto")

VICTIM = r"""
import sys, time, json, os
stop = sys.argv[1]
def work(n=4_000_000):
    s = 0
    for i in range(n):
        s += i * i
def burst():
    s = 0
    for i in range(40000):
        s += i * i
# 1. warm-up: a foreground that is busy, like a build or a render the user waits for
t0 = time.time()
while not os.path.exists(stop) and time.time() - t0 < 60:
    work()
# 2. short interactive-like bursts
short = []
for _ in range(300):
    t = time.perf_counter(); burst(); short.append((time.perf_counter() - t) * 1000); time.sleep(0.01)
# 3. one long job
long_ = []
for _ in range(10):
    t = time.perf_counter(); work(); long_.append((time.perf_counter() - t) * 1000)
print(json.dumps({"short": short, "long": long_}))
"""

LAUNCHER = r"""
import subprocess, sys, time
procs = [subprocess.Popen([p, "-c", "while True: pass"]) for p in sys.argv[1:]]
time.sleep(10**6)
"""


def nice_of(pid):
    try:
        return int(open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()[16])
    except OSError:
        return None


def group_nice(pid):
    try:
        return int(open(f"/proc/{pid}/autogroup").read().split()[2])
    except (OSError, IndexError, ValueError):
        return 0


def cpu_ticks(pid):
    try:
        f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        return int(f[11]) + int(f[12])
    except OSError:
        return 0


def children_of(pid):
    out = []
    for d in os.listdir("/proc"):
        if d.isdigit():
            try:
                if int(open(f"/proc/{d}/stat").read().rsplit(")", 1)[1].split()[1]) == pid:
                    out.append(int(d))
            except OSError:
                pass
    return out


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * p / 100))]


def link(tmp, name, target=PY):
    assert len(name) <= 15, name
    path = os.path.join(tmp, name)
    if not os.path.lexists(path):
        os.symlink(target, path)
    return path


def run_case(tmp, layout, arm, tag, with_bg=True):
    names = [f"rfb{os.getpid() % 10000}{tag}{i}" for i in range(NBG)]
    paths = [link(tmp, n) for n in names]
    bg_root = None
    solo = []
    if with_bg:
        if layout == "scattered":
            solo = [subprocess.Popen([p, "-c", "while True: pass"], start_new_session=True) for p in paths]
        elif layout == "separate":
            # one launcher in its own session, like a job started from another terminal tab
            launcher = link(tmp, f"rfl{os.getpid() % 10000}{tag}")
            bg_root = subprocess.Popen([launcher, "-c", LAUNCHER] + paths, start_new_session=True)
        else:
            launcher = link(tmp, f"rfl{os.getpid() % 10000}{tag}")
            bg_root = subprocess.Popen([launcher, "-c", LAUNCHER] + paths)
        time.sleep(0.8)
    bg = [p.pid for p in solo] if solo else (children_of(bg_root.pid) if bg_root else [])

    stop = os.path.join(tmp, f"stop-{tag}")
    victim_path = link(tmp, f"rfv{os.getpid() % 10000}{tag}")
    victim = subprocess.Popen([victim_path, "-c", VICTIM, stop], stdout=subprocess.PIPE)

    daemon, log = None, None
    if arm != "none":
        conf = os.path.join(tmp, f"{tag}.conf")
        with open(conf, "w") as f:
            f.write("dry_run = false\ninterval_ms = 500\nconfirm = 2\n")
            f.write(f"foreground_cmd = echo {victim.pid}\n")
            f.write("use_autogroup = %s\n" % ("false" if arm == "task" else "true"))
            if arm == "auto":
                f.write("auto = true\nauto_min_uid = 0\n")
            else:
                for n in names:
                    f.write(f"yield = comm:{n}\n")
        log = open(os.path.join(tmp, f"{tag}.log"), "w")
        daemon = subprocess.Popen([BIN, "run", "--config", conf, "--state-dir", os.path.join(tmp, "st-" + tag)],
                                  stdout=log, stderr=subprocess.DEVNULL)

    # Warm-up: the foreground is busy while the culprits run; RamFlow gets the chance to react.
    t0 = time.time()
    lowered_at = None
    if arm == "none":
        time.sleep(12)
    else:
        while time.time() - t0 < 40:
            if lowered_at is None and any((nice_of(p) or 0) > 0 or group_nice(p) > 0 for p in bg):
                lowered_at = time.time() - t0
            if lowered_at is not None and time.time() - t0 > lowered_at + 8:
                break
            time.sleep(0.2)
    open(stop, "w").close()

    c0 = sum(cpu_ticks(p) for p in bg)
    w0 = time.time()
    out, _ = victim.communicate(timeout=180)
    wall = time.time() - w0
    bg_cpu = (sum(cpu_ticks(p) for p in bg) - c0) / TICK / wall if bg else 0.0
    res = json.loads(out)

    events = {}
    restored = None
    final_levels = None
    if daemon:
        final_levels = [nice_of(p) for p in bg[:2]] + [group_nice(p) for p in bg[:1]]
        daemon.send_signal(signal.SIGTERM)
        daemon.wait(timeout=30)
        log.close()
        text = open(log.name).read()
        events = {
            "yielded": text.count("양보:"),
            "level_changes": text.count("단계 조절"),
            "no_effect": text.count("효과 없음"),
            "hogs": text.count("독차지 발견"),
        }
        restored = all(nice_of(p) in (0, None) for p in bg) and all(group_nice(p) == 0 for p in bg)
    for p in bg + ([bg_root.pid] if bg_root else []):
        try:
            os.kill(p, signal.SIGKILL)
        except OSError:
            pass
    for h in solo + ([bg_root] if bg_root else []):
        h.wait()
    return {
        "short_p50": pct(res["short"], 50), "short_p95": pct(res["short"], 95),
        "long_med": statistics.median(res["long"]),
        "bg_cores": bg_cpu, "lowered_after_s": lowered_at,
        "events": events, "restored": restored, "final": final_levels,
    }


def main():
    tmp = tempfile.mkdtemp(prefix="rf-bench-")
    results = {(l, a): [] for l in LAYOUTS for a in ARMS}
    all_restored = True
    try:
        base = run_case(tmp, "same", "none", "z0", with_bg=False)
        print(f"alone (no background): short p95 {base['short_p95']:.2f} ms, long job {base['long_med']:.1f} ms", flush=True)
        for r in range(ROUNDS):
            order = [(l, a) for l in LAYOUTS for a in ARMS]
            if r % 2:
                order.reverse()
            for k, (layout, arm) in enumerate(order):
                tag = f"{r}{k}"
                m = run_case(tmp, layout, arm, tag)
                results[(layout, arm)].append(m)
                if m["restored"] is False:
                    all_restored = False
                lowered = "-" if m["lowered_after_s"] is None else f"{m['lowered_after_s']:.1f}s"
                print(f"round {r + 1} {layout:8s} {arm:5s}: long job {m['long_med']:7.1f} ms | short p95 {m['short_p95']:6.2f} ms | "
                      f"background {m['bg_cores']:.2f}/{CORES} cores | lowered after {lowered} | {m['events']}", flush=True)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    def med(layout, arm, key):
        return statistics.median(m[key] for m in results[(layout, arm)])

    summary = {"cores": CORES, "culprits": NBG, "rounds": ROUNDS,
               "alone_ms": {"long_job": base["long_med"], "short_p95": base["short_p95"]},
               "all_restored_on_sigterm": all_restored, "cases": {}}
    for layout in LAYOUTS:
        none_long, none_short, none_bg = med(layout, "none", "long_med"), med(layout, "none", "short_p95"), med(layout, "none", "bg_cores")
        for arm in ARMS:
            long_, short_, bg_ = med(layout, arm, "long_med"), med(layout, arm, "short_p95"), med(layout, arm, "bg_cores")
            summary["cases"][f"{layout}/{arm}"] = {
                "long_job_ms": round(long_, 1), "short_p95_ms": round(short_, 2), "background_cores": round(bg_, 2),
                "long_job_change_pct": round((long_ / none_long - 1) * 100, 1),
                "short_p95_change_pct": round((short_ / none_short - 1) * 100, 1),
                "background_change_pct": round((bg_ / none_bg - 1) * 100, 1) if none_bg else None,
                "no_effect_withdrawals": sum(m["events"].get("no_effect", 0) for m in results[(layout, arm)]),
            }
    print(json.dumps(summary, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
