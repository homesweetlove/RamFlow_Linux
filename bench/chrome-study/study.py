#!/usr/bin/env python3
"""Real-Chrome foreground study, modelled on the Windows RamFlow protocol. (stdlib only, needs root)

A real Chrome (DevTools protocol, trusted input events) runs two features of a generated
dashboard -- group search (rebuilds 1,000 DOM rows) and sort -- while 8 CPU-bound background
processes (2 threads each, SHA-256 over 64 KiB chunks, each in its own session) compete for
the CPU. Per paired block the same Chrome instance and the same background processes are used
for every arm; arm order is shuffled with a fixed seed.

Arms:  off   RamFlow not running
       on    RamFlow with the background jobs listed (yield = comm:...)
       auto  RamFlow with auto = true and no list at all (default thresholds: 35 % of a core for 5 s)
       auto15 like auto, but a program counts as a hog from 15 % of a core

Per condition: 12 s stabilization (the foreground is busy), 5 unreported warm-ups, 1 s wait,
then 30 searches + 30 sorts, 150 ms apart. p95 per block; median over blocks is reported.

usage: sudo python3 bench/chrome-study/study.py [--blocks 5] [--layout scattered|same]
                                               [--seed 20261118] [--out DIR] [--bin PATH]
"""
import argparse, csv, json, os, platform, random, shutil, signal, statistics, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
ap = argparse.ArgumentParser()
ap.add_argument("--blocks", type=int, default=5)
ap.add_argument("--layout", choices=("scattered", "same"), default="scattered")
ap.add_argument("--seed", type=int, default=20261118)
ap.add_argument("--out", default=os.path.join(HERE, "out"))
ap.add_argument("--bin", default=os.path.join(HERE, "..", "..", "target", "release", "ramflow"))
ap.add_argument("--arms", default="off,on,auto,auto15")
ap.add_argument("--n", type=int, default=30)
ap.add_argument("--bg", type=int, default=8, help="number of background processes")
ap.add_argument("--threads", type=int, default=2, help="threads per background process")
args = ap.parse_args()
BIN = os.path.abspath(args.bin)
ARMS = args.arms.split(",")
NBG, THREADS = args.bg, args.threads
PY = os.path.realpath(shutil.which("python3"))
CHROME = shutil.which("google-chrome") or shutil.which("chromium")
PORT = 9411

BG_CODE = r"""
import hashlib, os, sys, threading, time
path = sys.argv[1]; counts = [0] * int(sys.argv[2])
def work(k):
    buf = os.urandom(65536)
    while True:
        hashlib.sha256(buf).digest(); counts[k] += 1
for k in range(len(counts)):
    threading.Thread(target=work, args=(k,), daemon=True).start()
while True:
    time.sleep(0.25)
    with open(path + ".tmp", "w") as f: f.write(str(sum(counts)))
    os.replace(path + ".tmp", path)
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


def p95(xs):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * 0.95))]


class Background:
    def __init__(self, tmp, tag, layout):
        self.dir = os.path.join(tmp, "bg-" + tag)
        os.makedirs(self.dir)
        self.names, self.procs = [], []
        for i in range(NBG):
            name = f"rfs{os.getpid() % 10000}{tag}{i}"
            assert len(name) <= 15, name
            link = os.path.join(self.dir, name)
            os.symlink(PY, link)
            self.names.append(name)
            self.procs.append(subprocess.Popen(
                [link, "-c", BG_CODE, os.path.join(self.dir, f"cnt{i}"), str(THREADS)],
                start_new_session=(layout == "scattered"), stderr=subprocess.DEVNULL))
        self.samples = []  # (epoch seconds, total count)
        self.stop = False
        self.thread = threading.Thread(target=self._sample, daemon=True)
        self.thread.start()

    def total(self):
        t = 0
        for i in range(NBG):
            try:
                t += int(open(os.path.join(self.dir, f"cnt{i}")).read() or 0)
            except (OSError, ValueError):
                pass
        return t

    def _sample(self):
        while not self.stop:
            self.samples.append((time.time(), self.total()))
            time.sleep(0.2)

    def rate(self, t0, t1):
        def at(t):
            before = [s for s in self.samples if s[0] <= t]
            after = [s for s in self.samples if s[0] >= t]
            if not before or not after:
                return None
            (ta, ca), (tb, cb) = before[-1], after[0]
            return ca if tb == ta else ca + (cb - ca) * (t - ta) / (tb - ta)
        a, b = at(t0), at(t1)
        return None if a is None or b is None else (b - a) / (t1 - t0)

    def lowered(self):
        return any((nice_of(p.pid) or 0) > 0 or group_nice(p.pid) > 0 for p in self.procs)

    def pristine(self):
        return all(nice_of(p.pid) in (0, None) and group_nice(p.pid) == 0 for p in self.procs)

    def close(self):
        self.stop = True
        for p in self.procs:
            try:
                p.kill()
            except OSError:
                pass
            p.wait()


def start_chrome(tmp, tag):
    prof = os.path.join(tmp, "profile-" + tag)
    proc = subprocess.Popen([CHROME, "--headless=new", "--no-sandbox", "--disable-gpu", f"--remote-debugging-port={PORT}",
                             f"--user-data-dir={prof}", "--no-first-run", "about:blank"],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    import urllib.request
    for _ in range(60):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{PORT}/json/version", timeout=1).read()
            break
        except OSError:
            time.sleep(0.5)
    else:
        raise RuntimeError("chrome did not start")
    main = None
    for d in os.listdir("/proc"):
        if d.isdigit():
            try:
                cmd = open(f"/proc/{d}/cmdline").read().split("\0")
            except OSError:
                continue
            if any(c == f"--user-data-dir={prof}" for c in cmd) and not any(c.startswith("--type=") for c in cmd):
                main = int(d)
                break
    assert main, "chrome main pid not found"
    version = json.load(urllib.request.urlopen(f"http://127.0.0.1:{PORT}/json/version"))["Browser"]
    return proc, main, version


def run_condition(tmp, arm, block, chrome_pid, bg, seed, cond_tag):
    daemon, log = None, None
    state = os.path.join(tmp, "state-" + cond_tag)
    if arm != "off":
        conf = os.path.join(tmp, cond_tag + ".conf")
        with open(conf, "w") as f:
            f.write("dry_run = false\ninterval_ms = 500\nconfirm = 2\n")
            f.write(f"foreground_cmd = echo {chrome_pid}\n")
            if arm in ("auto", "auto15"):
                f.write("auto = true\nauto_min_uid = 0\n")
                if arm == "auto15":
                    f.write("auto_hog_cpu = 15\n")
            else:
                for n in bg.names:
                    f.write(f"yield = comm:{n}\n")
        log = open(os.path.join(tmp, cond_tag + ".log"), "w")
        daemon = subprocess.Popen([BIN, "run", "--config", conf, "--state-dir", state], stdout=log, stderr=open(os.path.join(tmp, cond_tag + '.trace'), 'w'), env={**os.environ, 'RAMFLOW_TRACE': '1'})
    t_start = time.time()
    lowered_at = None
    watcher_stop = threading.Event()

    def watch():
        nonlocal lowered_at
        while not watcher_stop.is_set():
            if lowered_at is None and bg.lowered():
                lowered_at = time.time() - t_start
            time.sleep(0.2)
    w = threading.Thread(target=watch, daemon=True)
    w.start()
    out = subprocess.run(["node", os.path.join(HERE, "driver.mjs"), "--port", str(PORT), "--stabilize", "12", "--warmup", "5",
                          "--n", str(args.n), "--seed", str(seed)], capture_output=True, text=True, timeout=600)
    watcher_stop.set()
    if out.returncode != 0:
        raise RuntimeError("driver failed: " + out.stderr[-400:])
    res = json.loads(out.stdout)
    lowered_during = bg.lowered()
    events, restored, journal_gone = {}, None, None
    if daemon:
        daemon.send_signal(signal.SIGTERM)
        daemon.wait(timeout=30)
        log.close()
        text = open(log.name).read()
        os.makedirs(os.path.join(args.out, 'logs'), exist_ok=True)
        shutil.copy(log.name, os.path.join(args.out, 'logs', cond_tag + '.log'))
        shutil.copy(os.path.join(tmp, cond_tag + '.trace'), os.path.join(args.out, 'logs', cond_tag + '.trace'))
        events = {"yielded": text.count("양보:"), "level_changes": text.count("단계 조절"),
                  "no_effect": text.count("효과 없음"), "hogs": text.count("독차지 발견")}
        time.sleep(0.3)
        restored = bg.pristine()
        journal_gone = not os.path.exists(os.path.join(state, "journal"))
    time.sleep(0.6)  # let the sampler record a point after the window ends
    rate = bg.rate(res["window"]["startMs"] / 1000, res["window"]["endMs"] / 1000)
    return {
        "search": [x["ms"] for x in res["search"]], "sort": [x["ms"] for x in res["sort"]],
        "search_inner": [x["inner"] for x in res["search"]], "sort_inner": [x["inner"] for x in res["sort"]],
        "errors": res["errors"], "bg_rate": rate, "lowered_after_s": lowered_at, "lowered_during": lowered_during,
        "events": events, "restored": restored, "journal_gone": journal_gone,
    }


def main():
    os.makedirs(args.out, exist_ok=True)
    tmp = tempfile.mkdtemp(prefix="rf-chrome-")
    rng = random.Random(args.seed)
    trials, meta = [], {}
    raw = []
    try:
        for block in range(1, args.blocks + 1):
            order = list(ARMS)
            rng.shuffle(order)
            chrome, chrome_pid, version = start_chrome(tmp, f"b{block}")
            meta["chrome"] = version
            bg = Background(tmp, f"b{block}", args.layout)
            time.sleep(6)  # background settles, Chrome idle
            print(f"block {block}: order {order}, chrome pid {chrome_pid}", flush=True)
            for arm in order:
                r = run_condition(tmp, arm, block, chrome_pid, bg, args.seed + block, f"{block}{arm}")
                raw.append({"block": block, "arm": arm, **r})
                for metric in ("search", "sort"):
                    xs = r[metric]
                    trials.append({"block": block, "condition": arm, "metric": metric, "p50Ms": statistics.median(xs),
                                   "p95Ms": p95(xs), "maxMs": max(xs), "samples": len(xs),
                                   "eventToFrameP95Ms": p95(r[metric + "_inner"]), "backgroundPerSecond": r["bg_rate"],
                                   "errors": len(r["errors"]), "loweredAfterS": r["lowered_after_s"],
                                   "resourcesRestored": r["restored"], "journalGone": r["journal_gone"], **{f"ev_{k}": v for k, v in r["events"].items()}})
                s = r["search"]; t = r["sort"]
                print(f"  {arm:5s} search p95 {p95(s):6.1f} ms | sort p95 {p95(t):6.1f} ms | bg {r['bg_rate'] or 0:8.0f}/s | "
                      f"lowered after {r['lowered_after_s'] if r['lowered_after_s'] is None else round(r['lowered_after_s'], 1)} s | {r['events']} | "
                      f"errors {len(r['errors'])} restored {r['restored']}", flush=True)
                time.sleep(3)
            bg.close()
            chrome.terminate()
            chrome.wait(timeout=20)
            time.sleep(1)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    # ---- summary ----
    def block_p95(arm, metric):
        return [t["p95Ms"] for t in trials if t["condition"] == arm and t["metric"] == metric]

    summary = {"protocol": {"layout": args.layout, "blocks": args.blocks, "seed": args.seed, "samples_per_feature_per_condition": args.n,
                            "background": f"{NBG} processes x {THREADS} threads, SHA-256 over 64 KiB, "
                                          + ("each in its own session" if args.layout == "scattered" else "same session as the harness")},
               "environment": {"kernel": platform.release(), "cores": os.cpu_count(), "chrome": meta.get("chrome"),
                               "ramflow": subprocess.run([BIN, "version"], capture_output=True, text=True).stdout.strip()},
               "features": {}, "background": {}, "integrity": {}}
    for metric in ("search", "sort"):
        off = block_p95("off", metric)
        entry = {"off_p95_median_ms": statistics.median(off), "block_p95_off": off}
        for arm in ARMS:
            if arm == "off":
                continue
            v = block_p95(arm, metric)
            entry[arm] = {"p95_median_ms": statistics.median(v), "block_p95": v,
                          "reduction_pct": round((1 - statistics.median(v) / statistics.median(off)) * 100, 1),
                          "improved_blocks": f"{sum(1 for a, b in zip(v, off) if a < b)}/{len(v)}"}
        summary["features"][metric] = entry
    for arm in ARMS:
        rates = [r["bg_rate"] for r in raw if r["arm"] == arm and r["bg_rate"]]
        summary["background"][arm] = {"median_per_second": statistics.median(rates), "per_block": rates}
    base = summary["background"]["off"]["median_per_second"]
    for arm in ARMS:
        if arm != "off":
            summary["background"][arm]["change_pct"] = round((summary["background"][arm]["median_per_second"] / base - 1) * 100, 1)
    summary["integrity"] = {
        "conditions": len(raw), "feature_operations": sum(len(r["search"]) + len(r["sort"]) for r in raw),
        "result_check_failures": sum(len(r["errors"]) for r in raw),
        "restored_all": all(r["restored"] in (None, True) for r in raw),
        "journal_removed_all": all(r["journal_gone"] in (None, True) for r in raw),
        "no_effect_withdrawals": sum(r["events"].get("no_effect", 0) for r in raw if r["events"]),
    }
    with open(os.path.join(args.out, "trials.csv"), "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(trials[0].keys()))
        w.writeheader(); w.writerows(trials)
    json.dump(summary, open(os.path.join(args.out, "summary.json"), "w"), indent=1, ensure_ascii=False)
    json.dump(raw, open(os.path.join(args.out, "raw.json"), "w"))
    print(json.dumps(summary, indent=1, ensure_ascii=False))


if __name__ == "__main__":
    main()
