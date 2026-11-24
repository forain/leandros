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


class MainPhases(unittest.TestCase):
    """pi_sync.main(): phase one swaps the unguarded paths, then the gate runs,
    phase two swaps the PAM-dependent ones only when the gate passed."""

    def run_main(self, gate_ok, fail_first=False):
        import types
        calls = []
        acts = {"put": [("/bin/ls",), ("/bin/sudo",), ("/usr/lib/security/pam_unix.so",)], "mkdir": []}
        saved = {k: getattr(pi_sync, k) for k in ("plan", "describe", "total_bytes", "apply", "shadow_gate")}
        pi_sync.plan = lambda t, R, man, blobdir: (acts, [])
        pi_sync.describe = lambda a, lst: (["x", "y", "z"], {"bin": 3})
        pi_sync.total_bytes = lambda a: 0

        def fake_apply(t, R, a):
            calls.append(("apply", sorted(x[0] for k, v in a.items() if k != "mkdir" for x in v)))
            return ["boom"] if fail_first and len(calls) == 1 else []

        def fake_gate(t, R):
            calls.append(("gate",))
            return gate_ok, "msg"
        pi_sync.apply, pi_sync.shadow_gate = fake_apply, fake_gate
        import tempfile, json
        mf = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
        json.dump({"files": [], "dirs": []}, mf)
        mf.close()
        a = types.SimpleNamespace(manifest=mf.name, list=False, plan_out=None, dry_run=False, board=None)
        t = types.SimpleNamespace(label=lambda: "fake")
        try:
            rc = pi_sync.main(t, FakeR(""), a)
        finally:
            for k, v in saved.items():
                setattr(pi_sync, k, v)
            os.unlink(mf.name)
        return rc, calls

    def test_gate_ok_swaps_guarded_after_gate(self):
        rc, calls = self.run_main(True)
        self.assertEqual(rc, 0)
        self.assertEqual(calls, [("apply", ["/bin/ls"]), ("gate",),
                                 ("apply", ["/bin/sudo", "/usr/lib/security/pam_unix.so"])])

    def test_gate_refused_never_swaps_guarded(self):
        rc, calls = self.run_main(False)
        self.assertEqual(rc, 1)
        self.assertEqual(calls, [("apply", ["/bin/ls"]), ("gate",)])

    def test_earlier_failure_never_swaps_guarded(self):
        rc, calls = self.run_main(True, fail_first=True)
        self.assertEqual(rc, 1)
        self.assertEqual(calls, [("apply", ["/bin/ls"]), ("gate",)])


if __name__ == "__main__":
    unittest.main()
