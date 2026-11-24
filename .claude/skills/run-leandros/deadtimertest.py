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
when the guest reports "tick is dead". The strict part needs x86_64 KVM (TCG cannot reproduce the
dead tick; the guest says so). A missing knob or CPU is a FAIL. Every boot is time-bounded: a hang
(e.g. the BSP's tick dying on CPU 0) is a FAIL. Exit 0 iff every boot passes.
"""
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "x86_64")
tag = sys.argv[2] if len(sys.argv) > 2 else f"deadtimer-{arch}"
boots = sys.argv[3:] or ["1:7", "1:1", "1:2", "1:0"]
BOOT_TIMEOUT = int(os.environ.get("DEADTIMER_BOOT_TIMEOUT", "420"))
bad = 0
for b in boots:
    cpu, m = b.split(":")
    red = (int(m) & 3) == 0
    env = dict(os.environ, LEANDROS_RUN_ID=f"{os.environ.get('LEANDROS_RUN_ID', 'x')}-deadtimer{cpu}m{m}")
    ok, why = False, ""
    try:
        r = subprocess.run([sys.executable, os.path.join(HERE, "runtests.py"), arch, f"{tag}-{cpu}-{m}", "--suite", "deadtimer",
                            "--cmdline", f"timer.dead_delay={cpu} timer.fixmask={m}"],
                           env=env, timeout=BOOT_TIMEOUT)
        if red:
            # Red case: the guest must have failed because the tick is dead, nothing else.
            log = ""
            import glob
            for f in glob.glob(f"/tmp/ffsession/tests-{tag}-{cpu}-{m}*/serial-full.log"):
                log += open(f, errors="replace").read()
            ok = r.returncode != 0 and "FAIL: tick is dead" in log
            why = "red as expected" if ok else "expected red (tick is dead) but got rc=%d" % r.returncode
        else:
            ok = r.returncode == 0
            why = "green" if ok else "rc=%d" % r.returncode
    except subprocess.TimeoutExpired:
        why = f"HANG: no result within {BOOT_TIMEOUT}s"
    print(f"deadtimertest.py {arch} cpu{cpu} fixmask={m}: {'PASS' if ok else 'FAIL'} ({why})", flush=True)
    bad += not ok
print(f"deadtimertest.py {arch}: {'PASS' if not bad else 'FAIL'}")
sys.exit(1 if bad else 0)
