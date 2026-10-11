#!/usr/bin/env python3
"""pagecache-crashtest.py <arch> — page cache + fsync ordering across a SIGKILL.

Data that exists only as dirty page-cache pages (stored through a
MAP_SHARED mapping, still mapped) must reach f2fs as write requests before
fsync/fdatasync/syncfs/sync decide whether a checkpoint is needed (lane
fsyncbatch skips the checkpoint for a file it thinks is clean). Also checks
plain write-through + `sync -d`, and msync(MS_SYNC) (pcachetest quick's
fsync_durable + `pcachetest verify`). Adapted from lane fsyncbatch's
crashtest.py.
"""
import os, sys, time, signal
HERE = os.path.dirname(os.path.abspath(__file__))
WT = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))

os.environ.setdefault("LEANDROS_RUN_ID", "pccrash")
os.environ.pop("LEANDROS_USB_STICK", None)
sys.path.insert(0, HERE)
os.chdir(WT)
import driver  # noqa

arch = sys.argv[1]
fails = []


def boot():
    driver.cmd_start(arch)
    driver.cmd_login("root", "root", timeout=60 * driver.wait_scale(arch))


def run(c, t=180):
    r = driver.serial_run(c, timeout=t, arch=arch)
    print(f"$ {c}\n{r['output']}\n[rc={r['rc']} {r['status']} {r['secs']}s]", flush=True)
    return r


def hard_kill():
    pid = driver._qemu_pid()
    if pid:
        os.kill(pid, signal.SIGKILL)
        time.sleep(1)
    driver.cmd_stop()


def expect(r, want, what):
    if want not in r["output"]:
        fails.append(f"{what}: wanted {want!r}")
        print(f"FAIL {what}", flush=True)
    else:
        print(f"ok   {what}", flush=True)


boot()
run("rm -f /root/pc_w /root/pc_m* /root/pc_h* /root/pc_nosync /root/pc_dur.bin; sync; echo SETUP_OK")
# msync(MS_SYNC) and fsync of written data (pcachetest's own cases).
expect(run("/bin/pcachetest quick", 600), "fsync_durable: PASS", "pcachetest quick")
# Plain write (write-through to f2fs) + per-file fsync.
run("echo W1 > /root/pc_w && sync -d /root/pc_w && echo W_DONE")
# Dirty mapped pages, synced by the mapping process itself, still mapped.
for tag, how in (("M1", "fsync"), ("M2", "fdatasync"), ("M3", "syncfs"), ("M4", "sync")):
    r = run(f"(/bin/pcachetest mmapsync /root/pc_{tag.lower()} {tag} {how} > /tmp/{tag}.out 2>&1 &); "
            f"sleep 3; cat /tmp/{tag}.out")
    expect(r, "MMAPSYNC_DONE", f"mmapsync {how} returned")
# Dirty mapped pages held by another process, synced from outside.
r = run("(/bin/pcachetest mmaphold /root/pc_h1 H1 120 > /tmp/h1.out 2>&1 &); sleep 2; "
        "cat /tmp/h1.out; sync -d /root/pc_h1 && echo H1_SYNCED")
expect(r, "H1_SYNCED", "sync -d of a file another process holds dirty")
r = run("(/bin/pcachetest mmaphold /root/pc_h2 H2 120 > /tmp/h2.out 2>&1 &); sleep 2; "
        "cat /tmp/h2.out; sync && echo H2_SYNCED")
expect(r, "H2_SYNCED", "sync(2) with a file another process holds dirty")
# Control: a mapped store nobody synced (may be lost; must not corrupt).
run("(/bin/pcachetest mmaphold /root/pc_nosync N1 120 > /dev/null 2>&1 &); sleep 1; echo NOSYNC_STORED")
hard_kill()
boot()
for tag in ("M1", "M2", "M3", "M4", "H1", "H2"):
    expect(run(f"tr -d '\\000' < /root/pc_{tag.lower()}"), tag, f"{tag} survived SIGKILL")
expect(run("cat /root/pc_w"), "W1", "write + sync -d survived SIGKILL")
expect(run("/bin/pcachetest verify"), "verify_durable: PASS", "msync(MS_SYNC) data survived SIGKILL")
run("ls -la /root/pc_nosync; tr -d '\\000' < /root/pc_nosync; echo")
expect(run("echo POST > /root/pc_post && sync -d /root/pc_post && cat /root/pc_post"), "POST",
       "volume writable after crash")
run("rm -f /root/pc_w /root/pc_m* /root/pc_h* /root/pc_nosync /root/pc_post /root/pc_dur.bin; sync")
driver.cmd_stop()
print("PC_CRASHTEST", arch, "PASS" if not fails else "FAIL", fails)
sys.exit(1 if fails else 0)
