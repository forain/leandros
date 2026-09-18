#!/usr/bin/env python3
"""gpuirqbench — boot the default (greeter) desktop on one arch, drive pointer
motion for N seconds and report the [DRMSTAT] control-queue census: presents
per second, completion-observation latency of asynchronous commands, vCPU
time in synchronous waits, interrupts taken, flips delivered on the fence
path. The kernel must be built with `DRM_STATS = true`
(drivers/src/drm_device_interface.rs); nothing is printed otherwise.

usage: gpuirqbench.py <tag> [--arch aarch64|x86_64] [--dur 60] [--settle 45]
                            [--noinput] [--keep]

Every path is env-driven (LEANDROS_RUN_ID, GPUIRQBENCH_REPO, GPUIRQBENCH_OUT)
so two trees — a baseline and a candidate — can be measured from one shell.
"""
import os, sys, time, select, json, re, subprocess, threading

os.environ.setdefault("LEANDROS_RUN_ID", "gpuirqbench")
REPO = os.environ.get("GPUIRQBENCH_REPO") or os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, f"{REPO}/.claude/skills/run-leandros")
os.chdir(REPO)
import driver  # noqa: E402

OUT = os.environ.get("GPUIRQBENCH_OUT") or os.path.expanduser("~/gpuirqbench")
os.makedirs(OUT, exist_ok=True)

args = sys.argv[1:]
TAG = args[0]
def opt(name, default, conv=int):
    if name in args:
        return conv(args[args.index(name) + 1])
    return default
ARCH = opt("--arch", "aarch64", str)
DUR = opt("--dur", 60)
SETTLE = opt("--settle", 45)
NOINPUT = "--noinput" in args
# Keystrokes into the greeter's focused field (5/s): pointer motion alone is
# a cursor-plane commit and never touches the control queue, so this is what
# makes the compositor re-render and present the primary plane.
KEYS = "--keys" in args
KEEP = "--keep" in args

LOGF = open(f"{OUT}/{TAG}-serial.log", "wb")
STAT = []
lock = threading.Lock()
stop = threading.Event()

def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)

STAT_RE = re.compile(rb"\[DRMSTAT\] (.*)")
KV_RE = re.compile(rb"(\w+)=0x([0-9a-fA-F]+)")

def serial_reader(s):
    buf = b""
    while not stop.is_set():
        if not select.select([s], [], [], 0.2)[0]:
            continue
        try:
            chunk = s.recv(65536)
        except BlockingIOError:
            continue
        if not chunk:
            return
        LOGF.write(chunk); LOGF.flush()
        buf += chunk
        while b"\n" in buf:
            line, _, buf = buf.partition(b"\n")
            m = STAT_RE.search(line)
            if m:
                d = {k.decode(): int(v, 16) for k, v in KV_RE.findall(m.group(1))}
                with lock:
                    STAT.append((time.time(), d))

def input_loop(dur):
    import math
    ses = driver._qmp_open()
    t0 = time.time(); i = 0
    try:
        while time.time() - t0 < dur and not stop.is_set():
            ang = i * 0.2
            x = int(16000 + 8000 * math.cos(ang)); y = int(16000 + 8000 * math.sin(ang))
            ev = [{"type": "abs", "data": {"axis": "x", "value": x}},
                  {"type": "abs", "data": {"axis": "y", "value": y}}]
            driver._qmp_command(ses, "input-send-event", {"events": ev}, timeout=5)
            if KEYS and i % 2 == 0:
                key = "abcdefghijklmnopqrstuvwxyz"[(i // 2) % 26]
                for down in (True, False):
                    driver._qmp_command(ses, "input-send-event", {"events": [
                        {"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": key}}}]}, timeout=5)
            i += 1
            time.sleep(max(0, (t0 + i * 0.1) - time.time()))
    finally:
        ses.close()

def main():
    log(f"=== gpuirqbench {TAG} arch={ARCH} dur={DUR} settle={SETTLE} repo={REPO} ===")
    if driver._qemu_pid():
        driver.cmd_stop()
        time.sleep(1)
    driver.cmd_start(ARCH, "uefi")
    s = driver._connect_with_retry(driver.SERIAL_SOCK)
    s.setblocking(False)
    th = threading.Thread(target=serial_reader, args=(s,), daemon=True); th.start()
    log("greeter booting; settling")
    time.sleep(SETTLE)
    with lock:
        s0 = STAT[-1][1] if STAT else {}
    log(f"after settle: {len(STAT)} samples, flips_sub={s0.get('flips_sub')} ctrlq_n={s0.get('ctrlq_n')} "
        f"ctrlq_irqs={s0.get('ctrlq_irqs')}")
    tw0 = time.time()
    if not NOINPUT:
        it = threading.Thread(target=input_loop, args=(DUR,), daemon=True); it.start()
        it.join(DUR + 30)
    else:
        time.sleep(DUR)
    tw1 = time.time()
    time.sleep(3)
    with lock:
        samples = [(t, d) for (t, d) in STAT if tw0 - 2.5 <= t <= tw1 + 0.5]
    if len(samples) < 2:
        log("NOT ENOUGH DRMSTAT SAMPLES", len(samples), "(is DRM_STATS on?)")
    else:
        a, b = samples[0][1], samples[-1][1]
        secs = samples[-1][0] - samples[0][0]
        def dl(k): return b.get(k, 0) - a.get(k, 0)
        fl = dl("flips_sub"); fd = dl("flips_del"); cq = dl("ctrlq_n")
        asy = dl("ctrlq_async"); syn = dl("ctrlq_sync")
        lat = dl("ctrlq_lat_us"); spin = dl("ctrlq_us")
        log(f"WINDOW {secs:.1f}s: flips_sub={fl} ({fl/secs:.2f}/s) flips_del={fd} ({fd/secs:.2f}/s) "
            f"flips_irq={dl('flips_irq')} atomic={dl('atomic')} curs_mv={dl('curs_mv')} evpush={dl('evpush')}")
        log(f"  ctrlq_n={cq} async={asy} sync={syn} irqs={dl('ctrlq_irqs')} parked={dl('ctrlq_parked')} "
            f"intx_spurious={dl('intx_spurious')} timeouts={dl('ctrlq_to')} room_waits={dl('ctrlq_room')}")
        log(f"  async completion latency: mean={lat/max(asy,1):.0f}us max_sofar={b.get('ctrlq_lat_max')}us; "
            f"sync wait vCPU time={spin}us ({spin/secs/1e4:.3f}% of wall) mean={spin/max(syn,1):.0f}us "
            f"max_sofar={b.get('ctrlq_max')}us")
        # Cumulative since boot, for the boot-time census (the session start
        # is where the reply-needing commands are).
        log(f"  since boot: ctrlq_n={b.get('ctrlq_n')} sync={b.get('ctrlq_sync')} "
            f"sync_wait_us={b.get('ctrlq_us')} sync_max_us={b.get('ctrlq_max')} "
            f"async={b.get('ctrlq_async')} async_lat_us={b.get('ctrlq_lat_us')} "
            f"async_lat_max_us={b.get('ctrlq_lat_max')} irqs={b.get('ctrlq_irqs')} "
            f"parked={b.get('ctrlq_parked')} kicks={b.get('park_kicks')} "
            f"flips_del={b.get('flips_del')} flips_irq={b.get('flips_irq')}")
        with open(f"{OUT}/{TAG}-stat.json", "w") as f:
            json.dump({"window": [tw0, tw1], "stat": samples}, f)
    stop.set()
    if not KEEP:
        driver.cmd_stop()
    log("done")

if __name__ == "__main__":
    main()
