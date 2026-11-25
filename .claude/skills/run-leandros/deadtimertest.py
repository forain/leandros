#!/usr/bin/env python3
"""deadtimertest.py <arch> [tag] [boot...]: x86_64 dead local LAPIC tick, one boot per fix combination.

SELF_BOOTS: each boot runs `runtests.py --suite deadtimer` under its own kernel command line
(timer.dead_delay=<cpu> timer.fixmask=<m>; fixmask bit0 = fix 1 count reload after the mode flip,
bit1 = fix 2 check_alive recovers a periodic CPU, bit2 = fix 3 stale one-shot deadline ignored).
Default boots (~1 min each):
  1:7  all fixes          green: tick alive, dead_recovered == 0 (fix 1 prevents the death)
  1:1  fix 1 only         green: same
  1:2  fix 2 only         green: the tick dies, check_alive recovers it exactly once (periodic state)
  1:0  no fix             must be RED ("tick is dead"): the reproducer proves the stall kills the tick
Boot args are "<cpu>:<fixmask>"; a boot with fixmask&3 == 0 expects the red result and PASSES only
when the guest reports "tick is dead" (on CPU 0 also: the guest never boots to a shell). The strict part needs x86_64 KVM (TCG cannot reproduce the
dead tick; the guest says so). A missing knob or CPU is a FAIL. Every boot is time-bounded: a hang
(e.g. the BSP's tick dying on CPU 0) is a FAIL. Exit 0 iff every boot passes.
"""
import glob
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "x86_64")
tag = sys.argv[2] if len(sys.argv) > 2 else f"deadtimer-{arch}"
boots = sys.argv[3:] or ["1:7", "1:1", "1:2", "1:0"]
BOOT_TIMEOUT = int(os.environ.get("DEADTIMER_BOOT_TIMEOUT", "420"))
bad = 0
BASE_OUT = os.environ.get("FFSESSION_OUT", "/tmp/ffsession")
BASE_RID = os.environ.get("LEANDROS_RUN_ID", "x")


def reap(rid):
    """Stop only the VM(s) started under this exact LEANDROS_RUN_ID (and its -sN shard ids):
    driver.py stop on the id's own pid file. Never matches by worktree path or process name."""
    ids = [rid] + [os.path.basename(f)[len("leandros-"):-len("-qemu.pid")]
                   for f in glob.glob(f"/tmp/leandros-{glob.escape(rid)}-s[0-9]*-qemu.pid")]
    for i in ids:
        subprocess.run([sys.executable, os.path.join(HERE, "driver.py"), "stop"],
                       env=dict(os.environ, LEANDROS_RUN_ID=i), stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, check=False)


for b in boots:
    cpu, m = b.split(":")
    red = (int(m) & 3) == 0
    ok, why = False, ""
    # The stall kills the tick only when it lands in the race window; with no fix about one
    # boot in six keeps its tick. A red boot therefore gets up to 3 attempts, the green ones one.
    for attempt in range(3 if red else 1):
        # Unique run id and out dir per attempt: a leftover wedged VM or log never aliases the next.
        rid = f"{BASE_RID}-deadtimer{cpu}m{m}a{attempt}-{os.getpid()}"
        out_base = os.path.join(BASE_OUT, f"deadtimer-{os.getpid()}-{cpu}-{m}-a{attempt}")
        os.makedirs(out_base, exist_ok=True)
        env = dict(os.environ, LEANDROS_RUN_ID=rid, FFSESSION_OUT=out_base)
        rtag = f"{tag}-{cpu}-{m}"
        captured = ""
        try:
            p = subprocess.run([sys.executable, os.path.join(HERE, "runtests.py"), arch, rtag, "--suite", "deadtimer",
                                "--cmdline", f"timer.dead_delay={cpu} timer.fixmask={m}"],
                               env=env, timeout=BOOT_TIMEOUT, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, text=True, errors="replace")
            captured = p.stdout or ""
            sys.stdout.write(captured)
            sys.stdout.flush()
            rc = p.returncode
            if red:
                # Red case: the guest must have failed because the tick is dead, nothing else.
                log = captured
                for f in glob.glob(os.path.join(out_base, f"tests-{rtag}*", "**", "serial-full.log"), recursive=True):
                    log += open(f, errors="replace").read()
                # On CPU 0 (the BSP, which keeps global time) a dead tick can also stop the
                # guest from ever reaching a shell: that is the same defect, so it is red too.
                ok = rc != 0 and ("FAIL: tick is dead" in log or (cpu == "0" and "dead_local_tick: cpu" not in log))
                why = "red as expected" if ok else "expected red (tick is dead) but got rc=%d" % rc
            else:
                ok = rc == 0
                why = "green" if ok else "rc=%d" % rc
        except subprocess.TimeoutExpired:
            ok = red and cpu == "0"  # a dead BSP tick hangs the whole guest: the expected red
            why = f"HANG: no result within {BOOT_TIMEOUT}s" + (" (BSP tick dead, red as expected)" if ok else "")
        # A hung guest (dead BSP tick) outlives runtests: reap exactly this attempt's VM by its
        # own run id so the image lock is released, and nothing else.
        reap(rid)
        if ok or not red:
            break
    print(f"deadtimertest.py {arch} cpu{cpu} fixmask={m}: {'PASS' if ok else 'FAIL'} ({why})", flush=True)
    bad += not ok
print(f"deadtimertest.py {arch}: {'PASS' if not bad else 'FAIL'}")
sys.exit(1 if bad else 0)
