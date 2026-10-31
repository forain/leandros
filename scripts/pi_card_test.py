#!/usr/bin/env python3
"""Unit tests for pi_card.py gates and the pi-card verbs' --dry-run (no Pi needed).
Run: python3 scripts/pi_card_test.py"""
import os
import sys
import tempfile
import types
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pi_card as P
import remote as R

TAG = "abc1234-20261031T1200Z"
OLD = "def5678-20261030T0900Z"


def cpio(names):
    out = b""
    for n in names + ["TRAILER!!!"]:
        nb = n.encode() + b"\0"
        h = b"070701" + b"".join(b"%08x" % v for v in [1, 0o100644, 0, 0, 1, 0, 0, 0, 0, 0, 0, len(nb), 0])
        out += h + nb
        out += b"\0" * (-len(out) % 4)
    return out


class Env:
    def __init__(self, d, modules=False, kc="CONFIG_BOARD_RPI5=y\nCONFIG_X=y\n", binpatch=False, tag=TAG):
        self.elf, self.bin, self.initrd, self.kc = (os.path.join(d, n) for n in ("k", "k.bin", "i", "c"))
        open(self.elf, "wb").write(b"\x7fELF-fake " + tag.encode() + b" tail")
        open(self.bin, "wb").write(b"FLAT" + (b"x" if binpatch else b""))
        open(self.initrd, "wb").write(cpio(["bin/init"] + (["lib/modules/6/x.ko"] if modules else [])))
        open(self.kc, "w").write(kc)
        self.good = {"kernel": P.sha256(open(self.elf, "rb").read()), "initrd": P.sha256(open(self.initrd, "rb").read()), "tag": OLD}

    def run(self, **kw):
        g = kw.pop("good", self.good)
        return P.gates_stage(self.elf, self.bin, self.initrd, self.kc, g, objcopy=lambda e: b"FLAT", check_elf=lambda p: None, **kw)


class Gates(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.TemporaryDirectory()

    def test_ok(self):
        f = Env(self.d.name).run()
        self.assertEqual(f["tag"], TAG)

    def test_modules_in_initrd_refused(self):
        e = Env(self.d.name, modules=True)
        with self.assertRaisesRegex(P.Refuse, "lib/modules"):
            e.run()

    def test_kconfig_m_refused(self):
        with self.assertRaisesRegex(P.Refuse, "=m"):
            Env(self.d.name, kc="CONFIG_BOARD_RPI5=y\nCONFIG_FOO=m\n").run()

    def test_not_rpi5_refused(self):
        with self.assertRaisesRegex(P.Refuse, "rpi5"):
            Env(self.d.name, kc="CONFIG_X=y\n").run()

    def test_bin_ne_objcopy_refused(self):
        with self.assertRaisesRegex(P.Refuse, "objcopy"):
            Env(self.d.name, binpatch=True).run()

    def test_sha_not_good_refused_and_unverified_override(self):
        e = Env(self.d.name)
        g = dict(e.good, kernel="0" * 64)
        with self.assertRaisesRegex(P.Refuse, "kexec-verified"):
            e.run(good=g)
        self.assertEqual(e.run(good=g, unverified=True)["tag"], TAG)

    def test_initrd_not_good_refused(self):
        e = Env(self.d.name)
        with self.assertRaisesRegex(P.Refuse, "initrd"):
            e.run(good=dict(e.good, initrd="1" * 64))

    def test_tag_equals_good_refused(self):
        e = Env(self.d.name)
        with self.assertRaisesRegex(P.Refuse, "nothing new"):
            e.run(good=dict(e.good, tag=TAG))

    def test_no_tag_refused(self):
        e = Env(self.d.name)
        open(e.elf, "wb").write(b"\x7fELF no tag here")
        with self.assertRaisesRegex(P.Refuse, "BUILD_TAG"):
            P.gates_stage(e.elf, e.bin, e.initrd, e.kc, e.good, True, objcopy=lambda x: b"FLAT")

    def test_oversize_refused(self):
        e = Env(self.d.name)
        old, P.SLOT_KERNEL = P.SLOT_KERNEL, 2
        try:
            with self.assertRaisesRegex(P.Refuse, "larger"):
                e.run()
        finally:
            P.SLOT_KERNEL = old


class Secrets(unittest.TestCase):
    def test_pi_status_never_cats_cmdline(self):
        c = P.pi_status_cmd()
        self.assertNotIn("cat /proc/cmdline", c)
        self.assertIn("sha256sum /proc/cmdline", c)
        self.assertIn("grep -qw", c)

    def test_remote_pi_status_uses_it(self):
        src = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "remote.py")).read()
        i = src.index("def cmd_pi_status")
        self.assertNotIn("cat /proc/cmdline", src[i:i + 600])


class FakeT:
    pi = True
    host = "x"

    def __init__(self, sha_k, sha_i):
        self.cmds, self.sha = [], (sha_k, sha_i)

    def label(self):
        return "fake"

    def close_master(self):
        pass

    def out(self, cmd, **kw):
        self.cmds.append(cmd)
        if cmd.startswith("sha256sum"):
            return f"{self.sha[0]}  k\n{self.sha[1]}  i\n"
        if cmd == "uname -v":
            return "#1 old"
        return ""

    def run(self, cmd, **kw):
        self.cmds.append(cmd)
        return types.SimpleNamespace(returncode=0, stdout=b"s0 state=good   tag=" + OLD.encode() + b"\n")


class DryRun(unittest.TestCase):
    def ns(self, verb, **kw):
        d = dict(verb=verb, dry_run=True, kernel=None, kernel_bin=None, initrd=None, kconfig=None, unverified=False,
                 accept_root_sector_risk=False, timeout=5)
        d.update(kw)
        return types.SimpleNamespace(**d)

    def test_every_verb_dry_run_writes_nothing(self):
        with tempfile.TemporaryDirectory() as dd:
            e = Env(dd)
            real_objcopy = P.objcopy_binary
            P.objcopy_binary = lambda p: b"FLAT"
            R_stub = types.SimpleNamespace(REPO=dd, pi_check_elf=lambda p: None, bid_check=lambda k, i: None,
                                           die=lambda m: (_ for _ in ()).throw(SystemExit(m)), upload=lambda *a, **k: self.fail("upload in dry-run"),
                                           Target=R.Target, probe=R.probe)
            try:
                for verb in ["status", "setup", "stage", "try", "commit", "abort", "rollback"]:
                    t = FakeT(e.good["kernel"], e.good["initrd"])
                    P.gates_stage.__defaults__  # noqa
                    a = self.ns(verb, kernel=e.elf, kernel_bin=e.bin, initrd=e.initrd, kconfig=e.kc)
                    # gates_stage uses the default objcopy via the module attribute at call time
                    old = P.gates_stage
                    P.gates_stage = lambda *x, **k: old(*x, objcopy=lambda p: b"FLAT", **k)
                    try:
                        P.main(t, R_stub, a)
                    finally:
                        P.gates_stage = old
                    for c in t.cmds:
                        if c.startswith("espctl pi") and not c.startswith("espctl pi status"):
                            self.assertIn("--dry-run", c, f"{verb}: {c}")
                        self.assertFalse(c.startswith("mkdir"), f"{verb}: {c}")
            finally:
                P.objcopy_binary = real_objcopy


if __name__ == "__main__":
    unittest.main()
