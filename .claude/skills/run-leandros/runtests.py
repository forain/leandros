#!/usr/bin/env python3
"""runtests.py <arch> <tag> [options] [cmd ...] -- boot, serial root login, run
each command, report each one's exit status reliably.

Every command goes through driver.serial_run: a nonce sentinel plus a status
file in the guest, so interleaved kernel console lines cannot turn a finished
run into `RC=?` (see the comment above serial_run in driver.py).

Options:
  --gpu none|virgl|venus   boot device (default none; RUNTESTS_VIRGL=1 = virgl)
  --virgl / --venus        same as --gpu virgl / --gpu venus
  --suite NAME             add a named list of commands (repeatable):
                             regress  the 13 regression suites + vfstest + nettest
                             net      /bin/nettest alone
                             drm      /bin/drmsmoke (needs --gpu virgl|venus)
                           no commands and no --suite = regress
  --repeat N               boot N times, run the whole list each time
  --timeout S              per-command timeout in seconds, before scaling
                           (default 300)
  --mode M                 driver.py start mode (uefi, uefi-tcg, ...)
  --keep                   leave QEMU running after the last boot

Waits scale with the accelerator (driver.wait_scale: x3 on TCG, x1 on HVF/KVM;
LEANDROS_WAIT_SCALE overrides).

Output: $FFSESSION_OUT (default /tmp/ffsession)/tests-<tag>/
  run-<i>/results.txt (every command's output), run-<i>/serial.log,
  summary.json. One line per command on stdout:
  `=== <cmd>: RC=<n> [<status>, <secs>s] fails=<k>`, then a summary.
Exit status 0 iff every command returned 0 with a known status.
LEANDROS_RUN_ID defaults to "runtests" (scopes driver.py's sockets/logs).
"""
import json
import os
import re
import subprocess
import sys
import time

os.environ.setdefault("LEANDROS_RUN_ID", "runtests")
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")
sys.path.insert(0, HERE)
os.chdir(REPO)
import driver  # noqa: E402

SUITES = {
    "regress": ["/bin/sigtest", "/bin/sigtest2", "/bin/memtest", "/bin/scmtest",
                "/bin/polltest", "/bin/forktest", "/bin/exectest", "/bin/pthreadtest",
                "/bin/epolltest", "/bin/timertest", "/bin/jobtest", "/bin/waittest",
                "/bin/sigchldtest", "/bin/vfstest", "/bin/nettest"],
    # nettest finds the gateway in /proc/net/route and the DNS server in
    # /etc/resolv.conf, so it runs unchanged on slirp (10.0.2.x) and on
    # socket_vmnet (192.168.105.x); LEANDROS_NET=user forces slirp. Its DNS
    # cases resolve example.com, so the host needs internet access.
    "net": ["/bin/nettest"],
    "drm": ["/bin/drmsmoke"],
}


def main(argv):
    if len(argv) < 2 or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 2
    arch, tag = argv[0], argv[1]
    rest = argv[2:]
    gpu = "virgl" if os.environ.get("RUNTESTS_VIRGL") else "none"
    repeat, base_timeout, mode, keep = 1, 300.0, "uefi", False
    cmds, suites = [], []
    i = 0
    while i < len(rest):
        a = rest[i]
        if a == "--gpu":
            gpu = rest[i + 1]; i += 2
        elif a in ("--virgl", "--venus"):
            gpu = a[2:]; i += 1
        elif a == "--suite":
            suites.append(rest[i + 1]); i += 2
        elif a == "--repeat":
            repeat = int(rest[i + 1]); i += 2
        elif a == "--timeout":
            base_timeout = float(rest[i + 1]); i += 2
        elif a == "--mode":
            mode = rest[i + 1]; i += 2
        elif a == "--keep":
            keep = True; i += 1
        else:
            cmds.append(a); i += 1
    if not cmds and not suites:
        suites = ["regress"]
    for s in suites:
        if s not in SUITES:
            print(f"unknown suite {s!r}; known: {', '.join(SUITES)}")
            return 2
        cmds += SUITES[s]

    scale = driver.wait_scale(arch, mode)
    accel = driver.accel_kind(arch, mode)
    timeout = base_timeout * scale
    out = os.path.join(os.environ.get("FFSESSION_OUT", "/tmp/ffsession"), f"tests-{tag}")
    os.makedirs(out, exist_ok=True)
    print(f"runtests: arch={arch} accel={accel} gpu={gpu} scale={scale} "
          f"timeout={timeout:.0f}s repeat={repeat} out={out}", flush=True)

    summary = {"arch": arch, "accel": accel, "gpu": gpu, "runs": []}
    all_ok = True
    for run in range(repeat):
        rdir = os.path.join(out, f"run-{run}") if repeat > 1 else out
        os.makedirs(rdir, exist_ok=True)
        entry = {"run": run, "boot": None, "results": []}
        summary["runs"].append(entry)
        if driver._qemu_pid() is not None:
            subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)
        start = [sys.executable, DRV, "start", arch, mode] + ([f"--{gpu}"] if gpu != "none" else [])
        booted = False
        for attempt in range(2):
            r = subprocess.run(start, capture_output=True, text=True, timeout=900)
            if r.returncode == 0:
                lg = subprocess.run([sys.executable, DRV, "login", "root", "root",
                                     str(int(60 * scale))],
                                    capture_output=True, text=True, timeout=900)
                if lg.returncode == 0:
                    booted = True
                    break
                print(f"run {run}: login failed (attempt {attempt}): {lg.stdout[-300:]}{lg.stderr[-300:]}", flush=True)
            else:
                print(f"run {run}: boot failed (attempt {attempt}): {r.stdout[-300:]}{r.stderr[-300:]}", flush=True)
            subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)
        entry["boot"] = "ok" if booted else "failed"
        if not booted:
            all_ok = False
            continue
        with open(os.path.join(rdir, "results.txt"), "w") as res:
            for c in cmds:
                try:
                    r = driver.serial_run(c, timeout=timeout, arch=arch)
                except Exception as e:  # noqa: BLE001
                    r = {"rc": None, "status": f"error {e}", "output": "", "secs": 0}
                fails = re.findall(r"^\s*(\S+): FAIL\b", r["output"], re.M)
                rc = r["rc"] if r["rc"] is not None else "?"
                ok = r["rc"] == 0 and r["status"] in ("ok", "rc-file")
                all_ok &= ok
                print(f"=== {c}: RC={rc} [{r['status']}, {r['secs']}s] fails={len(fails)}"
                      + (f" {','.join(fails)}" if fails else ""), flush=True)
                res.write(f"=== {c}: RC={rc} [{r['status']}, {r['secs']}s]\n{r['output']}\n")
                res.flush()
                entry["results"].append({"cmd": c, "rc": r["rc"], "status": r["status"],
                                         "secs": r["secs"], "fails": fails})
        subprocess.run(["cp", driver.SERIAL_LOG, os.path.join(rdir, "serial.log")])
        if keep and run == repeat - 1:
            break
        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)

    with open(os.path.join(out, "summary.json"), "w") as f:
        json.dump(summary, f, indent=1)
    # Per-command tally over all runs.
    print("--- summary")
    for c in cmds:
        rows = [x for e in summary["runs"] for x in e["results"] if x["cmd"] == c]
        good = sum(1 for x in rows if x["rc"] == 0 and x["status"] in ("ok", "rc-file"))
        unknown = sum(1 for x in rows if x["rc"] is None)
        print(f"{c}: {good}/{len(rows)} RC=0, {unknown} unknown")
    bad_boots = sum(1 for e in summary["runs"] if e["boot"] != "ok")
    print(f"boots failed: {bad_boots}/{repeat}; overall: {'PASS' if all_ok else 'FAIL'}")
    return 0 if all_ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
