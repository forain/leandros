#!/usr/bin/env python3
"""straysweeptest.py -- init's stray sweep spares detached ssh jobs.

When greetd exits, init kills every process the kernel reparented to it
outside the console/VT logins (`sweep_strays`): that is how a dead
graphical session's leftovers (a greeter spinning on a broken Wayland
socket) go away. A job started over ssh with `nohup ... &` is reparented
to init as soon as its ssh session closes, so the sweep used to kill it
too: on the laptop, `nohup sh s0.sh &` (which stops the desktop and then
runs `dd ... | tail`) lost sh and dd silently, with nothing in klog
(openbugs #89, lane ddkill).

The VM must be running with sshd and the graphical login (greetd) up,
same LEANDROS_RUN_ID. Over ssh as leandro it starts three detached jobs,
then kills greetd from the root serial console:
  - an ssh job (`nohup sh -c 'exec sleep'`): must survive the sweep;
  - an ssh pipeline (`nohup sh -c 'sleep | cat'`), the s0.sh shape: the
    shell and both pipeline children must survive;
  - a control job with SSH_CONNECTION/SSH_CLIENT unset, i.e. a stray of
    no ssh session: must be killed (proves the sweep ran).
Prints `stray_<step>: PASS|FAIL` and a summary; exit 0 iff all passed.
"""
import os
import hostclean
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402
from sshsmoke import Ssh, target  # noqa: E402

RESULTS = []


def step(name, ok, detail=""):
    RESULTS.append((name, ok))
    print(f"stray_{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)
    return ok


def serial(cmd, timeout=30):
    r = driver.serial_run(cmd, timeout=timeout)
    # Kernel console lines ("[FORK] ...") can interleave with the output.
    lines = [ln for ln in (r["output"] or "").splitlines() if not ln.lstrip().startswith("[")]
    return r["rc"], "\n".join(lines).strip()


def alive(pid):
    # Alive and not a zombie: /proc/<pid>/stat state field.
    rc, out = serial(f"cat /proc/{pid}/stat 2>/dev/null | cut -d')' -f2 | cut -d' ' -f2,3")
    return out.split()[:2] if out else None


def main():
    host, port = target()
    tmp = tempfile.mkdtemp(prefix="straysweep-")
    hostclean.track(tmp)
    ssh = Ssh(host, port, tmp)
    user, pw = "leandro", "leandro"
    deadline = time.time() + 90
    while time.time() < deadline:
        r = subprocess.run(["ssh-keyscan", "-T", "5", "-p", str(port), host], capture_output=True, timeout=30)
        if b"ssh-" in r.stdout:
            break
        time.sleep(2)
    dm = ""
    deadline = time.time() + 120
    while time.time() < deadline:
        _, dm = serial("cat /run/greetd-init.pid 2>/dev/null")
        if dm.isdigit():
            break
        time.sleep(3)
    if not step("greetd_running", dm.isdigit(), f"pid={dm!r}"):
        return 1
    d = "/tmp/straysweep"
    jobs = (
        f"mkdir -p {d}; "
        f"nohup sh -c 'echo $$ > {d}/plain; exec sleep 6001' </dev/null >/dev/null 2>&1 & "
        f"nohup sh -c 'echo $$ > {d}/pipe; sleep 6002 | cat' </dev/null >/dev/null 2>&1 & "
        f"nohup sh -c 'unset SSH_CONNECTION SSH_CLIENT; echo $$ > {d}/ctl; exec sleep 6003' "
        f"</dev/null >/dev/null 2>&1 & "
        "sleep 1; echo started"
    )
    rc, out, err = ssh.run(user, jobs, password=pw, timeout=60)
    step("jobs_started", rc == 0 and "started" in out, f"rc={rc} err={err.strip()[-120:]!r}")
    time.sleep(2)
    _, pids = serial(f"cat {d}/plain {d}/pipe {d}/ctl")
    pids = pids.split()
    if not step("pids", len(pids) == 3 and all(p.isdigit() for p in pids), f"{pids}"):
        return 1
    plain, pipe, ctl = pids
    # The pipeline's children: everything whose ppid is the pipeline shell.
    _, kids = serial(f"for s in /proc/[0-9]*/stat; do cut -d')' -f2 < $s | cut -d' ' -f3 | "
                     f"{{ read pp; [ \"$pp\" = {pipe} ] && echo ${{s#/proc/}}; }}; done 2>/dev/null")
    kids = [k.split("/")[0] for k in kids.split() if k.split("/")[0].isdigit()]
    step("pipe_children", len(kids) == 2, f"{kids}")
    before = {p: alive(p) for p in (plain, pipe, ctl)}
    # greetd is init's child, so its ppid is init's pid (not necessarily 1).
    init = (alive(dm) or ["", ""])[1]
    step("reparented_to_init", init.isdigit() and all(v and v[1] == init for v in before.values()),
         f"init={init} {before}")
    serial(f"kill -9 {dm}")
    time.sleep(4)
    after = {p: alive(p) for p in (plain, pipe, ctl, *kids)}
    ok = lambda p: after[p] is not None and after[p][0] != "Z"  # noqa: E731
    step("ssh_job_survives", ok(plain), f"{after[plain]}")
    step("ssh_pipeline_survives", ok(pipe) and all(ok(k) for k in kids),
         f"sh={after[pipe]} kids={[after[k] for k in kids]}")
    step("non_ssh_stray_killed", not ok(ctl), f"{after[ctl]}")
    serial(f"kill -9 {plain} {pipe} {ctl} {' '.join(kids)} 2>/dev/null; rm -rf {d}")
    bad = [n for n, good in RESULTS if not good]
    print(f"STRAYSWEEP summary: {len(RESULTS) - len(bad)}/{len(RESULTS)} passed"
          + (f"; failed: {' '.join(bad)}" if bad else ""), flush=True)
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(hostclean.run(main))
