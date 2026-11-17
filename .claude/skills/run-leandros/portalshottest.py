#!/usr/bin/env python3
"""portalshottest.py <arch> [tag]: org.freedesktop.portal.Screenshot in a live COSMIC session.

Boots a GPU session (ffsession.py --nofirefox: greeter login as leandro, cosmic-term),
then, as leandro on the session bus, runs `leandros-sysbus portal screenshot 90`
(ports/sysbus/src/portal_probe.rs): it records the PermissionStore "screenshot" grant
(without it xdg-desktop-portal asks Access.AccessDialog, which nobody answers headless:
response 2 after 25 s), calls Screenshot non-interactively and checks response 0 and a
PNG at the returned file uri. This script then re-checks from the guest shell that the
file exists and is non-empty. Exit 0 iff both hold.

PORTALSHOT_TIMEOUT=<s> overrides the probe's wait (1 = red: no response in time).
Takes the QEMU VM (use the qemu lock); run id `<LEANDROS_RUN_ID or portalshot>`.
"""
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "aarch64")
tag = sys.argv[2] if len(sys.argv) > 2 else f"portalshot-{arch}"
out = os.environ.setdefault("FFSESSION_OUT", "/tmp/ffsession")
os.environ.setdefault("LEANDROS_RUN_ID", "portalshot")
t = os.environ.get("PORTALSHOT_TIMEOUT", "90")
bus = "unix:path=/run/user/1000/bus"
post = (f"su leandro -c 'DBUS_SESSION_BUS_ADDRESS={bus} DBUS_SYSTEM_BUS_ADDRESS={bus} "
        f"XDG_RUNTIME_DIR=/run/user/1000 /usr/libexec/leandros-sysbus portal screenshot {t}' "
        "> /tmp/portalshot.out 2>&1; echo PORTALSHOT_RC=$?; grep -E '^(PASS|FAIL|PORTAL_PROBE)' /tmp/portalshot.out; "
        "f=$(sed -n 's/.*file:\\/\\/\\([^ ]*\\.png\\).*/\\1/p' /tmp/portalshot.out | head -n 1); "
        "echo SHOTFILE=$f; [ -n \"$f\" ] && ls -l \"$f\"; [ -n \"$f\" ] && [ -s \"$f\" ] && echo SHOT_NONEMPTY")
r = subprocess.run([sys.executable, os.path.join(HERE, "ffsession.py"), arch, tag,
                    "--nofirefox", "--post", post, "--post-timeout", "240"])
try:
    text = open(f"{out}/run-{tag}/post.txt").read()
except OSError:
    text = ""
lines = [l for l in text.splitlines() if re.match(r"(PASS|FAIL|PORTAL_PROBE|PORTALSHOT_RC|SHOTFILE|SHOT_NONEMPTY|-rw)", l)]
print("\n".join(lines))
ok = ("PORTALSHOT_RC=0" in text and "SHOT_NONEMPTY" in text
      and re.search(r"^PASS Screenshot\.Screenshot", text, re.M) is not None)
print(f"portalshottest.py {arch}: {'PASS' if ok else 'FAIL'} (ffsession rc {r.returncode}, {out}/run-{tag})")
sys.exit(0 if ok else 1)
