#!/usr/bin/env python3
"""udppeertest.py [aarch64|x86_64] [--keep] -- the guest's IPv4/UDP/TCP edges
against a real wire: a host-side Python peer speaking raw Ethernet frames to a
second guest NIC (lane udp2).

Topology: LEANDROS_QEMU_EXTRA adds
    -netdev dgram,id=peer,local.type=inet,local.host=127.0.0.1,local.port=<Q>,
            remote.type=inet,remote.host=127.0.0.1,remote.port=<P>
    -device virtio-net-pci,netdev=peer,mac=02:50:45:45:52:02
QEMU's dgram netdev sends each Ethernet frame as one UDP datagram from
127.0.0.1:Q to 127.0.0.1:P; this script binds P and answers to Q. The guest
names the NIC eth1; the peer (MAC 02:50:45:45:52:01, 10.9.0.1/24) is its DHCP
server (lease 10.9.0.2, no router, no DNS: eth0 keeps the default route).

The peer thread (all frames parsed and built here, stdlib only):
  ARP            reply for 10.9.0.1; learns the guest's MAC/IP
  DHCP (UDP 67)  DISCOVER -> OFFER, REQUEST -> ACK, options 53 54 51 1
  ICMP echo      reply (fragmented replies in reverse order when > 1500)
  IP reassembly  of the guest's fragments; stats: fragmented datagrams, max
                 frame length, 8-byte alignment of non-last fragments, DF
                 clear, overlaps
  UDP :7         echo from port 7; > 1500-byte replies go out as 1480-byte
                 fragments in REVERSE order; a request to 10.9.0.255 or
                 255.255.255.255 is answered to 10.9.0.255 (Ethernet
                 broadcast), dst port = the request's source port
  UDP :9         ICMP 3/3 quoting IP header + min(payload, 528)
  TCP SYN :81/:82/:83  ICMP 3/1, 3/3, 3/13 (IP header + 8 TCP bytes);
                 other ports: RST|ACK
  IGMP           v1/v2/v3 reports and leaves tracked per group
  UDP :7000      control, replies `OK <verb>` to the sender:
                   MCAST <group> <port> <n>   n x `mdns-<i>` from :5353 (only
                                              if the group is joined, else
                                              `NOJOIN <group>`)
                   IGMP <group>               JOINED | LEFT | NONE
                   FRAGS <port>               incomplete / ping-of-death /
                                              complete fragment sets to the guest
                   TOS <port> <tos> <ttl>     3-byte datagram with that TOS/TTL
                   WEAK <ip> <port>           `weak` from :7002 to <ip> (the
                                              guest's OTHER interface address)
                                              through this link (weak host)
                   STATS                      JSON of the stats
Every frame in and out is logged to <outdir>/peer-frames.log.

Then, in the guest: `/bin/udptest peer 10.9.0.1 10.9.0.2` (userland/udptest/
peer.c). Prints that output and the peer stats, then
`udppeertest: PASS|FAIL` (PASS iff the run printed `0 failed` and exited 0).
Exit 0 iff PASS. UDPPEER_ARGS adds guest arguments (e.g. `-x peer_mdns`). Run id `<caller>-udppeer`; outputs under $UDPPEER_OUT
(default /tmp/udppeer-<arch>).
"""
import json
import os
import imgclone
import random
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

PEER_MAC = bytes.fromhex("025045455201")
GUEST_MAC0 = bytes.fromhex("025045455202")
BCAST = b"\xff" * 6
PEER_IP = "10.9.0.1"
GUEST_IP0 = "10.9.0.2"
BCAST_IP = "10.9.0.255"
MTU = 1500


def free_port(kind=socket.SOCK_DGRAM):
    s = socket.socket(socket.AF_INET, kind)
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


# ── packet building ────────────────────────────────────────────────────────

def csum(d):
    if len(d) & 1:
        d += b"\0"
    s = sum(struct.unpack("!%dH" % (len(d) // 2), d))
    while s >> 16:
        s = (s & 0xffff) + (s >> 16)
    return (~s) & 0xffff


def ip_a(s):
    return socket.inet_aton(s)


def mac_s(m):
    return ":".join("%02x" % b for b in m)


def eth(dst, src, et, payload):
    return dst + src + struct.pack("!H", et) + payload


def ip_packet(src, dst, proto, payload, ident, tos=0, ttl=64, mf=False, off=0, df=False):
    ff = (0x4000 if df else 0) | (0x2000 if mf else 0) | (off // 8)
    h = struct.pack("!BBHHHBBH4s4s", 0x45, tos, 20 + len(payload), ident, ff, ttl, proto, 0, ip_a(src), ip_a(dst))
    h = h[:10] + struct.pack("!H", csum(h)) + h[12:]
    return h + payload


def udp_dgram(src, dst, sport, dport, payload):
    ln = 8 + len(payload)
    h = struct.pack("!HHHH", sport, dport, ln, 0)
    c = csum(ip_a(src) + ip_a(dst) + struct.pack("!BBH", 0, 17, ln) + h + payload)
    return struct.pack("!HHHH", sport, dport, ln, c or 0xffff) + payload


def icmp_msg(typ, code, rest4, body):
    h = struct.pack("!BBH", typ, code, 0) + rest4 + body
    return h[:2] + struct.pack("!H", csum(h)) + h[4:]


def parse_ip(pkt):
    """-> dict or None."""
    if len(pkt) < 20 or pkt[0] >> 4 != 4:
        return None
    ihl = (pkt[0] & 15) * 4
    if ihl < 20 or len(pkt) < ihl:
        return None
    tot = struct.unpack("!H", pkt[2:4])[0]
    tot = min(tot, len(pkt)) if tot >= ihl else len(pkt)
    ff = struct.unpack("!H", pkt[6:8])[0]
    return {
        "ihl": ihl, "tos": pkt[1], "tot": tot, "ident": struct.unpack("!H", pkt[4:6])[0],
        "df": bool(ff & 0x4000), "mf": bool(ff & 0x2000), "off": (ff & 0x1fff) * 8,
        "ttl": pkt[8], "proto": pkt[9], "src": socket.inet_ntoa(pkt[12:16]), "dst": socket.inet_ntoa(pkt[16:20]),
        "hdr": pkt[:ihl], "payload": pkt[ihl:tot], "csum_ok": csum(pkt[:ihl]) == 0,
    }


def summarize(f):
    if len(f) < 14:
        return f"runt len={len(f)}"
    dst, src, et = f[0:6], f[6:12], struct.unpack("!H", f[12:14])[0]
    s = f"{mac_s(src)}>{mac_s(dst)} "
    body = f[14:]
    if et == 0x0806 and len(body) >= 28:
        op = struct.unpack("!H", body[6:8])[0]
        return s + f"ARP {'req' if op == 1 else 'rep' if op == 2 else op} {socket.inet_ntoa(body[14:18])} -> {socket.inet_ntoa(body[24:28])}"
    if et != 0x0800:
        return s + f"ethertype {et:#06x} len={len(f)}"
    ip = parse_ip(body)
    if ip is None:
        return s + f"bad IPv4 len={len(f)}"
    s += f"IP {ip['src']}>{ip['dst']} p{ip['proto']} tot={ip['tot']} id={ip['ident']} ttl={ip['ttl']} tos={ip['tos']:#x}"
    if ip["df"]:
        s += " DF"
    if ip["mf"] or ip["off"]:
        s += f" FRAG off={ip['off']} len={len(ip['payload'])}{' MF' if ip['mf'] else ''}"
    p = ip["payload"]
    if ip["off"] == 0:
        if ip["proto"] == 17 and len(p) >= 8:
            sp, dp, ln = struct.unpack("!HHH", p[:6])
            s += f" UDP {sp}>{dp} len={ln}"
        elif ip["proto"] == 6 and len(p) >= 14:
            sp, dp = struct.unpack("!HH", p[:4])
            s += f" TCP {sp}>{dp} flags={p[13]:#04x}"
        elif ip["proto"] == 1 and len(p) >= 2:
            s += f" ICMP {p[0]}/{p[1]}"
        elif ip["proto"] == 2 and len(p) >= 8:
            s += f" IGMP {p[0]:#04x} {socket.inet_ntoa(p[4:8])}"
    return s + f" frame={len(f)}"


# ── the peer ───────────────────────────────────────────────────────────────

class Peer:
    def __init__(self, sock, qemu_port, logpath):
        self.sock = sock
        self.qaddr = ("127.0.0.1", qemu_port)
        self.log = open(logpath, "w", buffering=1)
        self.t0 = time.time()
        self.lock = threading.Lock()
        self.ident = random.Random().randrange(1000, 40000)
        self.guest_mac = GUEST_MAC0
        self.guest_ip = GUEST_IP0
        self.stop = False
        self.dhcp_offer = False
        self.dhcp_ack = False
        self.frames_in = 0
        self.frames_out = 0
        self.max_frame = 0
        self.frag_keys = set()
        self.frag_unaligned = 0
        self.frag_df = 0
        self.frag_overlap = 0
        self.bad_ip_csum = 0
        self.bad_udp_csum = 0
        self.reasm = {}
        self.reasm_done = set()
        self.igmp = {}
        self.thread = None

    # logging / io
    def logline(self, d, msg):
        with self.lock:
            self.log.write(f"{time.time() - self.t0:9.3f} {d:3s} {msg}\n")

    def next_id(self):
        with self.lock:
            self.ident = (self.ident + 1) & 0xffff
            return self.ident

    def send_frame(self, f):
        self.logline("OUT", summarize(f))
        self.frames_out += 1
        try:
            self.sock.sendto(f, self.qaddr)
        except OSError as e:
            self.logline("ERR", f"sendto: {e}")

    def send_ip(self, src, dst, proto, payload, dmac, ident=None, tos=0, ttl=64, reverse=False, gap=0.0):
        """One IPv4 packet; payloads over the MTU go out as 1480-byte fragments."""
        ident = self.next_id() if ident is None else ident
        if 20 + len(payload) <= MTU:
            self.send_frame(eth(dmac, PEER_MAC, 0x0800, ip_packet(src, dst, proto, payload, ident, tos, ttl)))
            return
        frags = [(o, payload[o:o + 1480]) for o in range(0, len(payload), 1480)]
        if reverse:
            frags.reverse()
        for o, c in frags:
            mf = o + len(c) < len(payload)
            self.send_frame(eth(dmac, PEER_MAC, 0x0800, ip_packet(src, dst, proto, c, ident, tos, ttl, mf=mf, off=o)))
            if gap:
                time.sleep(gap)

    def send_udp(self, src, dst, sport, dport, payload, dmac, **kw):
        self.send_ip(src, dst, 17, udp_dgram(src, dst, sport, dport, payload), dmac, **kw)

    def icmp_error(self, typ, code, ip, quote_len, dmac):
        quote = ip["hdr"] + ip["payload"][:quote_len]
        self.send_ip(PEER_IP, ip["src"], 1, icmp_msg(typ, code, b"\0\0\0\0", quote), dmac)

    # receive loop
    def start(self):
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        self.sock.settimeout(0.25)
        while not self.stop:
            try:
                f, _ = self.sock.recvfrom(70000)
            except socket.timeout:
                continue
            except OSError as e:
                if self.stop:
                    break
                self.logline("ERR", f"recv: {e}")
                continue
            try:
                self.on_frame(f)
            except Exception as e:  # noqa: BLE001
                self.logline("ERR", f"on_frame: {type(e).__name__}: {e}")

    def on_frame(self, f):
        self.logline("IN", summarize(f))
        self.frames_in += 1
        if len(f) < 14 or f[6:12] == PEER_MAC:
            return
        self.max_frame = max(self.max_frame, len(f))
        src_mac = f[6:12]
        if src_mac != self.guest_mac:
            self.guest_mac = src_mac
        et = struct.unpack("!H", f[12:14])[0]
        if et == 0x0806:
            self.on_arp(f[14:], src_mac)
        elif et == 0x0800:
            ip = parse_ip(f[14:])
            if ip is None:
                return
            if not ip["csum_ok"]:
                self.bad_ip_csum += 1
                self.logline("WARN", "bad IPv4 header checksum")
            if ip["src"] not in ("0.0.0.0", "255.255.255.255"):
                self.guest_ip = ip["src"]
            if ip["mf"] or ip["off"]:
                self.on_fragment(ip, src_mac)
            else:
                self.on_l4(ip, ip["payload"], src_mac)

    def on_arp(self, b, src_mac):
        if len(b) < 28:
            return
        op = struct.unpack("!H", b[6:8])[0]
        sha, spa, tpa = b[8:14], socket.inet_ntoa(b[14:18]), socket.inet_ntoa(b[24:28])
        if spa not in ("0.0.0.0", PEER_IP):
            self.guest_ip = spa
        if op == 1 and tpa == PEER_IP:
            rep = struct.pack("!HHBBH", 1, 0x0800, 6, 4, 2) + PEER_MAC + ip_a(PEER_IP) + sha + ip_a(spa)
            self.send_frame(eth(sha, PEER_MAC, 0x0806, rep))

    # reassembly of the guest's fragments
    def on_fragment(self, ip, src_mac):
        key = (ip["src"], ip["dst"], ip["proto"], ip["ident"])
        pl = len(ip["payload"])
        if key in self.reasm_done:
            return
        if key not in self.reasm:
            self.reasm[key] = {"parts": [], "total": None, "first": None}
            self.frag_keys.add(key)
        r = self.reasm[key]
        if ip["df"]:
            self.frag_df += 1
        if ip["mf"] and pl % 8:
            self.frag_unaligned += 1
        lo, hi = ip["off"], ip["off"] + pl
        if any(lo < h and l < hi for l, h, _ in r["parts"]):
            self.frag_overlap += 1
            self.logline("WARN", f"overlapping fragment {key} [{lo},{hi})")
            return
        r["parts"].append((lo, hi, ip["payload"]))
        if lo == 0:
            r["first"] = ip
        if not ip["mf"]:
            r["total"] = hi
        if r["total"] is None or r["first"] is None:
            return
        pos = 0
        for l, h, _ in sorted(r["parts"]):
            if l != pos:
                return
            pos = h
        if pos != r["total"]:
            return
        data = b"".join(p for _, _, p in sorted(r["parts"]))
        first = r["first"]
        del self.reasm[key]
        self.reasm_done.add(key)
        self.logline("INFO", f"reassembled {key}: {len(data)} bytes")
        self.on_l4(first, data, src_mac)

    # transport
    def on_l4(self, ip, p, src_mac):
        dst = ip["dst"]
        if ip["proto"] == 1:
            self.on_icmp(ip, p, src_mac)
        elif ip["proto"] == 2:
            self.on_igmp(p)
        elif ip["proto"] == 17 and len(p) >= 8:
            self.on_udp(ip, p, src_mac)
        elif ip["proto"] == 6 and dst == PEER_IP and len(p) >= 20:
            self.on_tcp(ip, p, src_mac)

    def on_icmp(self, ip, p, src_mac):
        if ip["dst"] == PEER_IP and len(p) >= 8 and p[0] == 8:
            self.send_ip(PEER_IP, ip["src"], 1, icmp_msg(0, 0, p[4:8], p[8:]), src_mac)

    def on_igmp(self, p):
        if len(p) < 8:
            return
        t = p[0]
        if t in (0x12, 0x16):
            self.igmp[socket.inet_ntoa(p[4:8])] = "JOINED"
        elif t == 0x17:
            self.igmp[socket.inet_ntoa(p[4:8])] = "LEFT"
        elif t == 0x22:
            n = struct.unpack("!H", p[6:8])[0]
            o = 8
            for _ in range(n):
                if o + 8 > len(p):
                    break
                rt, aux, ns = p[o], p[o + 1], struct.unpack("!H", p[o + 2:o + 4])[0]
                grp = socket.inet_ntoa(p[o + 4:o + 8])
                if rt in (2, 4) or (rt in (1, 3, 5) and ns > 0):
                    self.igmp[grp] = "JOINED"      # EXCLUDE or INCLUDE/ALLOW with sources
                elif rt in (1, 3) and ns == 0:
                    self.igmp[grp] = "LEFT"
                o += 8 + 4 * ns + 4 * aux

    def on_tcp(self, ip, p, src_mac):
        flags = p[13]
        if not (flags & 0x02) or flags & 0x10:
            return
        dport = struct.unpack("!H", p[2:4])[0]
        code = {81: 1, 82: 3, 83: 13}.get(dport)
        if code is not None:
            self.icmp_error(3, code, ip, 8, src_mac)
            return
        sport = struct.unpack("!H", p[0:2])[0]
        seq = struct.unpack("!I", p[4:8])[0]
        h = struct.pack("!HHIIBBHHH", dport, sport, 0, (seq + 1) & 0xffffffff, 5 << 4, 0x14, 0, 0, 0)
        c = csum(ip_a(PEER_IP) + ip_a(ip["src"]) + struct.pack("!BBH", 0, 6, 20) + h)
        h = h[:16] + struct.pack("!H", c) + h[18:]
        self.send_ip(PEER_IP, ip["src"], 6, h, src_mac)

    def on_udp(self, ip, p, src_mac):
        sport, dport, ln = struct.unpack("!HHH", p[:6])
        ucs = struct.unpack("!H", p[6:8])[0]
        if ln < 8 or ln > len(p):
            return
        p = p[:ln]
        if ucs:
            pseudo = ip_a(ip["src"]) + ip_a(ip["dst"]) + struct.pack("!BBH", 0, 17, ln)
            if csum(pseudo + p) != 0:
                self.bad_udp_csum += 1
                self.logline("WARN", "bad UDP checksum")
        data = p[8:]
        dst = ip["dst"]
        if dport == 7 and dst == PEER_IP:
            self.__dict__.setdefault("df_by_sport", {})[sport] = ip["df"]
        bcast = dst in (BCAST_IP, "255.255.255.255")
        if dport == 67 and sport == 68:
            self.on_dhcp(data)
        elif dport == 7 and (dst == PEER_IP or bcast):
            if bcast:
                self.send_udp(PEER_IP, BCAST_IP, 7, sport, data, BCAST, reverse=True)
            else:
                self.send_udp(PEER_IP, ip["src"], 7, sport, data, src_mac, reverse=True)
        elif dport == 9 and dst == PEER_IP:
            self.icmp_error(3, 3, ip, 8 + min(len(data), 528), src_mac)
        elif dport == 7000 and dst == PEER_IP:
            self.on_control(ip, sport, data, src_mac)

    # DHCP
    def on_dhcp(self, d):
        if len(d) < 240 or d[0] != 1 or d[236:240] != b"\x63\x82\x53\x63":
            return
        xid, flags = d[4:8], d[10:12]
        chaddr = d[28:44]
        mt = None
        o = 240
        while o < len(d) and d[o] != 255:
            if d[o] == 0:
                o += 1
                continue
            if o + 1 >= len(d):
                break
            code, ln = d[o], d[o + 1]
            if code == 53 and ln >= 1:
                mt = d[o + 2]
            o += 2 + ln
        if mt not in (1, 3):
            return
        rt = 2 if mt == 1 else 5
        opts = (bytes([53, 1, rt]) + bytes([54, 4]) + ip_a(PEER_IP) + bytes([51, 4]) + struct.pack("!I", 3600)
                + bytes([1, 4]) + ip_a("255.255.255.0") + b"\xff")
        rep = (struct.pack("!BBBB4sHH", 2, 1, 6, 0, xid, 0, struct.unpack("!H", flags)[0])
               + b"\0\0\0\0" + ip_a(GUEST_IP0) + ip_a(PEER_IP) + b"\0\0\0\0" + chaddr + b"\0" * 192
               + b"\x63\x82\x53\x63" + opts)
        if mt == 1:
            self.dhcp_offer = True
        else:
            self.dhcp_ack = True
        self.send_udp(PEER_IP, "255.255.255.255", 67, 68, rep, BCAST)

    # control port
    def on_control(self, ip, sport, data, src_mac):
        words = data.decode(errors="replace").split()
        if not words:
            return

        def reply(msg):
            self.send_udp(PEER_IP, ip["src"], 7000, sport, msg.encode(), src_mac)

        verb = words[0].upper()
        try:
            if verb == "STATS":
                reply(json.dumps(self.stats()))
            elif verb == "IGMP" and len(words) == 2:
                reply(self.igmp.get(words[1], "NONE"))
            elif verb == "MCAST" and len(words) == 4:
                grp, port, n = words[1], int(words[2]), int(words[3])
                if self.igmp.get(grp) != "JOINED":
                    reply(f"NOJOIN {grp}")
                else:
                    reply("OK MCAST")
                    threading.Thread(target=self.do_mcast, args=(grp, port, n), daemon=True).start()
            elif verb == "FRAGS" and len(words) == 2:
                reply("OK FRAGS")
                threading.Thread(target=self.do_frags, args=(int(words[1]),), daemon=True).start()
            elif verb == "WEAK" and len(words) == 3:
                reply("OK WEAK")
                self.send_udp(PEER_IP, words[1], 7002, int(words[2]), b"weak", self.guest_mac)
            elif verb == "DFQ" and len(words) == 2:
                d = self.__dict__.get("df_by_sport", {}).get(int(words[1]))
                reply("NONE" if d is None else f"DF {int(d)}")
            elif verb == "TOS" and len(words) == 4:
                port, tos, ttl = int(words[1]), int(words[2]), int(words[3])
                reply("OK TOS")
                self.send_udp(PEER_IP, self.guest_ip, 7001, port, b"tos", self.guest_mac, tos=tos, ttl=ttl)
            else:
                reply(f"ERR {verb}")
        except ValueError:
            reply(f"ERR {verb}")

    def stats(self):
        return {
            "frag_datagrams": len(self.frag_keys),
            "max_frame": self.max_frame,
            "frag_aligned": self.frag_unaligned == 0,
            "df_clear": self.frag_df == 0,
            "overlap": self.frag_overlap > 0,
            "bad_ip_csum": self.bad_ip_csum,
            "bad_udp_csum": self.bad_udp_csum,
            "frames_in": self.frames_in,
            "frames_out": self.frames_out,
            "dhcp_offer": self.dhcp_offer,
            "dhcp_ack": self.dhcp_ack,
            "igmp": dict(self.igmp),
        }

    def do_mcast(self, grp, port, n):
        g = socket.inet_aton(grp)
        dmac = bytes([0x01, 0x00, 0x5e, g[1] & 0x7f, g[2], g[3]])
        for i in range(n):
            self.send_udp(PEER_IP, grp, 5353, port, f"mdns-{i}".encode(), dmac, ttl=255)
            time.sleep(0.1)

    def do_frags(self, port):
        gip, gmac = self.guest_ip, self.guest_mac

        def frag_set(ident, l4):
            return [(o, l4[o:o + 1480]) for o in range(0, len(l4), 1480)], len(l4)

        def emit(ident, o, c, total, last=None):
            mf = (o + len(c) < total) if last is None else (not last)
            self.send_frame(eth(gmac, PEER_MAC, 0x0800, ip_packet(PEER_IP, gip, 17, c, ident, mf=mf, off=o)))
            time.sleep(0.01)

        # (a) incomplete: 3000 bytes, the middle fragment never sent
        pa = b"incomplete" + bytes(i % 251 for i in range(10, 3000))
        l4 = udp_dgram(PEER_IP, gip, 7001, port, pa)
        fr, total = frag_set(0, l4)
        ida = self.next_id()
        emit(ida, fr[0][0], fr[0][1], total)
        emit(ida, fr[2][0], fr[2][1], total)
        time.sleep(0.05)
        # (b) ping of death: a last fragment at offset 65528 (100 bytes) and a first fragment
        idb = self.next_id()
        emit(idb, 65528, bytes(100), 0, last=True)
        emit(idb, 0, udp_dgram(PEER_IP, gip, 7001, port, bytes(1472))[:1480], 0, last=False)
        time.sleep(0.05)
        # (c) complete, shuffled, one fragment twice
        pc = b"complete" + bytes(i % 251 for i in range(8, 3000))
        l4 = udp_dgram(PEER_IP, gip, 7001, port, pc)
        fr, total = frag_set(0, l4)
        idc = self.next_id()
        order = [0, 1, 2]
        rnd = random.Random(7)
        while order == [0, 1, 2]:
            rnd.shuffle(order)
        order.insert(1, order[0])
        for k in order:
            emit(idc, fr[k][0], fr[k][1], total)


# ── the test ───────────────────────────────────────────────────────────────

def running_arch():
    sys.path.insert(0, HERE)
    import driver  # noqa: E402
    pid = driver._qemu_pid()
    if pid is None:
        return None
    r = subprocess.run(["ps", "-o", "command=", "-p", str(pid)], capture_output=True, text=True)
    for a in ("x86_64", "aarch64"):
        if f"qemu-system-{a}" in r.stdout:
            return a
    return None


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
    detected = running_arch()
    arch = arch or os.environ.get("LEANDROS_TEST_ARCH") or detected or "aarch64"
    if detected is not None:
        print(f"udppeertest: stopping the running VM (run id '{caller}')", flush=True)
        subprocess.run([sys.executable, DRV, "stop"], capture_output=True, timeout=120)

    outdir = os.environ.get("UDPPEER_OUT") or f"/tmp/udppeer-{arch}"
    os.makedirs(outdir, exist_ok=True)
    tmp = tempfile.mkdtemp(prefix="udppeer-")
    img = os.path.join(tmp, f"boot-{arch}.img")
    root = os.path.join(tmp, f"root-{arch}.img")
    for src, dst in ((os.path.join(REPO, f"leandros-limine-{arch}.img"), img),
                     (os.path.join(REPO, f"f2fs-data0-{arch}.img"), root)):
        if not os.path.exists(src):
            print(f"udppeertest: missing image {src}")
            shutil.rmtree(tmp, ignore_errors=True)
            return 1
        if subprocess.run(["cp", "-c", src, dst], capture_output=True).returncode != 0:
            imgclone.clone_file(src, dst)

    # The wire: the peer binds P, QEMU's dgram netdev binds Q.
    qport, pport = free_port(), free_port()
    while pport == qport:
        pport = free_port()
    psock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    psock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 << 20)
    psock.bind(("127.0.0.1", pport))
    peer = Peer(psock, qport, os.path.join(outdir, "peer-frames.log"))
    peer.start()

    virtio_extra = ",disable-legacy=on" if arch == "aarch64" else ""
    mine = (f"-netdev dgram,id=peer,local.type=inet,local.host=127.0.0.1,local.port={qport},"
            f"remote.type=inet,remote.host=127.0.0.1,remote.port={pport} "
            f"-device virtio-net-pci,netdev=peer,mac=02:50:45:45:52:02{virtio_extra}")
    run_id = f"{caller}-udppeer" if caller else "udppeer"
    env = dict(os.environ)
    env.update({
        "LEANDROS_RUN_ID": run_id, "LEANDROS_DISK_IMAGE": img, "LEANDROS_ROOT_IMAGE": root, "LEANDROS_NET": "user",
        "LEANDROS_QEMU_EXTRA": (os.environ.get("LEANDROS_QEMU_EXTRA", "") + " " + mine).strip(),
    })
    os.environ.update({k: env[k] for k in env if k.startswith("LEANDROS_")})
    sys.modules.pop("driver", None)
    sys.path.insert(0, HERE)
    import driver  # noqa: E402  (scoped to the udppeer run id)
    scale = driver.wait_scale(arch, "uefi")

    def sh(c, t=60):
        res = driver.serial_run(c, timeout=t * scale, arch=arch)
        return res.get("rc"), res.get("output", "") or ""

    ok = False
    try:
        t0 = time.time()
        r = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        if r.returncode != 0:
            print(f"udppeertest: boot failed: {r.stdout[-400:]}{r.stderr[-400:]}")
            return 1
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(90 * scale))],
                            env=env, capture_output=True, text=True, timeout=900)
        if lg.returncode != 0:
            print("udppeertest: login failed")
            return 1
        print(f"udppeertest: booted + logged in after {time.time() - t0:.0f} s (arch {arch}, scale {scale}); "
              f"peer {PEER_IP} on 127.0.0.1:{pport} <-> qemu :{qport}", flush=True)

        # Wait for the lease: the peer sent an ACK and the guest shows 10.9.0.2.
        nic, t1, out = None, time.time(), ""
        while time.time() - t1 < 120 * scale:
            _, out = sh("cat /proc/net/nic", 30)
            for m in re.finditer(r"\[(\w+)\]([^\[]*)", out):
                if re.search(r"^address: 10\.9\.0\.2/24", m.group(2), re.M):
                    nic = m.group(1)
            if nic and peer.dhcp_ack:
                break
            nic = None
            time.sleep(2)
        if nic is None:
            print(f"udppeertest: no lease on the peer link after {time.time() - t1:.0f} s "
                  f"(offer {peer.dhcp_offer}, ack {peer.dhcp_ack}); /proc/net/nic:\n{out}")
            return 1
        print(f"udppeertest: {nic} has 10.9.0.2 after {time.time() - t1:.0f} s", flush=True)
        _, rt = sh("cat /proc/net/route", 30)
        print("udppeertest: /proc/net/route\n" + rt.strip(), flush=True)

        # Informational plumbing check (ARP + ICMP echo) when the image has ping.
        rc, pg = sh("command -v ping >/dev/null && ping -c 2 -W 2 10.9.0.1 || echo no-ping", 30)
        print(f"udppeertest: ping 10.9.0.1: {pg.strip()[-200:]}", flush=True)

        res = driver.serial_run(f"/bin/udptest peer {PEER_IP} {GUEST_IP0} " + os.environ.get("UDPPEER_ARGS", ""), timeout=420 * scale, arch=arch)
        out, rc = res.get("output", "") or "", res.get("rc")
        print(out)
        print(f"udppeertest: udptest peer rc={rc} status={res.get('status')} secs={res.get('secs')}", flush=True)
        ok = rc == 0 and re.search(r"udptest peer: \d+ passed, 0 failed", out) is not None
    finally:
        peer.stop = True
        time.sleep(0.4)
        print("udppeertest: peer stats " + json.dumps(peer.stats()), flush=True)
        try:
            shutil.copyfile(driver.SERIAL_FULL_LOG, os.path.join(outdir, "serial.log"))
        except Exception:  # noqa: BLE001
            pass
        print(f"udppeertest: frame log {os.path.join(outdir, 'peer-frames.log')}", flush=True)
        if not keep:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"udppeertest: kept: LEANDROS_RUN_ID={run_id} LEANDROS_DISK_IMAGE={img} LEANDROS_ROOT_IMAGE={root} "
                  f"LEANDROS_QEMU_EXTRA={shlex.quote(mine)}")
    print(f"udppeertest: {'PASS' if ok else 'FAIL'} ({arch})", flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
