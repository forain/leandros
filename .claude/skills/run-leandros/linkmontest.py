#!/usr/bin/env python3
"""linkmontest.py [aarch64|x86_64] [--keep] -- e1000e link monitor (interrupt-driven) + TX bulk.

Boots its own VM with LEANDROS_NIC=e1000e (QEMU's 82574L) and checks:
  idle_wakes  the link monitor's timer wakes over 20 s idle are <= IDLE_BOUND/s (the old
              monitor polled STATUS every 250 ms: 4/s by construction);
  flap_down / flap_up
              QMP `set_link net0 off|on`: /proc/net/nic shows the new state within LAT_BOUND s
              of the command (up: LAT_UP_BOUND, QEMU's autoneg delay), and the monitor counted an interrupt wake (ICR.LSC) for it;
  tx_bulk     `/bin/netcheck --put <host> PORT 32` (10.0.2.2 or vmnet 192.168.105.1) reaches a host sink: PASS line, the
              host received every byte, Mbit/s reported.
Run id `<caller run id>-linkmon`. Exit 0 iff all PASS.
"""
import os
import re
import socket
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
DRV = os.path.join(HERE, "driver.py")
IDLE_BOUND = 1.0
LAT_BOUND = 1.0
LAT_UP_BOUND = 1.6  # QEMU e1000e completes autonegotiation ~0.5 s after set_link on
TX_MIB = 32


def main(argv):
    arch = "x86_64" if "x86_64" in argv else "aarch64"
    keep = "--keep" in argv
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)
    os.environ["LEANDROS_RUN_ID"] = f"{caller}-linkmon" if caller else "linkmon"
    os.environ["LEANDROS_NIC"] = "e1000e"
    sys.path.insert(0, HERE)
    import driver  # noqa: E402
    scale = driver.wait_scale(arch, "uefi")
    res = {}

    def verdict(name, ok, detail=""):
        res[name] = ok
        print(f"{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def sh(cmd, t=30):
        r = subprocess.run([sys.executable, DRV, "run", cmd, str(int(t * scale))],
                           capture_output=True, text=True, timeout=t * scale + 60)
        return r.stdout

    def linkmon():
        m = re.search(r"linkmon: irq_wakes (\d+) timer_wakes (\d+)", sh("cat /proc/net/nic"))
        return (int(m.group(1)), int(m.group(2))) if m else None

    def uptime():
        m = re.search(r"^(\d+\.\d+)", sh("cat /proc/uptime"), re.M)
        return float(m.group(1)) if m else -1.0

    try:
        subprocess.run([sys.executable, DRV, "start", arch], capture_output=True, text=True, timeout=900)
        subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(120 * scale))],
                       capture_output=True, text=True, timeout=1200)
        t0 = time.time()
        nic = ""
        while time.time() - t0 < 120 * scale:
            nic = sh("cat /proc/net/nic")
            if "dhcp: bound" in nic:
                break
            time.sleep(2)
        if "driver: e1000e" not in nic or "dhcp: bound" not in nic:
            verdict("setup", False, f"no e1000e lease: {nic[-300:]!r}")
            return 1
        time.sleep(3)
        # idle wakes
        a, ta = linkmon(), uptime()
        time.sleep(20)
        b, tb = linkmon(), uptime()
        if a is None or b is None:
            verdict("idle_wakes", False, "no linkmon line in /proc/net/nic")
        else:
            rate = (b[1] - a[1]) / max(tb - ta, 0.1)
            verdict("idle_wakes", rate <= IDLE_BOUND,
                    f"{rate:.2f} timer wakes/s over {tb - ta:.1f} s (irq wakes +{b[0] - a[0]}; old poll = 4/s)")
        # flaps: a foreground guest loop (serial) stamps the uptime at which /proc/net/nic shows
        # the new state; the host sends set_link over the monitor meanwhile.
        for name, cmd, want, bound in (("flap_down", "set_link net0 off", "down", LAT_BOUND),
                                       ("flap_up", "set_link net0 on", "up", LAT_UP_BOUND)):
            w0 = linkmon()
            tsend0 = uptime()
            h0 = time.time()
            out = {}

            def watch():
                out["o"] = sh(f"while :; do case \"$(cat /proc/net/nic)\" in *'link: {want}'*) break;; esac; done; "
                              f"echo LMSEEN $(cat /proc/uptime)", 20)
            th = threading.Thread(target=watch)
            th.start()
            time.sleep(3)
            tsend = tsend0 + (time.time() - h0)
            driver._monitor_send(cmd)
            th.join(30)
            m = re.search(r"LMSEEN (\d+\.\d+)", out.get("o", ""))
            w1 = linkmon()
            lat = float(m.group(1)) - tsend if m else -1
            irq = (w1[0] - w0[0]) if (w0 and w1) else 0
            verdict(name, m is not None and lat <= bound and irq >= 1,
                    f"seen {lat:.2f} s after the command (+- serial latency), irq wakes +{irq}")
        t0 = time.time()
        while time.time() - t0 < 60 * scale:
            nic = sh("cat /proc/net/nic")
            if "dhcp: bound" in nic and "link: up" in nic:
                break
            time.sleep(2)
        time.sleep(3)
        # TX bulk to a host sink
        srv = socket.socket()
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("0.0.0.0", 0))
        srv.listen(1)
        port = srv.getsockname()[1]
        got = [0]

        def sink():
            c, _ = srv.accept()
            while True:
                d = c.recv(1 << 16)
                if not d:
                    break
                got[0] += len(d)
            c.close()
        th = threading.Thread(target=sink, daemon=True)
        th.start()
        host = "10.0.2.2" if "10.0.2." in sh("cat /proc/net/nic") else "192.168.105.1"
        out = sh(f"/bin/netcheck --put {host} {port} {TX_MIB}", 120)
        th.join(10)
        line = next((l for l in out.splitlines() if l.startswith("NETCHECK put:")), out[-200:])
        verdict("tx_bulk", "PASS" in line and got[0] == TX_MIB << 20, f"{line.strip()} | host got {got[0]} B")
        if not res["tx_bulk"]:
            print("nic:", " | ".join(l for l in sh("cat /proc/net/nic").splitlines() if re.match(r"(link|carrier|dhcp|address|irq|linkmon)", l)))
    finally:
        if not keep:
            subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)
    ok = res and all(res.values())
    print(f"linkmontest: {'PASS' if ok else 'FAIL'}", flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
