#!/usr/bin/env python3
"""Host-side checks for scripts/pi_sync.py's protection rules and passwd/group merge.
Run: python3 scripts/pi_sync_test.py"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pi_sync as S  # noqa: E402

fails = 0


def check(cond, what):
    global fails
    print(("ok   " if cond else "FAIL ") + what)
    fails += 0 if cond else 1


for p in ("/etc/shadow", "/etc/gshadow", "/etc/leandros/wifi.conf", "/etc/leandros/network/profiles.a",
          "/root/.ssh/authorized_keys", "/home/leandro/.ssh/id_ed25519", "/etc/ssh/ssh_host_ed25519_key",
          "/boot/pi/good/kernel", "/boot/kexec", "/tmp/x", "/run/user/0", "/sys/class/block/vda/dev",
          "/sys/dev/char/226:0/uevent", "/proc/self"):
    check(S.classify(p) == "skip", f"skip {p}")
for p in ("/etc/leandros/network.conf", "/home/leandro/.config/x", "/root/.profile", "/var/log/x"):
    check(S.classify(p) == "keep", f"keep {p}")
for p in ("/etc/passwd", "/etc/group"):
    check(S.classify(p) == "merge", f"merge {p}")
for p in ("/bin/btd", "/lib/firmware/brcm/BCM4345C0.hcd", "/etc/init.rc", "/usr/lib/libEGL.so.1",
          "/run/dbus", "/sys/bus/platform/x"):
    check(S.classify(p) == "sync", f"sync {p}")

remote_g = "root:x:0:\nnetdev:x:108:leandro\nlocalgrp:x:500:bob\n"
build_g = "root:x:0:\nnetdev:x:108:leandro\nbluetooth:x:112:leandro\nvideo:x:44:leandro,cosmic-greeter\n"
text, notes = S.merge_db(remote_g, build_g, True)
lines = text.splitlines()
check("localgrp:x:500:bob" in lines, "group merge keeps a target-only group")
check("bluetooth:x:112:leandro" in lines, "group merge adds bluetooth")
check("+bluetooth" in notes and len(notes) == 2, f"group notes {notes}")
text2, notes2 = S.merge_db(text, build_g, True)
check(text2 == text and not notes2, "group merge is idempotent")
remote_g2 = "bluetooth:x:112:alice\n"
t3, n3 = S.merge_db(remote_g2, "bluetooth:x:112:leandro\n", True)
check(t3 == "bluetooth:x:112:alice,leandro\n" and n3 == ["bluetooth+=leandro"], "member added to existing group")
rp = "root:x:0:0:root:/root:/bin/shell\nleandro:x:1000:1000::/home/leandro:/bin/brush\n"
bp = "root:x:0:0:root:/root:/bin/sh\nleandro:x:1000:1000::/home/leandro:/bin/sh\nbtd:x:112:112::/:/bin/false\n"
t4, n4 = S.merge_db(rp, bp, False)
check(t4.startswith(rp) and t4.endswith("btd:x:112:112::/:/bin/false\n") and n4 == ["+btd"],
      "passwd merge: existing users untouched, new user appended")

# manifest generation: rg/grep/dmesg/coreutils are required unless --allow-missing
import tempfile, io, contextlib
_calls = []
def _fake_run(cmd, **kw):
    class R: returncode = 0
    if "mkfs-f2fs-populated.py" in " ".join(map(str, cmd)):
        _calls.append(kw["env"].get("LEANDROS_REQUIRE"))
        if kw["env"].get("LEANDROS_REQUIRE"):
            kw["stdout"].write("MISSING optional: rg (../ripgrep/x)\nFATAL: required binaries missing (LEANDROS_REQUIRE): rg\n")
            kw["stdout"].flush()
            R.returncode = 1
        else:
            open(os.path.join(kw["env"]["LEANDROS_ROOT_MANIFEST"], "manifest.json"), "w").write("{}")
    return R
_real_run = S.subprocess.run
S.subprocess.run = _fake_run
try:
    d = tempfile.mkdtemp()
    buf = io.StringIO()
    died = False
    try:
        with contextlib.redirect_stdout(buf):
            S.build_manifest(d, None, os.path.join(d, "l.log"))
    except SystemExit:
        died = True
    check(died and "MISSING optional: rg" in buf.getvalue(), "manifest without /bin/rg fails with the MISSING message")
    check(_calls[-1] == "rg,grep,dmesg,coreutils", "default requires rg,grep,dmesg,coreutils")
    S.build_manifest(d, None, os.path.join(d, "l2.log"), allow_missing=True)
    check(_calls[-1] is None, "--allow-missing passes without LEANDROS_REQUIRE")
finally:
    S.subprocess.run = _real_run
sys.exit(1 if fails else 0)
