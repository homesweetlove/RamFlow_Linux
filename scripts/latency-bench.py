#!/usr/bin/env python3
"""Does RamFlow actually help the foreground?  (stdlib only, needs root for --apply)

A "foreground" probe repeatedly does a fixed slice of CPU work and records how long each slice
takes on the wall clock. Meanwhile 2x-cores busy loops ("background jobs") compete for the CPU.
Rounds alternate: without RamFlow / with RamFlow (--apply) so slow drift hits both equally.

usage: sudo python3 scripts/latency-bench.py [path/to/ramflow] [rounds]
"""
import json, os, shutil, signal, statistics, subprocess, sys, tempfile, time

BIN = sys.argv[1] if len(sys.argv) > 1 else "target/release/ramflow"
ROUNDS = int(sys.argv[2]) if len(sys.argv) > 2 else 3
CORES = os.cpu_count() or 2
NBG = CORES * 2
PY = os.path.realpath(shutil.which("python3"))
TICK = os.sysconf("SC_CLK_TCK")


def nice_of(pid):
    try:
        return int(open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()[16])
    except OSError:
        return None


def cpu_ticks(pid):
    f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    return int(f[11]) + int(f[12])


def probe(n=300):
    out = []
    for _ in range(n):
        t = time.perf_counter()
        s = 0
        for i in range(40000):
            s += i * i
        out.append((time.perf_counter() - t) * 1000)
        time.sleep(0.01)
    return out


def probe_sustained(n=8):
    """A long foreground job (think: a build or a render) -- how long does one slice take?"""
    out = []
    for _ in range(n):
        t = time.perf_counter()
        s = 0
        for i in range(4_000_000):
            s += i * i
        out.append((time.perf_counter() - t) * 1000)
    return out


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * p / 100))]


def round_(with_ramflow, tmp, tag):
    names, procs = [], []
    for i in range(NBG):
        n = f"rfb{os.getpid() % 10000}{tag}{i}"
        assert len(n) <= 15
        os.symlink(PY, os.path.join(tmp, n))
        names.append(n)
        procs.append(subprocess.Popen([os.path.join(tmp, n), "-c", "while True: pass"]))
    daemon = None
    state = os.path.join(tmp, "state-" + tag)
    if with_ramflow:
        conf = os.path.join(tmp, f"{tag}.conf")
        with open(conf, "w") as f:
            f.write("dry_run = false\ninterval_ms = 200\nconfirm = 2\ncpu_enter = 55\ncpu_leave = 30\n")
            for n in names:
                f.write(f"yield = comm:{n}\n")
        daemon = subprocess.Popen([BIN, "run", "--config", conf, "--state-dir", state],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        t0 = time.time()
        while time.time() - t0 < 30 and not all(nice_of(p.pid) == 15 for p in procs):
            time.sleep(0.1)
        assert all(nice_of(p.pid) == 15 for p in procs), "background jobs were never lowered"
    else:
        time.sleep(1.0)
    c0 = sum(cpu_ticks(p.pid) for p in procs)
    w0 = time.time()
    xs = probe()
    sus = probe_sustained()
    wall = time.time() - w0
    bg_cpu = (sum(cpu_ticks(p.pid) for p in procs) - c0) / TICK / wall  # cores' worth
    restored = None
    if daemon:
        daemon.send_signal(signal.SIGTERM)
        daemon.wait(timeout=20)
        restored = all(nice_of(p.pid) == 0 for p in procs)
    for p in procs:
        p.kill()
        p.wait()
    return xs, sus, bg_cpu, restored


def main():
    tmp = tempfile.mkdtemp(prefix="rf-bench-")
    res = {"without": [], "with": []}
    sus_res = {"without": [], "with": []}
    bg = {"without": [], "with": []}
    all_restored = True
    try:
        for r in range(ROUNDS):
            for label, flag in (("without", False), ("with", True)):
                xs, sus, bgc, restored = round_(flag, tmp, f"{'y' if flag else 'n'}{r}")
                res[label].append(xs)
                sus_res[label].append(sus)
                bg[label].append(bgc)
                if restored is False:
                    all_restored = False
                print(f"round {r + 1} {label:7s}: short bursts p50 {pct(xs, 50):6.2f} ms p95 {pct(xs, 95):6.2f} ms | "
                      f"long job median {statistics.median(sus):7.1f} ms | background used {bgc:.2f} of {CORES} cores",
                      flush=True)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    def med(label, p):
        return statistics.median(pct(x, p) for x in res[label])

    def sus_med(label):
        return statistics.median(statistics.median(x) for x in sus_res[label])

    summary = {
        "cores": CORES, "background_jobs": NBG, "rounds": ROUNDS,
        "short_burst_ms_without": {"p50": med("without", 50), "p95": med("without", 95), "p99": med("without", 99)},
        "short_burst_ms_with": {"p50": med("with", 50), "p95": med("with", 95), "p99": med("with", 99)},
        "long_job_ms_without": sus_med("without"),
        "long_job_ms_with": sus_med("with"),
        "background_cores_used_without": statistics.median(bg["without"]),
        "background_cores_used_with": statistics.median(bg["with"]),
        "all_restored_on_sigterm": all_restored,
    }
    w, v = summary["short_burst_ms_without"], summary["short_burst_ms_with"]
    summary["short_burst_p95_reduction_pct"] = round((1 - v["p95"] / w["p95"]) * 100, 1)
    summary["long_job_reduction_pct"] = round((1 - summary["long_job_ms_with"] / summary["long_job_ms_without"]) * 100, 1)
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
