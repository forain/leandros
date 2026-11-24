"""Unit test: pi_sync/deploy-files refuse to swap PAM-dependent files unless
shadowfix ran and `shadowfix --check` says PAM can use /etc/shadow.
Run: python3 scripts/test_pi_sync_gate.py"""
import os
import subprocess
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pi_sync


class FakeR:
    def __init__(self, out):
        self.out = out

    def rrun(self, t, cmd, timeout=60, tries=6, what=None):
        return subprocess.CompletedProcess([], 0, self.out.encode(), b"")


class Gate(unittest.TestCase):
    def test_guarded(self):
        for p in ("/bin/sudo", "/bin/su", "/usr/sbin/greetd", "/usr/lib/libpam.so.0", "/etc/pam.d/sudo",
                  "/usr/lib/security/pam_unix.so", "/usr/sbin/unix_chkpwd"):
            self.assertTrue(pi_sync.guarded(p), p)
        for p in ("/bin/ls", "/usr/sbin/shadowfix", "/etc/shadow"):
            self.assertFalse(pi_sync.guarded(p), p)

    def test_ok(self):
        out = "SHADOWFIX-RC=0\nshadowfix: /etc/shadow usable by PAM\nSHADOWFIX-CHECK=0\n"
        self.assertTrue(pi_sync.shadow_gate(None, FakeR(out))[0])

    def test_refusals(self):
        self.assertFalse(pi_sync.shadow_gate(None, FakeR("SHADOWFIX-MISSING\n"))[0])
        self.assertFalse(pi_sync.shadow_gate(None, FakeR("SHADOWFIX-RC=0\nSHADOWFIX-CHECK=3\n"))[0])
        self.assertFalse(pi_sync.shadow_gate(None, FakeR("SHADOWFIX-RC=2\nSHADOWFIX-CHECK=0\nusable by PAM\n"))[0])
        # an old shadowfix ignores --check: exit 0 but no success line
        self.assertFalse(pi_sync.shadow_gate(None, FakeR("SHADOWFIX-RC=0\nSHADOWFIX-CHECK=0\n"))[0])


if __name__ == "__main__":
    unittest.main()
