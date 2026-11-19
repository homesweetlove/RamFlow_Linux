#!/usr/bin/env python3
"""How heavy is RamFlow itself?  CPU use and memory of the daemon over 60 s while the machine
is busy and N idle processes exist.  (stdlib only, needs root)

usage: sudo python3 bench/overhead.py [--out FILE]
"""
import json, os, signal, subprocess, sys, tempfile, time

BIN = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "target", "release", "ramflow"))
out = sys.argv[sys.argv.index("--out") + 1] if "--out" in sys.argv else None
CLK = os.sysconf("SC_CLK_TCK")
HASH = "import hashlib,os\nb=os.urandom(65536)\nwhile True: hashlib.sha256(b).digest()"
results = []
for n_idle in (0, 500, 2000):
    tmp = tempfile.mkdtemp(prefix="rf-ovh-")
    idle = [subprocess.Popen(["sleep", "600"], start_new_session=True) for _ in range(n_idle)]
    burners = [subprocess.Popen([sys.executable, "-c", HASH], start_new_session=True) for _ in range(4)]
    fg = subprocess.Popen(["sleep", "600"])
    conf = os.path.join(tmp, "c.conf")
    open(conf, "w").write(f"dry_run = false\nauto = true\nauto_min_uid = 0\nforeground_cmd = echo {fg.pid}\n")
    d = subprocess.Popen([BIN, "run", "--config", conf, "--state-dir", os.path.join(tmp, "st")], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(2)

    def cpu():
        f = open(f"/proc/{d.pid}/stat").read().rsplit(")", 1)[1].split()
        return (int(f[11]) + int(f[12])) / CLK
    t0, c0 = time.time(), cpu()
    time.sleep(60)
    t1, c1 = time.time(), cpu()
    status = open(f"/proc/{d.pid}/status").read()
    rss = next(int(l.split()[1]) for l in status.splitlines() if l.startswith("VmRSS"))
    hwm = next(int(l.split()[1]) for l in status.splitlines() if l.startswith("VmHWM"))
    threads = next(int(l.split()[1]) for l in status.splitlines() if l.startswith("Threads"))
    d.send_signal(signal.SIGTERM); d.wait(timeout=30)
    for p in idle + burners + [fg]:
        p.kill(); p.wait()
    r = {"idle_processes": n_idle, "daemon_cpu_percent_of_one_core": round((c1 - c0) / (t1 - t0) * 100, 2),
         "rss_kib": rss, "peak_rss_kib": hwm, "threads": threads, "seconds": round(t1 - t0)}
    results.append(r)
    print(r, flush=True)
if out:
    os.makedirs(os.path.dirname(out), exist_ok=True)
    json.dump({"binary_bytes": os.path.getsize(BIN), "results": results}, open(out, "w"), indent=1)
