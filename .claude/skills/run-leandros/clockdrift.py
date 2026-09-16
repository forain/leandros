#!/usr/bin/env python3
"""clockdrift.py [seconds] [label] — measure guest clock vs host wall clock.

Sends `date +%s%N; sleep N; date +%s%N` to the guest shell through the serial
socket and timestamps, on the host, the arrival of each 19-digit line. The
guest delta (its own CLOCK_REALTIME, which this kernel derives from the same
monotonic clock as everything else) is compared against the host delta between
the two arrivals. Prompt sync and command echo are excluded from both, so the
number is the drift of the guest clock alone, to roughly a millisecond.

Usage (from the repo root, with the same LEANDROS_RUN_ID as `driver.py start`):
    python3 .claude/skills/run-leandros/clockdrift.py 10 idle

Prints one line:  <label> guest=<s> host=<s> ratio=<guest/host> err=<pct>
Ratio < 1 means the guest clock runs SLOW (its second is longer than a host
second); `sleep N` then takes N/ratio host seconds.
"""
import os
import re
import select
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import driver  # noqa: E402


def main():
    secs = int(sys.argv[1]) if len(sys.argv) > 1 else 10
    label = sys.argv[2] if len(sys.argv) > 2 else "drift"
    s = driver._connect_with_retry(driver.SERIAL_SOCK)
    if s is None:
        sys.exit("ERROR: cannot connect to serial socket")
    s.setblocking(False)
    time.sleep(0.05)
    try:
        while select.select([s], [], [], 0.1)[0]:
            s.recv(4096)
    except Exception:
        pass
    s.setblocking(True)
    # Sync on the prompt exactly as driver._serial_send does.
    s.sendall(b"\r")
    sync = b""
    dl = time.time() + 2.0
    while time.time() < dl:
        if select.select([s], [], [], 0.1)[0]:
            c = s.recv(4096)
            if not c:
                break
            if b"\x1b[6n" in c:
                s.sendall(b"\x1b[24;1R" * c.count(b"\x1b[6n"))
            sync += c
            if b"#" in driver._strip_ansi(sync)[-24:]:
                break
    time.sleep(0.05)
    cmd = f"date +%s%N; sleep {secs}; date +%s%N"
    payload = ("  " + cmd + "\n").encode()
    for i in range(0, len(payload), 8):
        s.sendall(payload[i:i + 8])
        time.sleep(0.02)
    s.setblocking(False)

    buf = b""
    stamps = []   # (host_time, guest_ns)
    echoed = False
    deadline = time.time() + secs * 2 + 30
    while time.time() < deadline and len(stamps) < 2:
        if not select.select([s], [], [], 0.1)[0]:
            continue
        chunk = s.recv(4096)
        if not chunk:
            break
        now = time.time()
        if b"\x1b[6n" in chunk:
            s.setblocking(True)
            s.sendall(b"\x1b[24;1R" * chunk.count(b"\x1b[6n"))
            s.setblocking(False)
        buf += chunk
        text = driver._strip_ansi(buf).decode("utf-8", "replace")
        if not echoed:
            echoed = cmd in text
            if echoed:
                # Only lines after the echo count.
                buf = buf[buf.find(cmd.encode()) + len(cmd):]
            continue
        lines = re.findall(r"(?m)^[ \t]*(\d{10,19})[ \t]*\r?\n", driver._strip_ansi(buf).decode("utf-8", "replace"))
        for ln in lines[len(stamps):]:
            stamps.append((now, int(ln)))
            if os.environ.get("CLOCKDRIFT_DEBUG"):
                print(f"  line {ln} at host {now:.3f}", file=sys.stderr)
    if len(stamps) < 2:
        sys.exit(f"ERROR: only {len(stamps)} timestamp lines seen; buffer: {buf[-300:]!r}")
    (h0, g0), (h1, g1) = stamps
    guest = (g1 - g0) / 1e9
    host = h1 - h0
    ratio = guest / host if host else float("nan")
    print(f"{label} guest={guest:.3f}s host={host:.3f}s ratio={ratio:.4f} err={(ratio - 1) * 100:+.2f}%")


if __name__ == "__main__":
    main()
