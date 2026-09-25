#!/usr/bin/env python3
"""greeterstorm — kill the greeter's compositor N times and attribute the
memory each death costs.

usage: greeterstorm.py <arch> <deaths> [--settle S] [--first S] [--keep] [--tag T]

Boots the default graphical login (greetd -> cosmic-comp -> cosmic-greeter),
logs in as root on the serial console and then, per death:

  1. waits `--settle` seconds (default 100) so the respawned chain is past its
     start-up growth — memory around a respawn is only comparable phase to
     phase, never phase to trough;
  2. samples /proc/meminfo, /proc/kmemstat (buddy pages per allocation site,
     slab classes, live heap objects by exact size) and the live process list;
  3. SIGKILLs the compositor.

Every sample goes to <out>/<tag>-samples.jsonl and a per-site delta table
(first settled sample vs last, divided by deaths) is printed at the end: a
site whose live pages grow by a constant amount per death is the leak.

Env: LEANDROS_RUN_ID (default greeterleak) scopes driver.py's sockets/logs;
GREETERSTORM_OUT (default ~/greeterstorm) is where results go.
"""
import os, sys, time, json, re

os.environ.setdefault("LEANDROS_RUN_ID", "greeterleak")
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, f"{REPO}/.claude/skills/run-leandros")
os.chdir(REPO)
import driver  # noqa: E402

args = sys.argv[1:]
ARCH = args[0]
DEATHS = int(args[1])
def opt(name, default, conv=int):
    return conv(args[args.index(name) + 1]) if name in args else default
SETTLE = opt("--settle", 100)
FIRST = opt("--first", 120)
TAG = opt("--tag", f"{ARCH}-{time.strftime('%H%M%S')}", str)
KEEP = "--keep" in args
OUT = os.environ.get("GREETERSTORM_OUT") or os.path.expanduser("~/greeterstorm")
os.makedirs(OUT, exist_ok=True)
JL = open(f"{OUT}/{TAG}-samples.jsonl", "a")

def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)

def sh(cmd, t=20):
    return driver._serial_send(cmd, timeout=t)

# One builtin-only pass over /proc/<pid>/stat (brush's `read` does not fork),
# so a process census costs one `seq` exec however many pids there are.
# /proc/<pid>/stat's comm is a placeholder here; the exe link is real, so it
# names the process (one `readlink` exec per live pid only).
PS = ('hi=$(cut -d" " -f5 /proc/loadavg); for p in $(seq 1 $hi); do '
      'read -r a b c d rest < /proc/$p/stat 2>/dev/null && echo "P $a ($(readlink /proc/$p/exe)) $c $d"; '
      'done; echo "PS""END"')

def procs():
    out = sh(PS, 60)
    res = []
    for line in out.splitlines():
        m = re.match(r"P (\d+) \((.*)\) (\S) (\d+)", line.strip())
        if m:
            res.append((int(m[1]), m[2], m[3], int(m[4])))
    return res

def kmemstat():
    out = sh("cat /proc/kmemstat", 30)
    d = {"site": {}, "slab": {}, "heap": {}}
    for line in out.splitlines():
        f = line.split()
        if len(f) == 2 and f[0] in ("total_pages", "free_pages", "site_sum"):
            d[f[0]] = int(f[1])
        elif len(f) == 4 and f[0] == "site":
            d["site"][f[1]] = [int(f[2]), int(f[3])]
        elif len(f) == 4 and f[0] == "slab":
            d["slab"][f[1]] = [int(f[2]), int(f[3])]
        elif len(f) == 3 and f[0] == "heap":
            d["heap"][f[1]] = int(f[2])
    return d

def meminfo():
    out = sh("cat /proc/meminfo", 10)
    m = re.search(r"MemFree:\s+(\d+)", out)
    return int(m[1]) if m else None

def sample(i, phase):
    ps = procs()
    km = kmemstat()
    rec = {"i": i, "phase": phase, "t": time.time(), "memfree_kib": meminfo(),
           "procs": ps, "km": km}
    JL.write(json.dumps(rec) + "\n"); JL.flush()
    user = sum(v[0] for k, v in km["site"].items() if "vmm.rs" in k or "cow.rs" in k)
    slabp = sum(v[0] for v in km["slab"].values())
    log(f"[{i}:{phase}] free={km.get('free_pages')} user_sites={user} slab_pages={slabp} "
        f"procs={len(ps)} exes={sorted(set(p[1].rsplit('/',1)[-1] for p in ps))}")
    return rec

def comp_pids(ps):
    return [p[0] for p in ps if p[1].endswith("/cosmic-comp")]

if "--attach" not in args:
    driver.cmd_start(ARCH)
    driver.cmd_login("root", "root")
log(f"booted; first settle {FIRST}s")
time.sleep(FIRST)
samples = []
for i in range(DEATHS + 1):
    rec = sample(i, "settled")
    samples.append(rec)
    if i == DEATHS:
        break
    pids = comp_pids(rec["procs"])
    if not pids:
        log("no cosmic-comp alive; waiting 30 s more")
        time.sleep(30)
        pids = comp_pids(procs())
    # brush's `kill` takes one pid; the lowest is the thread-group leader
    # (the others are its threads), and SIGKILL takes the whole group.
    log(f"kill -9 {pids[0]} (of {pids})")
    sh(f"kill -9 {pids[0]}", 10)
    time.sleep(SETTLE)

def delta(a, b, key):
    out = {}
    for k in set(a["km"][key]) | set(b["km"][key]):
        va = a["km"][key].get(k, [0, 0] if key != "heap" else 0)
        vb = b["km"][key].get(k, [0, 0] if key != "heap" else 0)
        va = va[0] if isinstance(va, list) else va
        vb = vb[0] if isinstance(vb, list) else vb
        if va != vb:
            out[k] = vb - va
    return out

a, b = samples[1] if len(samples) > 2 else samples[0], samples[-1]
n = b["i"] - a["i"]
log(f"=== deltas sample {a['i']} -> {b['i']} ({n} deaths) ===")
log(f"free_pages {a['km'].get('free_pages')} -> {b['km'].get('free_pages')} "
    f"({(b['km'].get('free_pages',0)-a['km'].get('free_pages',0))/max(n,1):+.0f}/death)")
for key in ("site", "slab"):
    for k, v in sorted(delta(a, b, key).items(), key=lambda kv: kv[1]):
        log(f"  {key} {k}: {v:+d} pages ({v/max(n,1):+.1f}/death)")
hd = delta(a, b, "heap")
for k, v in sorted(hd.items(), key=lambda kv: -abs(kv[1] * int(kv[0])))[:25]:
    log(f"  heap size<={k}: {v:+d} objs ({v*int(k)/1024/max(n,1):+.1f} KiB/death)")
if not KEEP:
    driver.cmd_stop()
