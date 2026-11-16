#!/usr/bin/env python3
"""pidnszaptest.py <arch> [tag]: the `[pidns] zap timeout` path (own boot, cmdline pidns.zaptest=1).

The hook (sched/src/pidns.rs, off by default) makes a dying pid-ns init's zap skip the SIGKILL and
use a 300 ms deadline, so a survivor forces the timeout: nstest --zaptmo checks init is reaped in
bounded time, the kernel log carries the line, and fork/wait still work. Exit 0 iff it passes.
"""
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
tag = sys.argv[2] if len(sys.argv) > 2 else f"pidnszap-{arch}"
# SELF_BOOTS: runtests.py below boots its own VM (own cmdline), so run under our own run id.
os.environ["LEANDROS_RUN_ID"] = f"{os.environ.get('LEANDROS_RUN_ID', 'x')}-pidnszap"
r = subprocess.run([sys.executable, os.path.join(HERE, "runtests.py"), arch, tag, "--suite", "pidnszap"])
print(f"pidnszaptest.py {arch}: {'PASS' if r.returncode == 0 else 'FAIL'}")
sys.exit(r.returncode)
