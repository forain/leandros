#!/usr/bin/env python3
"""blanktest.py <arch> [tag]: Wayland frame callbacks while the outputs are powered off.

Boots a GPU session (ffsession.py --nofirefox: greeter login as leandro, cosmic-term),
then runs /bin/blanktest from the serial root shell against the session's compositor
(userland/blanktest): it powers every output off with zwlr_output_power_v1, checks that an
animating window keeps getting frame callbacks, that a window mapped while the outputs are
off gets its first callback, and that the compositor leaves the outputs off. Exit 0 iff
blanktest prints PASS.

Without ports/cosmic-comp's 0002 patch it fails with 0 callbacks while off.
Takes the QEMU VM (use the qemu lock); run id `<LEANDROS_RUN_ID or blanktest>`.
"""
import os
import sys

import driver
import ffsdiag

HERE = os.path.dirname(os.path.abspath(__file__))
arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
tag = sys.argv[2] if len(sys.argv) > 2 else f"blanktest-{arch}"
out = os.environ.setdefault("FFSESSION_OUT", "/tmp/ffsession")
os.environ.setdefault("LEANDROS_RUN_ID", "blanktest")
post = ("XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1 /bin/blanktest; "
        "echo BLANKTEST_RC=$?")
# runtests gives this script max(300, HOST_TIMEOUTS.get(script, 0)) * driver.wait_scale(arch)
# (blanktest.py is not in HOST_TIMEOUTS); finish ~30 s under it so ffsession is killed
# here (log kept, QEMU stopped) instead of by runtests' timeout.
budget = float(os.environ.get("BLANKTEST_BUDGET", 280 * driver.wait_scale(arch) - 30))
rc, flines, secs = ffsdiag.run(arch, tag, ["--nofirefox", "--post", post], budget)
try:
    text = open(f"{out}/run-{tag}/post.txt").read()
except OSError:
    text = ""
lines = [l for l in text.splitlines() if l.startswith("blanktest:") or "BLANKTEST_RC" in l]
print("\n".join(lines))
ok = "blanktest: PASS" in text
ffsdiag.diag(flines, f"{out}/run-{tag}", ok)
print(f"blanktest.py {arch}: {'PASS' if ok else 'FAIL'} (ffsession rc {rc}, {out}/run-{tag})")
sys.exit(0 if ok else 1)
