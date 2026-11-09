#!/usr/bin/env python3
"""ffsandbox.py <arch> <tag> [--mode on|off|both] [--secs 60] [-- extra ffsession.py args]

Firefox sandbox acceptance check, run through ffsession.py. For each mode it boots, starts
/bin/firefox (with LEANDROS_FIREFOX_SANDBOX=1 for "on") on a local page holding a looping
<video> (so the RDD process starts), then from the guest collects for every firefox process
its type (-contentproc ... <type>, or parent) and /proc/<pid>/status Seccomp:, Seccomp_filters:,
NoNewPrivs:, NSpid: plus the /proc/<pid>/root target. "Sandbox:" lines come from Firefox's
stderr (/tmp/ff.log, MOZ_SANDBOX_LOGGING=1).

Prints a table per mode. Exit 0 only for mode "on" when every child type (the forkserver is
not sandboxed itself and is exempt) shows Seccomp: 2, at least one child exists, and ff.log has
no "Sandbox:" error lines. Mode "off" is informational. Output: $FFSESSION_OUT/run-<tag>-<mode>/.
Hold the qemu lock: lock.sh qemu python3 ffsandbox.py aarch64 sb1
"""
import os, re, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
A = sys.argv
ARCH, TAG = A[1], A[2]
MODE = A[A.index("--mode") + 1] if "--mode" in A else "both"
SECS = int(A[A.index("--secs") + 1]) if "--secs" in A else 60
EXTRA = A[A.index("--") + 1:] if "--" in A else []
OUTD = os.environ.get("FFSESSION_OUT", "/tmp/ffsession")
W = os.path.join(OUTD, "ffsandbox")
os.makedirs(W, exist_ok=True)

clip = os.path.join(W, "clip.webm")
if not os.path.exists(clip):
    for codec in (["-c:v", "libsvtav1", "-preset", "10", "-crf", "63"], ["-c:v", "libvpx-vp9", "-b:v", "200k"]):
        r = subprocess.run(["ffmpeg", "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25:duration=3",
                            *codec, "-g", "25", "-pix_fmt", "yuv420p", clip], capture_output=True)
        if r.returncode == 0:
            break
    else:
        clip = None
page = os.path.join(W, "sb.html")
open(page, "w").write("""<html><title>sb</title><body><h1>sandbox check</h1>
<video id=v src="file:///tmp/sb.webm" autoplay muted loop width=320></video>
<script>var v=document.getElementById('v');v.play();
setInterval(function(){dump('SBV '+v.currentTime.toFixed(2)+'\\n');},2000);</script></body></html>""")
collect = os.path.join(W, "sbcollect.sh")
open(collect, "w").write(r'''for d in /proc/[0-9]*; do
  p=${d#/proc/}
  c=$(cat $d/cmdline 2>/dev/null | tr '\0' ' ')
  case "$c" in *firefox*) ;; *) continue ;; esac
  echo "SBPID $p CMD ${c:0:400}"
  while read k v; do
    case "$k" in Seccomp:|Seccomp_filters:|NoNewPrivs:|NSpid:) echo "SBST $p $k $v" ;; esac
  done < $d/status
  echo "SBROOT $p $(readlink $d/root 2>&1)"
done
echo SBDONE
''')


def ptype(cmd):
    t = cmd.split()
    if "-contentproc" in t:
        # Forkserver-spawned children end with a process type; the type may be followed by nothing.
        known = ("tab", "rdd", "socket", "utility", "gmplugin", "gpu", "forkserver", "vr", "ipdlunittest")
        for w in reversed(t):
            if w in known:
                return w
        return "child?"
    if cmd.startswith("sh ") or "/bin/firefox" in t[:2] and "/usr/lib" not in cmd:
        return "wrapper"
    return "parent"


def run(mode):
    tag = f"{TAG}-{mode}"
    env = dict(os.environ, FFSESSION_OUT=OUTD, LEANDROS_RUN_ID=f"{os.environ.get('LEANDROS_RUN_ID', 'ffsb')}-{mode}")
    puts = [f"{page}:/tmp/sb.html", f"{collect}:/tmp/sbcollect.sh"] + ([f"{clip}:/tmp/sb.webm"] if clip else [])
    cmd = [sys.executable, os.path.join(HERE, "ffsession.py"), ARCH, tag, "--put", ",".join(puts),
           "--url", "file:///tmp/sb.html", "--prefs", "browser.dom.window.dump.enabled=true;media.autoplay.default=0",
           "--wait", str(SECS), "--shot-every", str(SECS), "--post", "sh /tmp/sbcollect.sh", "--post-timeout", "120"]
    if mode == "on":
        cmd += ["--env", "LEANDROS_FIREFOX_SANDBOX=1"]
    rc = subprocess.run(cmd + EXTRA, env=env).returncode
    d = os.path.join(OUTD, f"run-{tag}")
    rd = lambda n: open(os.path.join(d, n), errors="replace").read() if os.path.exists(os.path.join(d, n)) else ""
    post, ff = rd("post.txt"), rd("ff.log")
    procs = {}
    for l in post.splitlines():
        m = re.match(r"SBPID (\d+) CMD (.*)", l)
        if m:
            procs[m[1]] = {"cmd": m[2], "st": {}, "root": "?"}
            continue
        m = re.match(r"SBST (\d+) (\S+) ?(.*)", l)
        if m and m[1] in procs:
            procs[m[1]]["st"][m[2]] = m[3].strip()
        m = re.match(r"SBROOT (\d+) ?(.*)", l)
        if m and m[1] in procs:
            procs[m[1]]["root"] = m[2].strip() or "-"
    # MOZ_SANDBOX_LOGGING also prints policy-building chatter; only the rest counts as an error.
    noise = re.compile(r"policy for |fixing recursive|removing redundant|Couldn't open video device|adding (read|write)"
                       r"|using seccomp tsync|Parent path is|Adding paths from|[Ff]ailed errno -2\b")
    real = re.compile(r"errno -13\b|EACCES|denied|seccomp sandbox violation", re.I)

    def counted(l):
        if real.search(l):
            return True  # broker denials and SIGSYS violations always count
        if "Sandbox:" in l and not noise.search(l):
            return True
        return "andbox" in l and not noise.search(l) and re.search(r"unsupported|not supported|disabled|failed", l, re.I)
    sbl = [l.strip() for l in ff.splitlines() if ("andbox" in l or "seccomp" in l.lower()) and counted(l)]
    exitl = [l for l in ff.splitlines() if l.startswith("EXIT=")]
    print(f"== mode={mode} ffsession rc={rc} firefox procs={len(procs)} {exitl[-1] if exitl else 'still running'} "
          f"video={'SBV' in ff}")
    print(f"{'pid':>5} {'type':9} {'Seccomp':7} {'filt':4} {'NNP':3} {'NSpid':9} root")
    types = {}
    for p, v in sorted(procs.items(), key=lambda kv: int(kv[0])):
        t = ptype(v["cmd"])
        s = v["st"]
        print(f"{p:>5} {t:9} {s.get('Seccomp:', '?'):7} {s.get('Seccomp_filters:', '?'):4} {s.get('NoNewPrivs:', '?'):3} "
              f"{s.get('NSpid:', '?')[:9]:9} {v['root'][:30]}")
        types.setdefault(t, []).append(s.get("Seccomp:", "?"))
    for l in sbl[:10]:
        print("  LOG", l[:150])
    print(f"  Sandbox log lines: {len(sbl)}")
    kids = {t: v for t, v in types.items() if t not in ("parent", "wrapper", "forkserver")}
    ok = bool(kids) and all(x == "2" for v in kids.values() for x in v) and not sbl
    return ok


modes = ["on", "off"] if MODE == "both" else [MODE]
res = {m: run(m) for m in modes}
print("RESULT", res)
sys.exit(0 if res.get("on", False) else 1)
