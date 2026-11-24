#!/usr/bin/env python3
"""failevidtest - runtests saves failure evidence (host-only, no boot).

A forced failing guest command and a forced failing host suite (driver.serial_run and
host_run stubbed, no VM) must leave <out>/fail/<NN>-<name>/ with the serial slice,
output, host info, summary.json and an "evidence:" line; a passing suite leaves nothing.
Usage: failevidtest.py [arch]   (both arches' code paths run regardless)
"""
import contextlib, io, json, os, sys, tempfile
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
tmp = tempfile.mkdtemp(prefix="failevidtest-")
os.environ["LEANDROS_RUN_ID"] = f"fevt{os.getpid()}"
os.environ["FFSESSION_OUT"] = tmp
import driver, runtests

fails = 0
def check(name, ok):
    global fails
    print(("PASS " if ok else "FAIL ") + name)
    fails += not ok

def run_main(arch, cmds, serial):
    lines = []
    open(driver.SERIAL_FULL_LOG, "w").write("boot line\n" * 10)
    state = {"alive": True}
    def fake_serial(c, timeout=0, arch=None, raw=False):
        if c in serial:
            with open(driver.SERIAL_FULL_LOG, "a") as f:
                f.write(f"guest wrote output for {c}\n")
            return dict(serial[c])
        return {"rc": 0, "status": "ok", "secs": 0.1, "output": "1234.56 99.0\n"}
    def fake_host(c, timeout, arch=None, scale=1.0):
        return {"rc": 3, "status": "ok", "secs": 0.2, "output": "host suite died\n"}
    runtests.boot_and_login = lambda *a, **k: True
    driver.serial_run = fake_serial
    runtests.host_run = fake_host
    driver._qemu_pid = lambda: 4242 if state["alive"] else None
    runtests.driver._qemu_pid = driver._qemu_pid
    runtests.subprocess_run_orig = None
    buf = io.StringIO()
    driver.claim_run_id = lambda *a, **k: None  # the in-process runs share this process's lock
    argv = [arch, f"fe-{arch}"]
    for c in cmds:
        argv += ["--suite", c] if False else [c]
    with contextlib.redirect_stdout(buf):
        try:
            rc = runtests.main(argv)
        except SystemExit as e:
            rc = e.code
    return rc, buf.getvalue()

for arch in ("x86_64", "aarch64"):
    runtests._ev.update(used=0, n=0)
    serial = {"badcmd": {"rc": 1, "status": "ok", "secs": 1.0, "output": "fails=1 wait4_scan_children_only\n"},
              "hangcmd": {"rc": None, "status": "timeout", "secs": 5.0, "output": "partial"}}
    rc, out = run_main(arch, ["goodcmd", "badcmd", "hangcmd", "host:nosuchscript.py"], serial)
    fdir = os.path.join(tmp, f"tests-fe-{arch}", "fail")
    ents = sorted(os.listdir(fdir)) if os.path.isdir(fdir) else []
    check(f"{arch}_run_failed", rc != 0)
    if os.environ.get("FEDEBUG"): print(repr(rc), out[-600:], ents)
    check(f"{arch}_three_dirs", len(ents) == 3 and not any("goodcmd" in e for e in ents))
    check(f"{arch}_evidence_lines", out.count("    evidence: ") == 3)
    bad = os.path.join(fdir, ents[0]) if ents else "/nonexistent"
    names = set(os.listdir(bad)) if os.path.isdir(bad) else set()
    check(f"{arch}_files", {"output.txt", "serial-full.log", "host.txt", "summary.json", "uptime.txt"} <= names)
    if os.path.isfile(os.path.join(bad, "summary.json")):
        j = json.load(open(os.path.join(bad, "summary.json")))
        check(f"{arch}_summary", j["suite"] == "badcmd" and j["rc"] == 1)
        check(f"{arch}_output_full", "wait4_scan_children_only" in open(os.path.join(bad, "output.txt")).read())
        check(f"{arch}_serial_has_guest_output", "guest wrote output for badcmd" in open(os.path.join(bad, "serial-full.log")).read())
    hang = os.path.join(fdir, ents[1]) if len(ents) > 1 else "/nonexistent"
    check(f"{arch}_hang_has_monitor_or_skipped", os.path.isfile(os.path.join(hang, "summary.json")))

# cap + failure tolerance: evidence never raises
runtests._ev.update(used=0, n=0)
runtests.EV_CAP = 10
d = runtests.save_evidence(tmp, 0, "x", {"rc": 1, "status": "ok", "secs": 1, "output": "o" * 100}, {"t": 0, "off": 0}, "x86_64", True)
check("cap_respected", runtests._ev["used"] <= 10)
check("unwritable_ok", runtests.save_evidence("/proc/nope", 0, "x", {"rc": 1}, {"t": 0, "off": 0}, "x86_64", True) is None or True)
# big shared log: slice since start + 400 lines before
runtests.EV_CAP = 200 * 1024 * 1024
runtests._ev.update(used=0, n=0)
with open(driver.SERIAL_FULL_LOG, "w") as f:
    f.write("".join(f"old{i}\n" for i in range(300000)))
    off = f.tell()
    f.write("NEWSTUFF\n")
s = runtests._ev_serial_slice(driver.SERIAL_FULL_LOG, off).decode()
check("slice_head_and_body", "NEWSTUFF" in s and "old299999" in s and "old299000" not in s and "SUITE STARTED" in s)
os.unlink(driver.SERIAL_FULL_LOG)
import glob
for p in glob.glob(f"/tmp/leandros-{os.environ['LEANDROS_RUN_ID']}*"): os.unlink(p)
print(f"failevidtest: {'FAIL' if fails else 'PASS'} ({fails} failed)")
sys.exit(1 if fails else 0)
