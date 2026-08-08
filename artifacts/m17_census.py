#!/usr/bin/env python3
"""Host half of /bin/m17-census.

Two questions in one boot.

CENSUS. `busd` 0.5.0 answers a method call addressed to a well-known name that
nobody owns by dropping it with a `warn!` and never replying, and neither busd
nor zbus imposes a reply timeout — so a blocking caller waits forever. That is
what parks libcosmic's `run_single_instance` probe. The standing record has
only a TAIL of one session log, holding two names; a tail is not a census, and
the question "which components has this been silently blocking, every boot,
since they were staged?" is answered by counting `unknown destination:` BY NAME
over a complete log. The guest half gives busd a file of its own to write that
log into, because two LeandrOS writers on one shared fd overwrite each other
and an absent name would otherwise prove nothing.

MEMORY. The busd patch that replies `ServiceUnknown` was tried once and the
image crash-looped, and the crash was never attributed. The faults are a null
dereference inside libxkbcommon, whose `xkb_context_new` returns NULL only when
its `calloc` fails; the leading explanation is that the reply UNBLOCKED four
more iced applications into a 2 GiB guest that was already running a softpipe
compositor. That is a claim about physical memory, so this run reads
`sysinfo(2)` through /bin/meminfo at every phase boundary, and the whole run is
repeated at a larger `-m` with nothing else changed.

WHY A SERIES AND NOT A SHOT. A single capture cannot tell "the pixels never
arrive" from "the pixels had not arrived yet"; the compositor takes 6-26 s to
settle. Every phase is sampled more than once and the first frame of a run is
treated as suspect, because console writes repaint the framebuffer and the
framebuffer is the scanout.

WHY THE CONTROL IS A MISSING BINARY. `nosuchbinary_xyz42` must be reported as
FAILING before anything else runs. If it is not, absence and failure are
indistinguishable on this console and every null result below is unfalsifiable.

usage: m17_census.py [outdir] [arch]
       LEANDROS_QEMU_MEM=4G m17_census.py /tmp/m17-4g x86_64
"""

import collections
import os
import re
import select
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
DRIVER = os.path.join(REPO, ".claude", "skills", "run-leandros", "driver.py")
SERIAL_SOCK = "/tmp/leandros-serial.sock"
SERIAL_LOG = "/tmp/leandros-serial.log"


class Serial:
    """QEMU's serial chardev serves ONE client at a time, so driver.py must be
    finished with it before this connects. The guest console asks for its cursor
    position with ESC[6n and blocks until answered, so every read path answers.

    Staying connected and READING is now the correct thing to do: a client that
    connects and stops reading back-pressures the 16550, and `putc` is reached
    from the timer IRQ. That used to wedge CPU 0; the cycle-counter deadline in
    `putc` bounds it, but a reader that keeps draining is still the only shape
    with no console loss at all."""

    def __init__(self, tee=None):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(SERIAL_SOCK)
        self.s.setblocking(False)
        self.tee = open(tee, "ab", buffering=0) if tee else None
        self.buf = b""
        self.pump(0.5)

    def send(self, cmd):
        # Drop what is already buffered: a control satisfiable by text that
        # predates the command is not a control.
        self.buf = b""
        payload = (cmd + "\n").encode()
        self.s.setblocking(True)
        for i in range(0, len(payload), 8):      # 16-byte PL011 RX FIFO
            self.s.sendall(payload[i:i + 8])
            time.sleep(0.02)
        self.s.setblocking(False)

    def pump(self, secs):
        end = time.time() + secs
        while True:
            left = end - time.time()
            if left <= 0:
                return
            if select.select([self.s], [], [], min(0.2, left))[0]:
                try:
                    c = self.s.recv(65536)
                except BlockingIOError:
                    continue
                if not c:
                    return
                if b"\x1b[6n" in c:
                    self.s.setblocking(True)
                    self.s.sendall(b"\x1b[24;1R" * c.count(b"\x1b[6n"))
                    self.s.setblocking(False)
                self.buf += c
                if self.tee:
                    self.tee.write(c)

    def read_until(self, pattern, timeout):
        end = time.time() + timeout
        while True:
            txt = self.buf.decode("utf-8", "replace")
            m = pattern.search(txt)
            if m:
                self.buf = txt[m.end():].encode("utf-8", "replace")
                return m, txt[:m.end()]
            if time.time() >= end:
                return None, txt
            self.pump(0.4)


MARK = re.compile(r"M17: MARK (\w+) (\d+)|M17: CAPTURES DONE")

# The guest's own instruments and the kernel's, in the one stream that carries
# both. x86_64 prints `user page fault ...: task killed`; aarch64 prints
# `[EXC] EL0 Fault!`. Both are matched so one analyser serves either arch.
RE_UNKNOWN = re.compile(r"unknown destination: (\S+)")
RE_FAULT_X86 = re.compile(r"user page fault RIP=0x(\S+) CR2=0x(\S+) CR3=0x\S+ err=0x(\S+)")
RE_FAULT_A64 = re.compile(r"\[EXC\] EL0 Fault! PID=(\d+) ESR=(\S+) FAR=(\S+)")
RE_MEM = re.compile(r"MEMINFO (\w+) total=(\d+) free=(\d+) used=(\d+) usedpct=(\d+)(?: procs=(\d+))?")


def d(*args, t=120):
    return subprocess.run([sys.executable, DRIVER, *args],
                          capture_output=True, text=True, timeout=t)


def readppm(path):
    """P6 binary PPM -> (w, h, bytes). QEMU's screendump writes maxval 255."""
    with open(path, "rb") as f:
        raw = f.read()
    if not raw.startswith(b"P6"):
        return None
    fields, i = [], 2
    while len(fields) < 3:
        while i < len(raw) and raw[i:i + 1].isspace():
            i += 1
        if raw[i:i + 1] == b"#":
            while i < len(raw) and raw[i] != 0x0A:
                i += 1
            continue
        j = i
        while j < len(raw) and not raw[j:j + 1].isspace():
            j += 1
        fields.append(int(raw[i:j]))
        i = j
    return fields[0], fields[1], raw[i + 1:]


def census_px(px):
    """How much of the frame is not the background colour, and how many distinct
    colours it holds. Stride-sampled; exactness is not the point."""
    n = len(px) // 3
    hist = {}
    for k in range(0, n * 3, 3 * 37):
        hist[px[k:k + 3]] = hist.get(px[k:k + 3], 0) + 1
    top = sorted(hist.items(), key=lambda kv: -kv[1])[0]
    nonbg = sum(v for c, v in hist.items() if c != top[0])
    return len(hist), top[0].hex(), nonbg / max(1, sum(hist.values()))


def diffbox(a, b, w, h):
    """Bounding box of the pixels that differ, sampled every 4th row/col."""
    x0 = y0 = 1 << 30
    x1 = y1 = -1
    n = 0
    for y in range(0, h, 4):
        row = y * w * 3
        for x in range(0, w, 4):
            k = row + x * 3
            if a[k:k + 3] != b[k:k + 3]:
                n += 1
                x0, x1 = min(x0, x), max(x1, x)
                y0, y1 = min(y0, y), max(y1, y)
    if n == 0:
        return None
    return (x0, y0, x1, y1, n)


def analyse(path):
    txt = open(path, "rb").read().decode("utf-8", "replace")

    print("\n" + "=" * 72)
    print("CENSUS — `busd::peers: unknown destination:` by name")
    print("=" * 72)
    names = collections.Counter(RE_UNKNOWN.findall(txt))
    if not names:
        print("  (none seen — check that RUST_LOG reached busd; the line is a warn!)")
    for name, n in sorted(names.items(), key=lambda kv: (-kv[1], kv[0])):
        print(f"  {n:3d}  {name}")
    print(f"  total lines: {sum(names.values())}   distinct names: {len(names)}")

    # The four autostarted components whose binaries carry COSMIC_SINGLE_INSTANCE.
    # These are APP_IDs, not binary names: cosmic-osd's is CosmicOnScreenDisplay
    # and cosmic-app-library's is CosmicAppLibrary.
    expected = [
        "com.system76.CosmicLauncher",
        "com.system76.CosmicOnScreenDisplay",
        "com.system76.CosmicWorkspaces",
        "com.system76.CosmicAppLibrary",
    ]
    print("\n  prediction (the four autostarted single-instance components):")
    for e in expected:
        hit = [k for k in names if k.lower() == e.lower()]
        print(f"    {'HIT ' if hit else 'MISS'} {e}"
              + (f"  (as {hit[0]}, x{names[hit[0]]})" if hit else ""))

    print("\n" + "=" * 72)
    print("FAULTS")
    print("=" * 72)
    x86 = RE_FAULT_X86.findall(txt)
    a64 = RE_FAULT_A64.findall(txt)
    print(f"  x86_64 `user page fault ... task killed` : {len(x86)}")
    if x86:
        by_cr2 = collections.Counter(f"CR2=0x{c}" for _, c, _ in x86)
        for k, v in by_cr2.most_common():
            print(f"      {v:3d}  {k}")
        by_rip = collections.Counter(f"RIP=0x{r}" for r, _, _ in x86)
        for k, v in by_rip.most_common(6):
            print(f"      {v:3d}  {k}")
    print(f"  aarch64 `[EXC] EL0 Fault!`               : {len(a64)}")
    if a64:
        by_far = collections.Counter(f"FAR={f}" for _, _, f in a64)
        for k, v in by_far.most_common():
            print(f"      {v:3d}  {k}")

    print("\n" + "=" * 72)
    print("MEMORY (sysinfo(2), i.e. the buddy allocator's own view)")
    print("=" * 72)
    rows = RE_MEM.findall(txt)
    if not rows:
        print("  (no MEMINFO lines — /bin/meminfo missing from the image?)")
    for label, total, free, used, pct, procs in rows:
        print(f"  {label:12s} total={int(total)/2**20:8.1f} MiB"
              f"  free={int(free)/2**20:8.1f} MiB"
              f"  used={int(used)/2**20:8.1f} MiB  ({pct}%)"
              f"  procs={procs or chr(45)}")
    if rows:
        trough = min(rows, key=lambda r: int(r[2]))
        print(f"  trough: {trough[0]} with {int(trough[2])/2**20:.1f} MiB free")

    # A full task table is the OTHER way an allocation fails here, and the one
    # userspace cannot tell apart from a real OOM: fork/clone return ENOMEM the
    # moment runqueue::MAX_TASKS is reached, with any amount of RAM still free.
    full = re.findall(r"\[SCHED\] task table FULL: (\d+)/(\d+)", txt)
    print(f"  `[SCHED] task table FULL`                : {len(full)}"
          + (f"   at {full[0][0]}/{full[0][1]} tasks" if full else ""))
    oom = len(re.findall(r"Out of memory \(os error 12\)", txt))
    print(f"  userspace `Out of memory (os error 12)`  : {oom}")

    print("\n" + "=" * 72)
    print("PROBE — hand-started second copies, stderr byte counts")
    print("=" * 72)
    for m in re.finditer(r"M17: ---- probe (\w+)\s+(\d+)\s+/data/m17/p-\w+\.log", txt):
        print(f"  {m.group(1):12s} {int(m.group(2)):7d} B")
    for pat, meaning in (
        (r"Another instance is running", "an autostarted copy OWNS the name -> it got through the probe"),
        (r"Failed to activate another instance", "not blocked, but nobody owned the name"),
        (r"Successfully activated another instance", "activation round-tripped"),
    ):
        n = len(re.findall(pat, txt))
        if n:
            print(f"  {n:3d}x  \"{pat}\"  — {meaning}")


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/m17"
    arch = sys.argv[2] if len(sys.argv) > 2 else "x86_64"
    os.makedirs(out, exist_ok=True)
    serial_tee = os.path.join(out, "serial.log")
    if os.path.exists(serial_tee):
        os.unlink(serial_tee)

    if os.environ.get("M17_ANALYSE_ONLY"):
        analyse(serial_tee)
        return

    if os.path.exists(SERIAL_LOG):
        os.unlink(SERIAL_LOG)
    mem = os.environ.get("LEANDROS_QEMU_MEM", "2G")
    print(f"=== boot ({arch}, -m {mem}) ===", flush=True)
    r = d("start", arch, t=300)
    print(r.stdout[-1200:], r.stderr[-800:], flush=True)
    if "QEMU started" not in r.stdout:
        sys.exit("boot failed")
    r = d("login", "root", "root", t=90)
    print(r.stdout[-600:], flush=True)

    ser = Serial(tee=serial_tee)

    print("\n=== POSITIVE CONTROL (must FAIL) ===", flush=True)
    ser.send("nosuchbinary_xyz42")
    m, txt = ser.read_until(
        re.compile(r"(not found|No such file|command not found|cannot)", re.I), 25)
    print(txt.strip()[-300:], flush=True)
    if not m:
        sys.exit(">>> CONTROL FAILED: absence and failure are indistinguishable "
                 "on this console. Aborting.")
    print(">>> CONTROL OK\n", flush=True)

    shots = []

    def shot(label):
        p = os.path.join(out, f"m17-{arch}-{label}.ppm")
        d("screenshot", p, t=60)
        img = readppm(p) if os.path.exists(p) else None
        if img is None:
            print(f"  [shot {label}] NO CAPTURE", flush=True)
            return
        w, h, px = img
        ncol, bg, frac = census_px(px)
        prev = shots[-1] if shots else None
        db = diffbox(prev[3], px, w, h) if prev and len(prev[3]) == len(px) else None
        shots.append((label, w, h, px))
        print(f"  [shot {label}] {w}x{h} colours={ncol} bg=#{bg} "
              f"non-bg={frac:.3f} diff_vs_prev={db}", flush=True)

    # ONE command for the whole session. Serial RX drops characters once a
    # session is live, and a truncated command line has already produced seven
    # EL0 faults that looked exactly like a kernel regression.
    ser.send("brush /bin/m17-census")

    while True:
        m, txt = ser.read_until(MARK, 1200)
        print(txt.strip()[-6000:], flush=True)
        if not m:
            print(">>> TIMEOUT waiting for a MARK; the guest stopped early.", flush=True)
            break
        if m.group(0) == "M17: CAPTURES DONE":
            print("\n>>> guest reports CAPTURES DONE", flush=True)
            break
        name, secs = m.group(1), int(m.group(2))
        print(f"\n===== PHASE {name} ({secs}s) =====", flush=True)
        t0 = time.time()
        for when in ([4, secs * 0.5, secs - 6] if secs >= 20 else [max(1, secs - 3)]):
            left = when - (time.time() - t0)
            if left > 0:
                ser.pump(left)
            shot(f"{name.lower()}-t{int(time.time() - t0)}")
        left = secs - (time.time() - t0)
        if left > 0:
            ser.pump(left)

    print(">>> draining guest dump...", flush=True)
    _, txt = ser.read_until(re.compile(r"M17: DONE"), 1200)
    print(txt, flush=True)
    ser.pump(5)
    print(f">>> serial log: {serial_tee}", flush=True)

    analyse(serial_tee)


if __name__ == "__main__":
    main()
