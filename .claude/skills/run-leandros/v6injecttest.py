#!/usr/bin/env python3
"""v6injecttest.py [aarch64|x86_64] [--keep] -- IPv6 router advertisements and DAD claims injected
by a host-side peer onto a real wire (lane v6inject).

Topology (as udppeertest.py): LEANDROS_QEMU_EXTRA adds a second virtio-net (the guest's eth1,
MAC 02:50:45:45:53:02) on a `-netdev dgram` UDP wire to this script, which hand-builds Ethernet/
IPv6/ICMPv6 frames (stdlib only, no root, no host network change). The peer is
fe80::50:45ff:fe45:5301, MAC 02:50:45:45:53:01. Phases, checked from the guest:
  ra_mtu            an RA (router lifetime 0, prefix 2001:db8:91::/64 L+A, MTU option 1400) answers
                    the guest's RS: IPV6_MTU of a UDP socket connected on
                    that prefix = 1400; a later RA with MTU 1300 lowers it, MTU 1000 (< 1280) and
                    1600 (> device MTU) are ignored
  dad_na            (on prefix 1, its address claimed from the start) the guest's DAD NS for the SLAAC
                    address is answered with a NA -> flags 0x840 (DADFAILED) in /proc/net/if_inet6, bind()
                    to it is EADDRNOTAVAIL, it stays failed
  dad_ns            prefix 1 withdrawn (valid 0), then prefix 2001:db8:93::/64, the peer instead probing the same target
                    with its own NS from `::`
Prints `v6injecttest: <phase> PASS|FAIL` lines and a final `v6injecttest: PASS|FAIL (<arch>)`. Exit
0 iff all pass. Frame log: $V6INJECT_OUT/peer-frames.log (default /tmp/v6inject-<arch>). Run id
`<caller>-v6inject`.
"""
import imgclone
import os
import re
import shlex
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")

PEER_MAC = bytes.fromhex("025045455301")
GUEST_MAC = bytes.fromhex("025045455302")
ALLNODES = bytes.fromhex("ff020000000000000000000000000001")
UNSPEC = bytes(16)
PEER_LL = bytes.fromhex("fe80000000000000") + bytes.fromhex("005045fffe455301")
IID = bytes.fromhex("005045fffe455302")  # the guest's EUI-64 interface id
PFX = {1: bytes.fromhex("20010db80091"), 2: bytes.fromhex("20010db80092"), 3: bytes.fromhex("20010db80093")}


def pfx_addr(n, host=None):
    return PFX[n] + bytes(2) + (host if host is not None else IID)


def hexs(b):
    return b.hex()


def csum(d):
    if len(d) & 1:
        d += b"\0"
    s = sum(struct.unpack("!%dH" % (len(d) // 2), d))
    while s >> 16:
        s = (s & 0xffff) + (s >> 16)
    return (~s) & 0xffff


def icmp6(src, dst, body):
    ck = csum(src + dst + struct.pack("!I", len(body)) + b"\0\0\0\x3a" + body)
    return body[:2] + struct.pack("!H", ck) + body[4:]


def v6_frame(dmac, src, dst, body, hlim=255):
    ip = struct.pack("!IHBB", 0x60000000, len(body), 58, hlim) + src + dst + icmp6(src, dst, body)
    return dmac + PEER_MAC + b"\x86\xdd" + ip


def mac_for(dst):
    return b"\x33\x33" + dst[12:16] if dst[0] == 0xff else GUEST_MAC


def ra_body(prefixes, mtu=None, life=0, valid=3600):
    b = struct.pack("!BBHBBHII", 134, 0, 0, 64, 0, life, 0, 0)
    b += b"\x01\x01" + PEER_MAC
    if mtu is not None:
        b += struct.pack("!BBHI", 5, 1, 0, mtu)
    for p in prefixes:
        b += struct.pack("!BBBBIII", 3, 4, 64, 0xc0, valid, min(valid, 1800), 0) + PFX[p] + bytes(10)
    return b


def na_body(target, flags):
    return struct.pack("!BBHI", 136, 0, 0, flags << 24) + target + b"\x02\x01" + PEER_MAC


def ns_body(target):
    return struct.pack("!BBHI", 135, 0, 0, 0) + target


class Peer:
    def __init__(self, sock, qport, logpath):
        self.sock, self.qaddr = sock, ("127.0.0.1", qport)
        self.log = open(logpath, "w", buffering=1)
        self.t0 = time.time()
        self.stop = False
        self.rs_seen = 0
        self.dad_ns = []  # targets of the guest's DAD probes
        self.na_claim = {1}  # prefix numbers whose DAD probes get a NA
        self.ns_claim = set()  # ... get a competing NS from ::
        self.boot_prefixes = [1]
        self.boot_mtu = 1400

    def logline(self, d, m):
        self.log.write(f"{time.time() - self.t0:9.3f} {d} {m}\n")

    def send(self, f, what):
        self.logline("OUT", what)
        self.sock.sendto(f, self.qaddr)

    def send_ra(self, prefixes, mtu=None, valid=3600):
        dst = ALLNODES
        self.send(v6_frame(mac_for(dst), PEER_LL, dst, ra_body(prefixes, mtu, valid=valid)), f"RA prefixes {prefixes} mtu {mtu} valid {valid}")

    def run(self):
        threading.Thread(target=self.loop, daemon=True).start()

    def loop(self):
        self.sock.settimeout(0.2)
        while not self.stop:
            try:
                f = self.sock.recv(65536)
            except OSError:
                continue
            if len(f) < 62 or f[12:14] != b"\x86\xdd" or f[20] != 58:
                continue
            src, body = f[22:38], f[54:]
            t = body[0]
            self.logline("IN", f"icmp6 type {t} from {hexs(src)}")
            if t == 133:
                self.rs_seen += 1
                self.send_ra(self.boot_prefixes, self.boot_mtu)
            elif t == 135 and len(body) >= 24 and src == UNSPEC:
                tgt = body[8:24]
                self.dad_ns.append(tgt)
                for n, p in PFX.items():
                    if tgt[:6] == p and tgt[8:] == IID:
                        if n in self.na_claim:
                            self.send(v6_frame(mac_for(ALLNODES), PEER_LL, ALLNODES, na_body(tgt, 0x20)), f"NA claim {hexs(tgt)}")
                        if n in self.ns_claim:
                            snm = bytes.fromhex("ff0200000000000000000001ff") + tgt[13:]
                            self.send(v6_frame(mac_for(snm), UNSPEC, snm, ns_body(tgt)), f"NS claim {hexs(tgt)}")
            elif t == 135 and len(body) >= 24 and body[8:24] in (PEER_LL, pfx_addr(1, bytes.fromhex("0000000000000001"))):
                self.send(v6_frame(GUEST_MAC, body[8:24], src, na_body(body[8:24], 0x60)), "NA reply")


def main(argv):
    arch, keep = None, False
    for a in argv:
        if a in ("aarch64", "x86_64"):
            arch = a
        elif a == "--keep":
            keep = True
        else:
            print(__doc__)
            return 2
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    arch = arch or os.environ.get("LEANDROS_TEST_ARCH") or "x86_64"
    outdir = os.environ.get("V6INJECT_OUT") or f"/tmp/v6inject-{arch}"
    os.makedirs(outdir, exist_ok=True)
    tmp = tempfile.mkdtemp(prefix="v6inject-")
    img, root = os.path.join(tmp, f"boot-{arch}.img"), os.path.join(tmp, f"root-{arch}.img")
    for src, dst in ((os.path.join(REPO, f"leandros-limine-{arch}.img"), img),
                     (os.path.join(REPO, f"f2fs-data0-{arch}.img"), root)):
        if not os.path.exists(src):
            print(f"v6injecttest: missing image {src}")
            shutil.rmtree(tmp, ignore_errors=True)
            return 1
        if subprocess.run([*imgclone.CPC, src, dst], capture_output=True).returncode != 0:
            imgclone.clone_file(src, dst)

    def fp():
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.bind(("127.0.0.1", 0))
        p = s.getsockname()[1]
        s.close()
        return p
    qport, pport = fp(), fp()
    while pport == qport:
        pport = fp()
    psock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    psock.bind(("127.0.0.1", pport))
    peer = Peer(psock, qport, os.path.join(outdir, "peer-frames.log"))
    peer.run()
    virtio_extra = ",disable-legacy=on" if arch == "aarch64" else ""
    mine = (f"-netdev dgram,id=peer,local.type=inet,local.host=127.0.0.1,local.port={qport},"
            f"remote.type=inet,remote.host=127.0.0.1,remote.port={pport} "
            f"-device virtio-net-pci,netdev=peer,mac=02:50:45:45:53:02{virtio_extra}")
    run_id = f"{caller}-v6inject" if caller else "v6inject"
    env = dict(os.environ)
    env.update({"LEANDROS_RUN_ID": run_id, "LEANDROS_DISK_IMAGE": img, "LEANDROS_ROOT_IMAGE": root, "LEANDROS_NET": "user",
                "LEANDROS_QEMU_EXTRA": (os.environ.get("LEANDROS_QEMU_EXTRA", "") + " " + mine).strip()})
    os.environ.update({k: env[k] for k in env if k.startswith("LEANDROS_")})
    sys.modules.pop("driver", None)
    sys.path.insert(0, HERE)
    import driver  # noqa: E402
    scale = driver.wait_scale(arch, "uefi")

    def sh(c, t=60):
        r = driver.serial_run(c, timeout=t * scale, arch=arch)
        return r.get("output", "") or ""

    results = {}

    def check(name, ok, detail=""):
        results[name] = bool(ok)
        print(f"v6injecttest: {name} {'PASS' if ok else 'FAIL'} {detail}", flush=True)

    def inet6(prefix_hex):
        """Flags (int) of the if_inet6 line starting with prefix_hex, None when absent."""
        for l in sh("cat /proc/net/if_inet6").splitlines():
            f = l.split()
            if len(f) == 6 and f[0].startswith(prefix_hex):
                return int(f[4], 16)
        return None

    def wait_addr(n, pred, secs=40):
        h = hexs(pfx_addr(n))
        t0, fl = time.time(), None
        while time.time() - t0 < secs * scale:
            fl = inet6(h)
            if fl is not None and pred(fl):
                return fl
            time.sleep(1)
        return fl

    def mtu(dst="2001:db8:91::1"):
        m = re.search(r"ip6mtu (\d+)", sh(f"/bin/ip6test mtu {dst}"))
        return int(m.group(1)) if m else None

    ok = False
    try:
        t0 = time.time()
        r = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        if r.returncode != 0:
            print(f"v6injecttest: boot failed: {r.stdout[-400:]}{r.stderr[-400:]}")
            return 1
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(90 * scale))], env=env, capture_output=True, text=True, timeout=900)
        if lg.returncode != 0:
            print("v6injecttest: login failed")
            return 1
        print(f"v6injecttest: booted after {time.time() - t0:.0f} s ({arch}, scale {scale}); peer on 127.0.0.1:{pport}", flush=True)

        # The net server keeps ONE SLAAC address per NIC, so the claimed address is replaced by
        # withdrawing its prefix (valid lifetime 0) before the next prefix is advertised.
        def dad_phase(n, name):
            fl = wait_addr(n, lambda f: f & 0x800)
            bind = sh(f"/bin/ip6test bindchk {socket.inet_ntop(socket.AF_INET6, pfx_addr(n))}")
            time.sleep(4 * scale)
            fl2 = inet6(hexs(pfx_addr(n)))
            check(f"{name}_dadfailed", fl is not None and fl & 0x840 == 0x840, f"flags {fl} probes {len(peer.dad_ns)}")
            check(f"{name}_not_usable", "ip6bind EADDRNOTAVAIL" in bind, bind.strip()[-60:])
            check(f"{name}_stays_failed", fl2 is not None and fl2 & 0x800 != 0, f"flags {fl2}")

        # Prefix 1 comes with the MTU option; its address is claimed by a NA (peer.na_claim).
        fl = wait_addr(1, lambda f: f & 0x800)
        check("ra_slaac_addr", fl is not None, f"flags {fl} rs_seen {peer.rs_seen}")
        m = mtu()
        check("ra_mtu_1400", m == 1400, f"ipv6 mtu {m}")
        peer.send_ra([1], 1300)
        time.sleep(2 * scale)
        m = mtu()
        check("ra_mtu_lowered", m == 1300, f"ipv6 mtu {m}")
        peer.send_ra([1], 1000)
        peer.send_ra([1], 1600)
        time.sleep(2 * scale)
        m = mtu()
        check("ra_mtu_out_of_range_ignored", m == 1300, f"ipv6 mtu {m}")
        dad_phase(1, "dad_na")
        peer.send_ra([1], None, valid=0)
        t1 = time.time()
        while time.time() - t1 < 30 * scale and inet6(hexs(pfx_addr(1))) is not None:
            time.sleep(1)
        peer.ns_claim.add(3)
        peer.send_ra([3])
        dad_phase(3, "dad_ns")
        ok = all(results.values()) and len(results) == 10
    finally:
        peer.stop = True
        time.sleep(0.4)
        print(f"v6injecttest: rs_seen {peer.rs_seen}, dad probes {len(peer.dad_ns)}", flush=True)
        try:
            shutil.copyfile(driver.SERIAL_FULL_LOG, os.path.join(outdir, "serial.log"))
        except Exception:  # noqa: BLE001
            pass
        if not keep:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"v6injecttest: kept: LEANDROS_RUN_ID={run_id} LEANDROS_DISK_IMAGE={img} LEANDROS_ROOT_IMAGE={root} LEANDROS_QEMU_EXTRA={shlex.quote(mine)}")
    print(f"v6injecttest: {'PASS' if ok else 'FAIL'} ({arch})", flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
