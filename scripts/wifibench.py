#!/usr/bin/env python3
"""wifibench.py [--mb N] [--runs K] [--ping N] [--label TEXT]
-- Wi-Fi throughput and retry rates of a real machine (the laptop) over SSH.

Target: LEANDROS_REMOTE, exactly as for remote.py. Run it from a machine on
a wired link (the Mac on en0), so only the guest's radio link is measured.

Per run it measures:
  down  N MB host -> guest   (ssh `cat > /dev/null`)
  up    N MB guest -> host   (ssh `head -c N /dev/zero`)
and `ping -c P` host -> guest. Around the whole run it snapshots the wifi0
counters of /proc/net/nic and the TX histograms of /proc/net/wifi and prints
the deltas:
  ap_retry   share of the AP's data frames to us with the Retry bit
             (rx_quality data/retry) -- the AP had to resend
  tx_retry   our extra attempts per attempted frame (tx_retries histograms)
  tx_fail    our frames the firmware gave up on (fail_fw / enq)
  rx rates   the AP's rate histogram to us during the run
plus the static association line (`ht:`, rc rate, signal).

Nothing here reads /proc/cmdline. Output is one `wifibench:` summary line
plus details; nothing is written to disk.
"""
import argparse
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import remote  # noqa: E402


def nic_wifi(t):
    txt = t.out("cat /proc/net/nic", timeout=60)
    sec = txt.split("[wifi0]", 1)[1] if "[wifi0]" in txt else ""
    sec = sec.split("\n[", 1)[0]
    d = {}
    for line in sec.splitlines():
        if ":" in line:
            k, v = line.split(":", 1)
            d.setdefault(k.strip(), v.strip())
    return d


def wifi_tx(t):
    txt = t.out("cat /proc/net/wifi", timeout=60)
    d = {}
    for line in txt.splitlines():
        m = re.match(r"tx_retries (ok|fail):\s+(.*)", line)
        if m:
            d["retries_" + m.group(1)] = [int(x) for x in m.group(2).split()]
        m = re.match(r"tx: (.*)", line)
        if m:
            d["tx"] = {k: int(v) for k, v in re.findall(r"(\w+) (\d+)", m.group(1))}
    return d


def kv(s):
    return {k: int(v) for k, v in re.findall(r"([A-Za-z0-9_.]+)[ :](\d+)", s)}


def rates(s):
    m = re.search(r"rate (.*)", s or "")
    return {k: int(v) for k, v in re.findall(r"(\S+?):(\d+)", m.group(1))} if m else {}


def timed(argv, stdin=None):
    t0 = time.time()
    p = subprocess.run(argv, stdin=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=900)
    return time.time() - t0, p


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mb", type=int, default=4)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--ping", type=int, default=50)
    ap.add_argument("--label", default="")
    a = ap.parse_args()
    t = remote.Target(os.environ.get("LEANDROS_REMOTE", ""))
    n = a.mb * 1024 * 1024
    blob = os.path.join(os.environ.get("TMPDIR", "/tmp"), f"wifibench-{os.getpid()}.bin")
    with open(blob, "wb") as f:
        f.write(os.urandom(n))

    n0, w0 = nic_wifi(t), wifi_tx(t)
    downs, ups = [], []
    for _ in range(a.runs):
        with open(blob, "rb") as f:
            dt, p = timed(t.ssh_argv("cat > /dev/null"), stdin=f)
        downs.append(a.mb * 8 / dt if p.returncode == 0 else 0.0)
        dt, p = timed(t.ssh_argv(f"head -c {n} /dev/urandom"))
        ups.append(len(p.stdout) * 8 / 1048576 / dt if p.returncode == 0 and len(p.stdout) == n else 0.0)
    os.unlink(blob)
    ping = subprocess.run(["ping", "-c", str(a.ping), "-i", "0.2", t.host], capture_output=True, text=True)
    pm = re.search(r"([\d.]+)% packet loss", ping.stdout)
    rtt = re.search(r"= ([\d.]+)/([\d.]+)/([\d.]+)", ping.stdout)
    n1, w1 = nic_wifi(t), wifi_tx(t)

    q0, q1 = kv(n0.get("rx_quality", "")), kv(n1.get("rx_quality", ""))
    data = q1.get("data", 0) - q0.get("data", 0)
    retry = q1.get("retry", 0) - q0.get("retry", 0)
    r0, r1 = rates(n0.get("rx_quality")), rates(n1.get("rx_quality"))
    rd = {k: r1[k] - r0.get(k, 0) for k in r1 if r1[k] - r0.get(k, 0) > 0}
    tot = sum(rd.values()) or 1
    top = sorted(rd.items(), key=lambda kv_: -kv_[1])[:5]

    def hist_delta(key):
        h0, h1 = w0.get(key, []), w1.get(key, [])
        d = [b - (h0[i] if i < len(h0) else 0) for i, b in enumerate(h1)]
        return h1 if any(x < 0 for x in d) else d  # counters restart with an association
    ok_h, fail_h = hist_delta("retries_ok"), hist_delta("retries_fail")
    frames = sum(ok_h) + sum(fail_h)
    extra = sum(i * c for i, c in enumerate(ok_h)) + sum(i * c for i, c in enumerate(fail_h))
    tx0, tx1 = w0.get("tx", {}), w1.get("tx", {})
    if tx1.get("enq", 0) < tx0.get("enq", 0):
        tx0 = {}
    enq = tx1.get("enq", 0) - tx0.get("enq", 0)
    failfw = tx1.get("fail_fw", 0) - tx0.get("fail_fw", 0)

    med = lambda v: sorted(v)[len(v) // 2] if v else 0.0  # noqa: E731
    print(f"wifibench{(' ' + a.label) if a.label else ''}: down {med(downs):.2f} Mbit/s "
          f"[{' '.join(f'{x:.2f}' for x in downs)}] up {med(ups):.2f} Mbit/s "
          f"[{' '.join(f'{x:.2f}' for x in ups)}]; ap_retry {100 * retry / max(data, 1):.1f}% "
          f"({retry}/{data}); tx_retry {extra / max(frames, 1):.2f}/frame "
          f"({frames} frames); tx_fail {100 * failfw / max(enq, 1):.2f}%; "
          f"ping loss {pm.group(1) if pm else '?'}% rtt {('/'.join(rtt.groups())) if rtt else '?'} ms")
    print("  rx rates: " + " ".join(f"{k}:{100 * v // tot}%" for k, v in top))
    print("  link: " + n1.get("bssid", "?"))
    print("  ht: " + n1.get("ht", "?"))
    print("  tx_rate: " + n1.get("tx_rate", "?")[:200])
    for k in ("stats", "rx_ba"):
        if k in n1:
            print(f"  {k}: " + n1[k][:240])


if __name__ == "__main__":
    main()
