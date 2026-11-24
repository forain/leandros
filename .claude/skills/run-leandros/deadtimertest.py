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
import os
import subprocess
import sys
import time

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
    # The stall kills the tick only when it lands in the race window; with no fix about one
    # boot in six keeps its tick. A red boot therefore gets up to 3 attempts, the green ones one.
    for attempt in range(3 if red else 1):
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
                # On CPU 0 (the BSP, which keeps global time) a dead tick can also stop the
                # guest from ever reaching a shell: that is the same defect, so it is red too.
                ok = r.returncode != 0 and ("FAIL: tick is dead" in log or (cpu == "0" and "dead_local_tick: cpu" not in log))
                why = "red as expected" if ok else "expected red (tick is dead) but got rc=%d" % r.returncode
            else:
                ok = r.returncode == 0
                why = "green" if ok else "rc=%d" % r.returncode
        except subprocess.TimeoutExpired:
            ok = red and cpu == "0"  # a dead BSP tick hangs the whole guest: the expected red
            why = f"HANG: no result within {BOOT_TIMEOUT}s" + (" (BSP tick dead, red as expected)" if ok else "")
        # A hung guest (dead BSP tick) outlives runtests: reap this worktree's QEMU so it
        # does not hold the image lock against the next boot.
        root = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
        subprocess.run(["pkill", "-f", f"[q]emu-system.*{root}/"], check=False)
        for _ in range(60):  # the image write lock is held until QEMU is gone
            if subprocess.run(["pgrep", "-f", f"[q]emu-system.*{root}/"], capture_output=True).returncode != 0:
                break
            time.sleep(1)
        if ok or not red:
            break
    print(f"deadtimertest.py {arch} cpu{cpu} fixmask={m}: {'PASS' if ok else 'FAIL'} ({why})", flush=True)
    bad += not ok
print(f"deadtimertest.py {arch}: {'PASS' if not bad else 'FAIL'}")
sys.exit(1 if bad else 0)
