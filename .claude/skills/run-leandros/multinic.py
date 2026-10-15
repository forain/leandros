#!/usr/bin/env python3
"""multinic.py <arch> [--mode M] [--keep] -- two wired interfaces in one guest.

Boots with LEANDROS_NIC2=1 and LEANDROS_NET=user: eth0 on SLIRP 10.0.2.0/24
(metric 100), eth1 on SLIRP 10.0.3.0/24 (metric 101). On both nets the
gateway (.2) is the host's 127.0.0.1, so a listener here is reachable from
the guest as 10.0.2.2:P (on-link via eth0) and 10.0.3.2:P (on-link via eth1).
The guest side is /bin/netcheck --via HOST PORT [HOLD] (which interface a
new flow takes, and whether a held connection survives). Links are cut with
the QEMU monitor's `set_link net0|net1 off|on`.

Steps (`multinic_<step>: PASS|FAIL <detail>`):
  leases     eth0 and eth1 each hold a DHCP lease (10.0.2.15, 10.0.3.15)
  route      /proc/net/route: both defaults, eth0 first with metric 100
  resolv     /etc/resolv.conf lists eth0's DNS (10.0.2.3) first
  onlink     10.0.3.2 leaves via eth1, 10.0.2.2 via eth0 (longest prefix)
  default    an off-link address (1.1.1.1) leaves via eth0 (lowest metric)
  listen     the guest sshd (INADDR_ANY) answers through eth0 and eth1
  blip       a 1.5 s cut of eth0 keeps an established eth0 connection
  failover   eth0 cut: new flows leave via eth1 at once; after the grace
             period eth0's address and route go and its connection is reset;
             resolv.conf switches to eth1's DNS
  recover    eth0 back: lease again, default route back on eth0
  eth1cut    cutting eth1 leaves the default route on eth0
Exit 0 iff all passed.
"""
import os
import re
import socket
import subprocess
import sys
import threading
import time

os.environ.setdefault("LEANDROS_RUN_ID", "multinic")
os.environ["LEANDROS_NIC2"] = "1"
os.environ["LEANDROS_NET"] = "user"
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "../../.."))
DRV = os.path.join(HERE, "driver.py")
sys.path.insert(0, HERE)
os.chdir(REPO)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


NIC2_SSH = free_port()
os.environ["LEANDROS_NIC2_SSH_PORT"] = str(NIC2_SSH)
import driver  # noqa: E402

RESULTS = []


def step(name, ok, detail=""):
    RESULTS.append((name, ok))
    print(f"multinic_{name}: {'PASS' if ok else 'FAIL'} {detail}", flush=True)


class Listener:
    """Accept and hold every connection (the guest's --via HOLD target)."""

    def __init__(self):
        self.s = socket.socket()
        self.s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.s.bind(("127.0.0.1", 0))
        self.s.listen(16)
        self.port = self.s.getsockname()[1]
        self.conns = []
        threading.Thread(target=self.loop, daemon=True).start()

    def loop(self):
        while True:
            try:
                c, _ = self.s.accept()
            except OSError:
                return
            self.conns.append(c)


def sections(nic):
    out, cur = {}, None
    for line in nic.splitlines():
        m = re.match(r"^\[(\w+)\]$", line.strip())
        if m:
            cur = out.setdefault(m.group(1), {})
            continue
        if cur is not None and ": " in line:
            k, v = line.split(": ", 1)
            cur.setdefault(k.strip(), v.strip())
    return out


def main(argv):
    arch = argv[1] if len(argv) > 1 else "aarch64"
    mode = "uefi"
    keep = "--keep" in argv
    if "--mode" in argv:
        mode = argv[argv.index("--mode") + 1]
    scale = driver.wait_scale(arch, mode)
    lst = Listener()
    print(f"multinic: arch={arch} scale={scale} host listener 127.0.0.1:{lst.port}, eth1 ssh fwd {NIC2_SSH}", flush=True)

    if driver._qemu_pid() is not None:
        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)
    r = subprocess.run([sys.executable, DRV, "start", arch, mode], capture_output=True, text=True, timeout=900)
    if r.returncode != 0:
        print(r.stdout[-2000:], r.stderr[-2000:])
        step("boot", False, "start failed")
        return 1
    lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(60 * scale))],
                        capture_output=True, text=True, timeout=900)
    if lg.returncode != 0:
        step("boot", False, "login failed")
        return 1

    def g(cmd, t=30):
        res = driver.serial_run(cmd, timeout=t * scale, arch=arch)
        return res["output"] or ""

    def mon(c):
        out = driver._monitor_send(c)
        print(f"  monitor: {c} -> {out.strip() or 'ok'}", flush=True)

    def nic():
        return sections(g("cat /proc/net/nic"))

    def wait_for(pred, secs, every=1.0):
        t0 = time.time()
        while time.time() - t0 < secs * scale:
            v = pred()
            if v:
                return v
            time.sleep(every)
        return None

    def via(host, port, hold=0, bg=None):
        if bg:
            g(f"( /bin/netcheck --via {host} {port} {hold} > {bg} 2>&1 & )", 10)
            return None
        out = g(f"/bin/netcheck --via {host} {port}", 20)
        m = re.search(r"from ([0-9.]+) \((\w+|\?)\)", out)
        return (m.group(1), m.group(2), out.strip().splitlines()[-1] if out.strip() else "") if m else (None, None, out.strip())

    try:
        # leases
        def both():
            s = nic()
            ok = all(s.get(i, {}).get("dhcp") == "bound" for i in ("eth0", "eth1"))
            return s if ok else None
        s = wait_for(both, 90, 2)
        if s:
            step("leases", s["eth0"].get("address", "").startswith("10.0.2.") and s["eth1"].get("address", "").startswith("10.0.3."),
                 f"eth0 {s['eth0'].get('address')} eth1 {s['eth1'].get('address')}")
        else:
            print(g("cat /proc/net/nic"))
            step("leases", False, "no lease on both interfaces")

        # route
        rt = g("cat /proc/net/route")
        rows = [l.split() for l in rt.splitlines() if re.match(r"^eth\d\s", l)]
        defs = [(r[0], int(r[6])) for r in rows if len(r) > 6 and r[1] == "00000000"]
        step("route", len(defs) >= 2 and defs[0] == ("eth0", 100) and ("eth1", 101) in defs, f"defaults {defs}")

        # resolv.conf
        rc = g("cat /etc/resolv.conf")
        ns = re.findall(r"nameserver ([0-9.]+)", rc)
        step("resolv", bool(ns) and ns[0] == "10.0.2.3", f"nameservers {ns}")

        # on-link longest prefix
        a = via("10.0.3.2", lst.port)
        b = via("10.0.2.2", lst.port)
        step("onlink", a[1] == "eth1" and b[1] == "eth0", f"10.0.3.2 via {a[1]} ({a[0]}), 10.0.2.2 via {b[1]} ({b[0]})")

        # off-link: lowest metric default
        c = via("1.1.1.1", 80)
        step("default", c[1] == "eth0", f"1.1.1.1 via {c[1]} ({c[0]}): {c[2]}")

        # listener on INADDR_ANY reachable through both interfaces
        def banner(port):
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=10 * scale) as so:
                    so.settimeout(10 * scale)
                    return so.recv(64).decode(errors="replace").strip()
            except OSError as e:
                return f"error {e}"
        st = open(driver.SSH_STATE_FILE).read().split()
        b0 = banner(int(st[1])) if len(st) == 2 else "no eth0 forward"
        b1 = banner(NIC2_SSH)
        step("listen", b0.startswith("SSH-") and b1.startswith("SSH-"), f"eth0 '{b0}' eth1 '{b1}'")

        # blip: established eth0 connection survives a short cut
        via("10.0.2.2", lst.port, int(25 * scale), "/tmp/mn-hold1")
        time.sleep(3 * scale)
        mon("set_link net0 off")
        time.sleep(1.5)
        mon("set_link net0 on")
        wait_for(lambda: nic().get("eth0", {}).get("carrier") == "up", 20)
        time.sleep(26 * scale)
        h1 = g("cat /tmp/mn-hold1")
        step("blip", "ALIVE" in h1 and "PASS" in h1, " | ".join(l for l in h1.splitlines() if "NETCHECK" in l))

        # failover
        via("10.0.2.2", lst.port, int(40 * scale), "/tmp/mn-hold2")
        time.sleep(3 * scale)
        mon("set_link net0 off")
        wait_for(lambda: nic().get("eth0", {}).get("carrier") == "down", 10)
        d = via("1.1.1.1", 80)
        rt2 = g("cat /proc/net/route")
        first_def = next((l.split()[0] for l in rt2.splitlines() if re.match(r"^eth\d\s+00000000", l)), None)
        s2 = wait_for(lambda: (lambda s: s if s.get("eth0", {}).get("ipv4") == "-" else None)(nic()), 20)
        ns2 = re.findall(r"nameserver ([0-9.]+)", g("cat /etc/resolv.conf"))
        wait_for(lambda: "hold:" in g("cat /tmp/mn-hold2"), 50, 2)
        h2 = g("cat /tmp/mn-hold2")
        ok = d[1] == "eth1" and first_def == "eth1" and s2 is not None and ns2[:1] == ["10.0.3.3"] and "RESET" in h2
        step("failover", ok, f"new flow via {d[1]} ({d[0]}); first default {first_def}; eth0 ipv4 "
             f"{(s2 or {}).get('eth0', {}).get('ipv4')}; dns {ns2}; held: "
             + " | ".join(l for l in h2.splitlines() if "NETCHECK" in l))

        # recover
        mon("set_link net0 on")
        s3 = wait_for(lambda: (lambda s: s if s.get("eth0", {}).get("ipv4", "").startswith("10.0.2.") else None)(nic()), 60, 2)
        e = via("1.1.1.1", 80)
        step("recover", s3 is not None and e[1] == "eth0", f"eth0 {(s3 or {}).get('eth0', {}).get('ipv4')}, 1.1.1.1 via {e[1]}")

        # cutting eth1 leaves eth0 the default
        mon("set_link net1 off")
        wait_for(lambda: nic().get("eth1", {}).get("carrier") == "down", 10)
        f = via("1.1.1.1", 80)
        mon("set_link net1 on")
        step("eth1cut", f[1] == "eth0", f"1.1.1.1 via {f[1]} with eth1 down")
    finally:
        try:
            subprocess.run(["cp", driver.SERIAL_FULL_LOG, f"/tmp/multinic-{arch}-serial.log"])
        except Exception:
            pass
        if not keep:
            subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)
    npass = sum(1 for _, ok in RESULTS if ok)
    print(f"multinic summary: {npass}/{len(RESULTS)} passed ({arch})", flush=True)
    return 0 if npass == len(RESULTS) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
