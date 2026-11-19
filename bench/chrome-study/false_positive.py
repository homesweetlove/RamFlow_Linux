#!/usr/bin/env python3
"""Does RamFlow leave things alone when it should? (needs root)

A: Chrome is open but idle while 8 CPU-bound processes saturate the machine for 30 s.
   An idle browser still wakes up now and then; if such a wake-up really waits for the CPU,
   a yield can start.  That is harmless (nothing else competes for the CPU the yielded
   programs give up), but it must happen at most once, must never touch the browser, and
   everything must be restored at exit.  With `auto` and the default settings it should not
   start at all.
B: Chrome is busy and nothing else is running.  Nothing may be lowered -- in particular not
   Chrome itself, nor the benchmark harness.

usage: sudo python3 bench/chrome-study/false_positive.py [--out DIR]
"""
import json, os, signal, subprocess, sys, tempfile, time, shutil
out_dir = "docs/evidence/2026-11-18-false-positive"
if "--out" in sys.argv:
    out_dir = sys.argv[sys.argv.index("--out") + 1]
sys.argv = ["x", "--out", "/tmp/fp-unused"]
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import study as S


def chrome_untouched(pid):
    # the whole Chrome tree: main process and every child
    bad = []
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            stat = open(f"/proc/{d}/stat").read()
            ppid = int(stat.rsplit(")", 1)[1].split()[1])
            nice = S.nice_of(int(d))
        except (OSError, ValueError):
            continue
        if (int(d) == pid or ppid == pid) and nice is not None and nice > 0:
            bad.append((int(d), nice))
    return bad


def log_restored(r):
    return r.get("bg_pristine", True)


def run(scenario, arm, tmp):
    chrome, cpid, _ = S.start_chrome(tmp, f"{scenario}{arm}")
    bg = S.Background(tmp, f"{scenario}{arm}", "scattered") if scenario == "A" else None
    time.sleep(6)
    conf = os.path.join(tmp, f"{scenario}{arm}.conf")
    with open(conf, "w") as f:
        f.write(f"dry_run = false\ninterval_ms = 500\nconfirm = 2\nforeground_cmd = echo {cpid}\n")
        if arm == "auto":
            f.write("auto = true\nauto_min_uid = 0\n")
        else:  # "list": names a program that does not exist in B and is the real bg in A
            if bg:
                for n in bg.names:
                    f.write(f"yield = comm:{n}\n")
            else:
                f.write("yield = comm:nonexistent\n")
    log = open(os.path.join(tmp, f"{scenario}{arm}.log"), "w")
    d = subprocess.Popen([S.BIN, "run", "--config", conf, "--state-dir", os.path.join(tmp, f"st{scenario}{arm}")], stdout=log,
                         stderr=open(os.path.join(tmp, f'{scenario}{arm}.trace'), 'w'), env={**os.environ, 'RAMFLOW_TRACE': '1'})
    lowered_ever = False
    if scenario == "A":
        end = time.time() + 30  # idle browser
        while time.time() < end:
            lowered_ever |= bg.lowered()
            time.sleep(0.3)
    else:
        drv = subprocess.Popen(["node", os.path.join(S.HERE, "driver.mjs"), "--port", str(S.PORT), "--stabilize", "20", "--warmup", "2",
                                "--n", "10", "--seed", "9"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        while drv.poll() is None:
            lowered_ever |= bool(chrome_untouched(cpid))
            time.sleep(0.3)
    d.send_signal(signal.SIGTERM); d.wait(timeout=30); log.close()
    text = open(log.name).read()
    os.makedirs(out_dir, exist_ok=True)
    shutil.copy(os.path.join(tmp, f'{scenario}{arm}.trace'), os.path.join(out_dir, f'{scenario}-{arm}.trace'))
    shutil.copy(log.name, os.path.join(out_dir, f'{scenario}-{arm}.log'))
    trace = open(os.path.join(tmp, f"{scenario}{arm}.trace")).read().splitlines()
    lowered_s = sum(0.5 for l in trace if " level=" in l and not l.split(" level=")[1].startswith("0 "))
    engagements = sum(1 for l in trace if "decision=Engage" in l)
    res = {"lowered_seconds": lowered_s, "engagements": engagements, "scenario": scenario, "arm": arm, "lowered_ever": lowered_ever, "yielded_events": text.count("양보:"),
           "hogs_found": text.count("독차지 발견"), "chrome_nice_touched": chrome_untouched(cpid)}
    res["bg_pristine"] = bg.pristine() if bg else True
    if bg:
        bg.close()
    chrome.terminate(); chrome.wait(timeout=20); time.sleep(1)
    return res


tmp = tempfile.mkdtemp(prefix="rf-fp-")
results = []
try:
    for scenario in ("A", "B"):
        for arm in ("list", "auto"):
            r = run(scenario, arm, tmp)
            max_engagements = 1 if (scenario == "A" and arm == "list") else 0
            r["restored"] = log_restored(r)
            ok = r["engagements"] <= max_engagements and not r["chrome_nice_touched"] and r["restored"]
            r["passed"] = ok
            results.append(r)
            print(r, flush=True)
finally:
    shutil.rmtree(tmp, ignore_errors=True)
os.makedirs(out_dir, exist_ok=True)
json.dump(results, open(os.path.join(out_dir, "results.json"), "w"), indent=1, ensure_ascii=False)
print("ALL PASSED" if all(r["passed"] for r in results) else "FAILED")
