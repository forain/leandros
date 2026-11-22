#!/usr/bin/env python3
"""conlosstest.py -- userland console writes are lossless (lane conloss).

The guest `/bin/conloss` writes N numbered lines (`CONLOSS <seq> xxxx..`) to
the console in bursts of 40 lines per write(2). A tty writer blocks when the
UART cannot keep up; it never drops. The host checks the sequence for gaps in
two copies of the stream: what the serial socket client received and what
QEMU itself logged (serial-full.log), so a loss between the guest and QEMU
shows in both and a loss between QEMU and the harness only in the first. A
line that is intact but for a spliced-in kernel line counts as lost too (the
kernel's diagnostics are line-atomic against user writes since lane conloss).
Kernel diagnostic lines interleaved in the flood are not checked (an IRQ-context
console write may drop under back-pressure by design).

    LEANDROS_RUN_ID=x LEANDROS_TEST_ARCH=aarch64 python3 conlosstest.py
env: CONLOSS_LINES (default 20000), CONLOSS_SLEEP_MS (default 0)
"""
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

N = int(os.environ.get("CONLOSS_LINES", "20000"))
SLEEP_MS = int(os.environ.get("CONLOSS_SLEEP_MS", "0"))
ROW = re.compile(rb"CONLOSS (\d{6}) x{40}\r?\n")
def gaps(buf: bytes, n: int):
    seen = {int(m.group(1)) for m in ROW.finditer(buf)}
    return sorted(set(range(1, n + 1)) - seen)


def segment(buf: bytes) -> bytes:
    """The flood in `buf`: first CONLOSS line through the DONE line."""
    i = buf.find(b"CONLOSS 000001 ")
    j = buf.find(b"CONLOSS-DONE")
    return buf[i:j] if 0 <= i < j else b""


def context(buf: bytes, gap: int) -> bytes:
    """Raw bytes around the first missing line: from the line before it to the next intact one."""
    i = buf.find(b"CONLOSS %06d " % (gap - 1))
    j = buf.find(b"CONLOSS %06d " % (gap + 12), i if i >= 0 else 0)
    return buf[max(i, 0):j if j > i else (i + 1500)][:2500]


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    ser = driver._Serial()
    fails = []
    try:
        start = len(ser.buf)
        r = driver.serial_run(f"/bin/conloss {N} 40 {SLEEP_MS}", timeout=600, arch=arch, ser=ser)
        ser.pump(2.0)
        sock = ser.buf[start:]
        out = r.get("output", "")
        m = re.search(r"CONLOSS-DONE lines=(\d+) bytes=(\d+) calls=(\d+) short=(\d+) retry=(\d+)", out)
        full = b""
        try:
            with open(driver.SERIAL_FULL_LOG, "rb") as f:
                full = f.read()
        except OSError:
            pass
        gs = gaps(sock, N)
        # QEMU's own logfile can DUPLICATE a byte when the chardev socket takes a
        # partial write (seen on x86_64: +75 bytes in 1.1 MB, socket stream exact),
        # so the log copy is held only to "not shorter than what the socket got";
        # aarch64 logs are byte-exact and also checked for gaps.
        seg_s, seg_f = segment(sock), segment(full)
        gf = gaps(full, N) if arch == "aarch64" else []
        short = max(0, len(seg_s) - len(seg_f))
        # The kernel's own count of console bytes it dropped on the UART.
        d = driver.serial_run("cat /proc/sysstat", timeout=30, arch=arch, ser=ser).get("output", "")
        md = re.search(r"# uartdrop total (\d+) user (\d+)", d)
        user_drops = int(md.group(2)) if md else -1
        ok = (r["status"] == "ok" and m is not None and not gs and not gf and not short and seg_s and user_drops == 0)
        print(f"conloss_lossless: {'PASS' if ok else 'FAIL'} (lines {N}, status {r['status']}, "
              f"{m.group(0) if m else 'no DONE line'}; socket gaps {len(gs)} {gs[:10]}; "
              f"qemu-log gaps {len(gf)} {gf[:10]} (bytes socket {len(seg_s)} log {len(seg_f)}); {md.group(0) if md else 'no # uartdrop line'})")
        if not ok:
            fails.append("lossless")
            for name, g, b in (("qemu-log", gf, full), ("socket", gs, sock)):
                if g:
                    print(f"  {name} context around line {g[0]}: {context(b, g[0])!r}")
    finally:
        ser.close()
    print("conloss", "FAIL" if fails else "PASS")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
