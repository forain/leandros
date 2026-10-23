#!/usr/bin/env python3
"""Is the shared socket_vmnet bridge passing frames right now?

socket_vmnet (1.2.2) relays every frame to every connected client with a
blocking writev from one thread. One client that stops reading - a QEMU that
is SIGSTOPped or paused, or a guest whose virtio-net RX ring is full because
it hung, so QEMU stops draining its socket - fills its socket buffer, and
from then on the daemon delivers nothing to ANY VM until that client reads
or disconnects. Every guest booted meanwhile sees zero frames: no DHCP
lease, and netctltest/nettest/udptest/sshsmoke/... fail (openbugs L5.13,
2026-10-23: four regress runs in a row, 18:56-19:37, all with zero RX).

probe(sock) connects as a reader, broadcasts a few UDP datagrams from the
host side of the bridge (192.168.105.1 -> .255), and reports whether any
frame came back. driver.py runs it before choosing vmnet and falls back to
SLIRP when the bridge is wedged, so one stuck VM no longer reds every
concurrent lane.

`python3 vmnethealth.py` prints the verdict; `--selftest` wedges the bridge on
purpose (a client that never reads, plus a broadcast flood), checks that the
probe sees it, disconnects the staller and checks that the probe recovers."""
import os, socket, struct, subprocess, sys, time

DEFAULT_SOCK = "/opt/homebrew/var/run/socket_vmnet"
GATEWAY, BCAST = "192.168.105.1", "192.168.105.255"


def _bcast(n, size=100, gap=0.0):
    u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        u.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        u.bind((GATEWAY, 0))
        for i in range(n):
            u.sendto(b"\0" * size, (BCAST, 9))
            if gap: time.sleep(gap)
            elif i % 100 == 99: time.sleep(0.005)
    finally:
        u.close()


def probe(sock=DEFAULT_SOCK, timeout=1.5):
    """True: frames flow. False: wedged. None: cannot tell (no bridge address
    on this host, socket missing, permission denied) - callers keep vmnet."""
    try:
        r = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        r.settimeout(timeout)
        r.connect(sock)
    except OSError:
        return None
    try:
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            try:
                _bcast(3)
            except OSError:
                return None
            r.settimeout(0.25)
            try:
                d = r.recv(65536)
            except socket.timeout:
                continue
            if d and len(d) >= 4 and struct.unpack(">I", d[:4])[0] > 0:
                return True
        return False
    except OSError:
        return None
    finally:
        r.close()


def stopped_qemus():
    """QEMU processes in state T (stopped): the usual staller."""
    try:
        out = subprocess.run(["ps", "-axo", "pid=,stat=,command="], capture_output=True,
                             text=True, timeout=5).stdout
    except Exception:
        return []
    return [l.strip()[:120] for l in out.splitlines()
            if "qemu-system" in l and l.split(None, 2)[1].startswith("T")]


def selftest(sock):
    if probe(sock) is not True:
        sys.exit(f"selftest: bridge not healthy before the test ({probe(sock)})")
    x = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    x.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
    x.connect(sock)
    try:
        _bcast(3000, size=1200)
        time.sleep(0.5)
        wedged = probe(sock)
    finally:
        x.close()
    time.sleep(0.5)
    healed = probe(sock)
    ok = wedged is False and healed is True
    print(f"vmnet_wedge_detected: {'PASS' if wedged is False else 'FAIL'} probe={wedged}")
    print(f"vmnet_recovers_after_staller_leaves: {'PASS' if healed is True else 'FAIL'} probe={healed}")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    sock = os.environ.get("SOCKET_VMNET", DEFAULT_SOCK)
    if "--selftest" in sys.argv:
        selftest(sock)
    v = probe(sock)
    print({True: "healthy", False: "WEDGED", None: "unknown"}[v], *stopped_qemus(), sep="\n  ")
    sys.exit(0 if v is not False else 1)
