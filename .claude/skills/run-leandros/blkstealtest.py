"""A descheduled vCPU holding the virtio-blk device lock must not make the other CPUs miss
timer ticks (lane f2fswatchdog, 2026-11-24).

Before the fix `VirtioDisk::rw/flush` took `spin::Mutex::lock()`: every waiter spun with IRQs
masked on a syscall/page-fault path, so one lock holder that the HOST descheduled (QEMU vCPU
thread starved) left the waiting CPUs without timer ticks -> `[WDOG] cpu took no timer tick`
and 1.4-1.7 s `[F2FS] long hold`s (50 runs of f2fsatomictest: 4 failures, steal 98-156 s).

Stress: virtio_blk.inject_every holds the device lock holder for 2.5 s every 250th request (BLKSTEAL_HOST=1 adds real host starvation of vCPU threads). The root disk's writes are throttled, three writers and three readers hammer f2fs
for BLKSTEAL_SECS (default 60) while the host starves one vCPU thread at a time for
2-3.5 s (the vCPU threads are pinned and put in SCHED_IDLE; a pinned busy-loop on the same
host CPU is the starvation, started and killed per stall; only our own QEMU is touched).
Gate: >= 5 `[VIRTIO-BLK] inject:` lines, >= 1 `[LOCKWAIT] VIRTIO_BLK`, guest rc 0, zero `[WDOG]`
lines on a CPU that answers the NMI sample (a starved vCPU does not) in the serial log, no
`[F2FS] long hold` above 4 s (a request injected 2.5 s late under F2FS_MOUNTS legitimately holds
it that long, so the threshold sits above the injection), the guest finishes. `BLKSTEAL_BENCH=1` instead prints throughput (seq write/read, rand 4k read;
no throttle, no stalls) as `BENCH name=ms`.
Usage: LEANDROS_RUN_ID=<id> python3 blkstealtest.py [aarch64|x86_64]
"""
import imgclone
import os, random, re, shutil, subprocess, sys, threading, time
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
DRV = [sys.executable, os.path.join(HERE, "driver.py")]
RUN_ID = os.environ.get("LEANDROS_RUN_ID", "")
TAG = f"-{RUN_ID}" if RUN_ID else ""
LOG = f"/tmp/leandros{TAG}-serial-full.log"
PID_FILE = f"/tmp/leandros{TAG}-qemu.pid"
SECS = int(os.environ.get("BLKSTEAL_SECS", "60"))
BENCH = os.environ.get("BLKSTEAL_BENCH") == "1"
LO, HI = float(os.environ.get("BLKSTEAL_LO", "2.0")), float(os.environ.get("BLKSTEAL_HI", "3.5"))
CONTROL = os.environ.get("BLKSTEAL_CONTROL") == "1"   # run on main: passes only if RED
MSGS = []
HOST = os.environ.get("BLKSTEAL_HOST") == "1"
# Every K-th request answers late: the holder of the device lock is "descheduled" for 2.5 s.
CMDLINE = f"virtio_blk.inject_every={os.environ.get('BLKSTEAL_EVERY', '250')}:{os.environ.get('BLKSTEAL_MS', '2500')} virtio_blk.slow_ms=60000"
CPUBASE = int(os.environ.get("BLKSTEAL_CPUBASE", "24"))

STRESS = (
    "rm -rf /root/st; mkdir -p /root/st; "
    "for i in 1 2 3; do (while [ ! -e /root/st/stop ]; do dd if=/dev/zero of=/root/st/w$i bs=64k count=64 conv=fsync 2>/dev/null; done) & done; "
    "for i in 1 2 3; do (while [ ! -e /root/st/stop ]; do cat /bin/* /lib/* > /dev/null 2>&1; done) & done; "
    "for i in 1 2; do (while [ ! -e /root/st/stop ]; do for f in /bin/ls /bin/cat /bin/hello /bin/sh; do $f /root > /dev/null 2>&1 < /dev/null; done; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null; done) & done; "
    f"sleep {SECS}; touch /root/st/stop; wait; rm -rf /root/st; sync; echo STRESS_DONE"
)
BENCHCMD = (
    "t() { read a _ < /proc/uptime; \"$@\" > /dev/null 2>&1; read b _ < /proc/uptime; echo $(( 10#${b/./} - 10#${a/./} )); }; "
    "for r in 1 2 3; do "
    "echo BENCH seqw=$(t dd if=/dev/zero of=/root/bench.dat bs=1M count=768 conv=fsync); "
    "echo 3 > /proc/sys/vm/drop_caches 2>/dev/null; "
    "echo BENCH seqr=$(t dd if=/root/bench.dat of=/dev/null bs=1M); "
    "echo BENCH rand4k=$(t sh -c 'i=0; while [ $i -lt 1500 ]; do dd if=/root/bench.dat of=/dev/null bs=4k count=1 skip=$(( (RANDOM*32768+RANDOM) % 196608 )) 2>/dev/null; i=$((i+1)); done'); "
    "done; rm -f /root/bench.dat; sync; echo BENCH_DONE"
)


def drv(*args, timeout=200, env=None):
    try:
        p = subprocess.run(DRV + list(args), capture_output=True, text=True, timeout=timeout, env=env, cwd=REPO)
    except subprocess.TimeoutExpired:
        return 124, f"driver {args[0]} timed out after {timeout}s"
    return p.returncode, p.stdout + p.stderr


def vcpu_tids(pid):
    out = []
    for t in os.listdir(f"/proc/{pid}/task"):
        try:
            c = open(f"/proc/{pid}/task/{t}/comm").read().strip()
        except OSError:
            continue
        if re.match(r"CPU \d+/KVM", c):
            out.append((int(c.split()[1].split("/")[0]), int(t)))
    return [t for _, t in sorted(out)]


def staller(pid, stop, stats):
    """Starve one vCPU at a time: pinned SCHED_IDLE vCPU threads + a pinned hog per stall."""
    tids = vcpu_tids(pid)
    if len(tids) < 2:
        stats["err"] = f"vcpu threads: {tids}"; return
    for i, t in enumerate(tids):
        os.sched_setaffinity(t, {CPUBASE + i})
        os.sched_setscheduler(t, os.SCHED_IDLE, os.sched_param(0))
    rng = random.Random(1)
    while not stop.is_set():
        i = rng.randrange(len(tids))
        hog = subprocess.Popen(["taskset", "-c", str(CPUBASE + i), sys.executable, "-c", "while 1: pass"])
        d = rng.uniform(LO, HI)
        time.sleep(d)
        hog.kill(); hog.wait()
        stats["stalls"] += 1; stats["ms"] += int(d * 1000)
        stop.wait(rng.uniform(0.3, 0.6))
    for t in tids:
        os.sched_setscheduler(t, os.SCHED_OTHER, os.sched_param(0))
        os.sched_setaffinity(t, set(range(os.cpu_count())))


def main():
    arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "x86_64")
    out = os.path.join(os.environ.get("FFSESSION_OUT", "/tmp/ffsession"), f"blksteal-{RUN_ID or arch}")
    os.makedirs(out, exist_ok=True)
    src = os.environ.get("LEANDROS_DISK_IMAGE") or os.path.join(REPO, f"leandros-limine-{arch}.img")
    img = os.path.join(out, f"blksteal-{arch}.img")
    if os.path.exists(img): os.unlink(img)
    if subprocess.run([*imgclone.CPC, src, img], stderr=subprocess.DEVNULL).returncode != 0:
        shutil.copyfile(src, img)
    if not BENCH:
        subprocess.run([os.path.join(REPO, "scripts", "set-image-cmdline.sh"), img, CMDLINE],
                       check=True, stdout=subprocess.DEVNULL)
    env = dict(os.environ, LEANDROS_DISK_IMAGE=img)
    if not BENCH:
        env["LEANDROS_ROOTFS_BPS_WRITE"] = os.environ.get("LEANDROS_ROOTFS_BPS_WRITE", "2000000")
    ok = True
    drv("stop", timeout=120)
    try:
        rc, o = drv("start", arch, timeout=400, env=env)
        if rc != 0:
            print("FAIL: boot\n" + o[-800:]); ok = False
        elif drv("login", "root", "root", "240", timeout=300)[0] != 0:
            print("FAIL: root login"); ok = False
        elif BENCH:
            rc, o = drv("run", BENCHCMD, "900", arch, env=env, timeout=1000)
            for l in o.splitlines():
                if l.startswith("BENCH"): print(l.replace("BENCH ", "BENCH "))
            if "BENCH_DONE" not in o:
                ok = False; print("FAIL: bench did not finish\n" + o[-300:])
        else:
            pid = int(open(PID_FILE).read().strip())
            stop = threading.Event(); stats = {"stalls": 0, "ms": 0}
            if HOST:
                th = threading.Thread(target=staller, args=(pid, stop, stats)); th.start()
            else:
                th = threading.Thread(target=lambda: None); th.start()
            try:
                rc, o = drv("run", STRESS, str(SECS + 400), arch, env=env, timeout=SECS + 500)
            finally:
                stop.set(); th.join()
            print(f"host stalls: {stats['stalls']} totalling {stats['ms']} ms" + (f" ERR {stats['err']}" if "err" in stats else ""))
            if HOST and ("err" in stats or stats["stalls"] < 5):
                ok = False; print("FAIL: stall injector did not run")
            if rc != 0:
                ok = False; print(f"FAIL: guest stress rc={rc}")
            if "STRESS_DONE" not in o:
                ok = False; print("FAIL: guest stress did not finish\n" + o[-300:])
            log = open(LOG, "rb").read().decode("utf-8", "replace")
            # A starved vCPU cannot tick or answer the NMI sample: that is the injected host
            # steal, not a kernel fault. A CPU that has no tick but ANSWERS the NMI (IRQs masked,
            # spinning) is the bug.
            wl = []
            for m in re.finditer(r"\[WDOG\] (cpu\d+) took no timer tick[^\n]*", log):
                nm = re.search(re.escape(m.group(1)) + r" NMI sample: ([^\n]*)", log[m.end():m.end() + 600])
                if not (nm and "no answer" in nm.group(1)):
                    wl.append(m.group(0) + " | " + (nm.group(1) if nm else "no NMI line"))
            nall = log.count("took no timer tick")
            holds = [int(x) for x in re.findall(r"\[F2FS\] long hold (\d+) ms", log)]
            print(f"WDOG: {nall} lines, {len(wl)} on CPUs that were not host-stalled; long holds: {sorted(holds)[-5:]}")
            ninj = len(re.findall(r"\[VIRTIO-BLK\] inject:", log))
            nlw = len(re.findall(r"\[LOCKWAIT\] VIRTIO_BLK", log))
            print(f"injected requests: {ninj}; [LOCKWAIT] VIRTIO_BLK lines: {nlw}")
            if ninj < 5:
                ok = False; print(f"FAIL: only {ninj} injected requests (< 5): the stress is vacuous")
            if nlw < 1:
                ok = False; print("FAIL: no [LOCKWAIT] VIRTIO_BLK line: the device lock was never contended")
            if any(h > 4000 for h in holds):
                ok = False; print(f"FAIL: [F2FS] long hold above 4 s (injection is 2.5 s): {sorted(holds)[-3:]}")
            nto = len(re.findall(r"\[IOLOCK\] non-sleeping wait for VIRTIO_BLK timed out", log))
            if nto:
                ok = False; print(f"FAIL: {nto} [IOLOCK] non-sleeping wait for VIRTIO_BLK timed out")
            if wl:
                ok = False; MSGS.append("FAIL: [WDOG]"); print("FAIL: [WDOG] on a running vCPU")
                for l in wl[:8]: print("  " + l[:240])
                open(os.path.join(out, "serial.log"), "w").write(log)
    finally:
        drv("stop", timeout=60)
    if CONTROL:
        # Negative control: on a kernel with the spin lock the same stress MUST show WDOG lines.
        red = any("[WDOG]" in m for m in MSGS)
        print("CONTROL " + ("RED as expected" if red else "NOT RED: the test is vacuous"))
        return 0 if red else 1
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
