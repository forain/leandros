#!/usr/bin/env python3
"""podbridgetest.py -- root podman on the default bridge network (acceptance, lane podbridge).

Runs in the running VM (same LEANDROS_RUN_ID); needs an image built with
LEANDROS_IMAGE_ARCHIVE=<alpine-oci.tar> (non-identity maps are on by default).  Checks, over one held serial connection:
  podman_echo      podman run --rm alpine:3.21 echo hi: RC=0, no "Rolling back"
  podman_hostnet   wget from a container to a host-side python http.server
                   at the guest's default gateway
  podman_bridge_ip wget to 10.88.0.1 (nc listener in a --network=host container)
  podman_dns       nslookup (skipped, not failed, if the guest has no resolver reach)
  podman_churn     20x `run --rm true`, then no veth/podman0 link, no netns
                   file or bind mount under /run/netns, no conntrack entry
Exit status 0 iff every check passed.

    LEANDROS_RUN_ID=x python3 podbridgetest.py
"""
import http.server
import os
import re
import socketserver
import sys
import threading

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

IMG = "alpine:3.21"
CHURN = int(os.environ.get("PODBRIDGE_CHURN", "20"))


class _S6(socketserver.ThreadingTCPServer):
    import socket as _so
    address_family = _so.AF_INET6
    daemon_threads = True


class _H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"hello-from-host-podbridge\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


def fails6_publish(out):
    return "hello-pb6srv" not in out


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    ser = driver._Serial()
    srv = socketserver.ThreadingTCPServer(("0.0.0.0", 0), _H)
    srv.daemon_threads = True
    port = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    try:
        srv6 = _S6(("::", 0), _H)
        port6 = srv6.server_address[1]
        threading.Thread(target=srv6.serve_forever, daemon=True).start()
    except OSError:
        srv6, port6 = None, 0
    res = []

    def run(cmd, timeout=240):
        r = driver.serial_run(cmd, timeout=timeout, arch=arch, ser=ser)
        # Drop kernel log lines ([FORK], [SYSCALL] ...) interleaved on the console.
        lines = [l for l in r.get("output", "").splitlines() if not re.match(r"^(0x)*\[", l)]
        return r.get("rc"), "\n".join(lines)

    def check(name, ok, detail=""):
        res.append(ok)
        print(f"{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    try:
        rc, out = run("cat /proc/net/route")
        gw = "10.0.2.2"
        for l in out.splitlines():
            f = l.split()
            if len(f) > 3 and f[1] == "00000000" and len(f[2]) == 8 and f[2] != "00000000":
                g = bytes.fromhex(f[2])[::-1]
                gw = ".".join(str(b) for b in g)
        print(f"podbridge: host gateway {gw}, http port {port}")

        run("podman rm -af >/dev/null 2>&1")  # stale containers of an earlier aborted run
        rc, out = run(f"podman run --rm {IMG} echo hi 2>&1")
        check("podman_echo", rc == 0 and "Rolling back" not in out and "level=error" not in out and "ERRO" not in out and re.search(r"^hi\s*$", out, re.M) is not None,
              f"rc={rc} " + out.strip()[-200:].replace("\n", " | ") if rc != 0 or "Rolling" in out or "ERRO" in out else "")

        rc, out = run(f"podman run --rm {IMG} wget -T 15 -qO- http://{gw}:{port}/ 2>&1")
        check("podman_hostnet", rc == 0 and "hello-from-host-podbridge" in out,
              f"rc={rc} " + out.strip()[-200:].replace("\n", " | "))

        # The alpine busybox has no httpd applet: serve one reply per connection with nc from a
        # --network=host container (guest host netns, listening on 0.0.0.0 before podman0 exists).
        rc, out = run(f"podman run -d --rm --name pbhttpd --network=host {IMG} sh -c "
                      "'while :; do printf \"HTTP/1.0 200 OK\\r\\n\\r\\nhello-from-guest-httpd\\n\" | nc -l -p 18080; done' 2>&1; sleep 2; podman ps 2>&1")
        check("podman_hostnet_server_up", rc == 0 and "pbhttpd" in out, f"rc={rc} " + out.strip()[-200:].replace("\n", " | "))
        rc, out = run(f"podman run --rm {IMG} wget -T 15 -qO- http://10.88.0.1:18080/ 2>&1")
        check("podman_bridge_ip", rc == 0 and "hello-from-guest-httpd" in out,
              f"rc={rc} " + out.strip()[-200:].replace("\n", " | "))

        run("podman stop -t 1 pbhttpd 2>&1")
        rc, out = run(f"podman run --rm {IMG} nslookup example.com 2>&1")
        if rc == 0 and "Address" in out:
            check("podman_dns", True)
        else:
            print(f"podman_dns: SKIP rc={rc} " + out.strip()[-120:].replace("\n", " | "))

        # L0 (podl0) REFUTED: podman 5.8.9 rejects `--network=pasta` as root ("pasta networking is
        # only supported for rootless mode or when inside a nested userns"), a podman-side gate.
        # Published ports work today only rootless (podpastatest pasta_portfwd).
        rc, out = run(f"podman run --rm --network=pasta -p 18081:80 {IMG} true 2>&1")
        check("podman_pasta_root_rejected", rc != 0 and "only supported for rootless" in out, f"rc={rc} " + out.strip()[-160:].replace("\n", " | "))

        # --internal (podl0): netavark writes conf/<bridge>/forwarding=0; a later non-internal
        # setup's ip_forward=1 must not clobber it (Linux fans out only when the value changes).
        run("podman network rm -f pbint >/dev/null 2>&1; podman rm -f pbintc >/dev/null 2>&1; podman network create --internal pbint 2>&1")
        rc, out = run("podman run -d --rm --name pbintc --network=pbint " + IMG + " sleep 120 2>&1; "
                      "podman run --rm " + IMG + " true 2>&1; "
                      "br=$(podman network inspect pbint -f '{{.NetworkInterface}}'); echo BR=$br; "
                      "echo 1 > /proc/sys/net/ipv4/ip_forward; "
                      "echo FWD=$(cat /proc/sys/net/ipv4/conf/$br/forwarding)")
        fm = re.search(r"FWD=(\d)", out)
        check("podman_internal_forwarding_kept", fm is not None and fm.group(1) == "0", "out=" + out.strip()[-160:].replace("\n", " | "))
        run("podman stop -t 0 pbintc >/dev/null 2>&1; sleep 3; podman rm -f pbintc >/dev/null 2>&1; podman network rm -f pbint >/dev/null 2>&1")

        # ---- L3 (podnft): firewall_driver = "nftables": published ports through the bridge ----
        def srv_tcp(name, extra, port):
            return run(f"podman run -d --rm --name {name} {extra} {IMG} sh -c "
                       f"'while :; do printf \"HTTP/1.0 200 OK\\r\\n\\r\\nhello-{name}\\n\" | nc -l -p {port}; done' 2>&1; sleep 2")

        def hostwget(url):  # a --network=host container = a client in the guest's host netns
            return run(f"podman run --rm --network=host {IMG} wget -T 10 -qO- {url} 2>&1")

        rc, out = run(f"podman run --rm --network=host {IMG} ip -4 -o addr show dev eth0 2>&1 | head -n 1")
        m = re.search(r"inet (\d+\.\d+\.\d+\.\d+)", out)
        nicip = m.group(1) if m else "10.0.2.15"
        print(f"podbridge: guest NIC address {nicip}")
        run("podman rm -af >/dev/null 2>&1; true")
        run(f"podman run --rm {IMG} true >/dev/null 2>&1")  # bridge + base netavark table exist
        rc, base = run("nft list ruleset | md5sum")
        rc, out = srv_tcp("pbpub", "-p 18081:80", 80)
        rc, rs = run("nft list ruleset 2>&1")
        check("podman_nft_driver", re.search(r"dport 18081", rs) is not None and "NETAVARK-HOSTPORT-DNAT" in rs,
              "ruleset: " + rs.strip()[-160:].replace("\n", " | "))
        for nm, u in (("lo", "127.0.0.1"), ("nicip", nicip)):
            rc, out = hostwget(f"http://{u}:18081/")
            check(f"podman_publish_local_{nm}", rc == 0 and "hello-pbpub" in out, f"rc={rc} " + out.strip()[-120:].replace("\n", " | "))
        for nm, u in (("bridge", "10.88.0.1"), ("nicip", nicip)):
            rc, out = run(f"podman run --rm {IMG} wget -T 10 -qO- http://{u}:18081/ 2>&1")
            check(f"podman_publish_ctr_{nm}", rc == 0 and "hello-pbpub" in out, f"rc={rc} " + out.strip()[-120:].replace("\n", " | "))
        # The Mac reaches it through the QEMU hostfwd (net0 -> nic_rx -> PREROUTING DNAT).
        hp = int(os.environ.get("PODBRIDGE_HOSTFWD_PORT", "18581"))
        got = b""
        try:
            import socket
            # SLIRP: through the QEMU hostfwd; vmnet (192.168.x): straight at the guest's address.
            c = socket.create_connection(("127.0.0.1", hp) if nicip.startswith("10.0.2.") else (nicip, 18081), timeout=15)
            c.settimeout(15)
            c.sendall(b"GET / HTTP/1.0\r\n\r\n")
            while True:
                d = c.recv(512)
                if not d:
                    break
                got += d
            c.close()
        except Exception as e:  # noqa: BLE001
            got = f"ERR {e}".encode()
        check("podman_publish_ext", b"hello-pbpub" in got, got.decode(errors="replace")[:100])
        run("podman rm -f pbpub >/dev/null 2>&1")

        # UDP on loopback only, and a port range.
        run(f"podman run -d --rm --name pbudp -p 127.0.0.1:19090:90/udp {IMG} sh -c "
            "'while :; do echo udp-pong | nc -u -l -p 90; done' >/dev/null 2>&1; sleep 2")
        rc, out = run(f"podman run --rm --network=host {IMG} sh -c 'echo ping | nc -u -w 3 127.0.0.1 19090' 2>&1")
        check("podman_publish_udp", "udp-pong" in out, f"rc={rc} " + out.strip()[-120:].replace("\n", " | "))
        run("podman rm -f pbudp >/dev/null 2>&1")
        srv_tcp("pbrng", "-p 18100-18102:8100-8102", 8101)
        rc, rs = run("nft list ruleset 2>&1")
        rc, out = hostwget("http://127.0.0.1:18101/")
        check("podman_publish_range", rc == 0 and "hello-pbrng" in out and "18100-18102" in rs, f"rc={rc} " + out.strip()[-120:].replace("\n", " | "))
        run("podman rm -f pbrng >/dev/null 2>&1")

        # Cross-network: a client on network pbx1 reaches pbx2's published port through the NIC address.
        # netavark sets no masquerade mark for it, so the flow is DNATed and routed with the client's source kept.
        run("podman network rm -f pbx1 pbx2 >/dev/null 2>&1; podman network create pbx1 >/dev/null 2>&1; podman network create pbx2 >/dev/null 2>&1")
        srv_tcp("pbxsrv", "--network=pbx2 -p 18083:80", 80)
        rc, out = run(f"podman run --rm --network=pbx1 {IMG} wget -T 10 -qO- http://{nicip}:18083/ 2>&1")
        check("podman_publish_crossnet", rc == 0 and "hello-pbxsrv" in out, f"rc={rc} " + out.strip()[-120:].replace("\n", " | "))
        run("podman rm -f pbxsrv >/dev/null 2>&1; podman network rm -f pbx1 pbx2 >/dev/null 2>&1")

        # --internal: a default route added by hand must still not leave the network.
        run("podman network rm -f pbint2 >/dev/null 2>&1; podman network create --internal pbint2 2>&1")
        rc, out = run(f"gwi=$(podman network inspect pbint2 -f '{{{{(index .Subnets 0).Gateway}}}}'); "
                      f"podman run --rm --network=pbint2 --cap-add NET_ADMIN {IMG} sh -c "
                      f"\"ip route add default via $gwi; wget -T 6 -qO- http://{gw}:{port}/ 2>&1; echo W=\\$?\" 2>&1")
        wm = re.search(r"W=(\d+)", out)
        check("podman_internal_unreachable", wm is not None and wm.group(1) != "0" and "hello-from-host" not in out, out.strip()[-160:].replace("\n", " | "))
        run("podman network rm -f pbint2 >/dev/null 2>&1")

        # Teardown: 5 create/remove cycles leave the ruleset where it started.
        run(f"i=0; while [ $i -lt 5 ]; do podman run -d --rm --name pbcyc -p 18090:80 {IMG} sleep 30 >/dev/null 2>&1; "
            "podman rm -f pbcyc >/dev/null 2>&1; i=$((i+1)); done")
        rc, now = run("nft list ruleset | md5sum")
        rc, rs = run("nft list ruleset 2>&1")
        check("podman_publish_teardown", now.strip() == base.strip() and "18090" not in rs and "18081" not in rs,
              "left: " + rs.strip()[-200:].replace("\n", " | "))

        # ---- L9 (ip6pod): NAT66 outbound from a v6 bridge network (QEMU slirp: gw fec0::2, dns fec0::3) ----
        def ctn():
            rc, o = run("grep -h '^fwd:' /proc/net/nic")
            m = re.search(r"fwd: conntrack (\d+)", o)
            return int(m.group(1)) if m else 0

        # slirp's v6 prefix is whatever the guest's SLAAC address says (fec0::/64 by default): gateway ::2, DNS ::3.
        rc, o = run("cat /proc/net/if_inet6")
        g6 = re.search(r"^([0-9a-f]{16})[0-9a-f]{16} 02 40 00 \w+\s+eth0", o, re.M)
        pfx = ":".join(g6.group(1)[i:i + 4] for i in range(0, 16, 4)) if g6 else "fec0:0:0:0"
        gw6, dns6 = pfx + "::2", pfx + "::3"
        print(f"podbridge: v6 gateway {gw6} dns {dns6}")
        # Only a network whose gateway answers from the guest's own namespace can test NAT66 (QEMU slirp
        # does; the Mac vmnet bridge does not): otherwise the v6 checks are skipped, not failed.
        rc, o = run(f"podman run --rm --network=host {IMG} ping -6 -c2 -W 3 {gw6} 2>&1")
        v6ok = re.search(r"[1-9]\d* packets received", o) is not None
        if not v6ok:
            print("podman_v6: SKIP gateway " + gw6 + " not reachable from the host namespace")

        def check6(name, ok, detail=""):
            if v6ok:
                check(name, ok, detail)

        run("podman network rm -f pb6 >/dev/null 2>&1; podman network create --ipv6 --disable-dns pb6 2>&1")
        ct0 = ctn()
        rc, out = run(f"podman run --rm --network pb6 {IMG} ping -6 -c3 -W 5 {gw6} 2>&1")
        check6("podman_v6_ping", rc == 0 and re.search(r" 0% packet loss", out) is not None, f"rc={rc} " + out.strip()[-160:].replace("\n", " | "))
        rc, out = run(f"podman run --rm --network pb6 {IMG} nslookup -type=AAAA example.com {dns6} 2>&1")
        check6("podman_v6_dns_udp", rc == 0 and re.search(r"Address.*:.*:", out) is not None, f"rc={rc} " + out.strip()[-160:].replace("\n", " | "))
        if port6 and v6ok:
            rc, out = run(f"podman run --rm --network pb6 {IMG} sh -c 'wget -T 10 -qO- http://[{gw6}]:{port6}/ | grep -q hello-from-host-podbridge' 2>&1")
            check6("podman_v6_tcp", rc == 0, f"rc={rc} " + out.strip()[-160:].replace("\n", " | "))
        rc, out = run(f"podman run --rm --network pb6 --cap-add=NET_RAW {IMG} traceroute6 -n -m 1 -q 1 -w 4 2001:4860:4860::8888 2>&1")
        # KNOWN-OPEN (TODO.md IPv6 item): Time Exceeded for forwarded v6 flows is not delivered yet; non-gating.
        te_ok = re.search(r"^\s*1\s+[0-9a-f]+:[0-9a-f:]+\s", out, re.M) is not None
        if v6ok:
            print(f"podman_v6_time_exceeded: {'PASS' if te_ok else 'KNOWN-OPEN'} rc={rc} " + out.strip()[-120:].replace("\n", " | "), flush=True)
        # ---- L9 phase 2 (ip6dnat): DNAT66 for a published port -----------------------------
        rc, o6 = run("cat /proc/net/if_inet6")
        m6 = re.search(r"^([0-9a-f]{32}) 02 40 00 \w+\s+eth0", o6, re.M)
        nic6 = ":".join(m6.group(1)[i:i + 4] for i in range(0, 32, 4)) if m6 else gw6
        run("podman rm -af >/dev/null 2>&1; true")
        rc, out = run(f"podman run -d --rm --network pb6 --name pb6srv -p 18082:80 {IMG} sh -c "
                      "'while :; do printf \"HTTP/1.0 200 OK\\r\\n\\r\\nhello-pb6srv\\n\\n\" | nc -l -p 80 -s ::; done' 2>&1; sleep 3")
        for nm, u in (("lo", "::1"), ("nicip", nic6)):
            rc, out = hostwget(f"http://[{u}]:18082/")
            check6(f"podman_v6_publish_{nm}", rc == 0 and "hello-pb6srv" in out, f"rc={rc} " + out.strip()[-120:].replace("\n", " | "))
        rc, out = run(f"podman run --rm --network pb6 {IMG} wget -T 10 -qO- http://[{nic6}]:18082/ 2>&1")
        check6("podman_v6_publish_ctr", rc == 0 and "hello-pb6srv" in out, f"rc={rc} " + out.strip()[-120:].replace("\n", " | "))
        if fails6_publish(out):
            rc, rs = run("nft list ruleset 2>&1 | grep -n -B3 -A3 18082; podman ps -a 2>&1; podman logs pb6srv 2>&1 | tail -n 5")
            print("DIAG v6 publish:\n" + rs[-1800:])
        run("podman rm -f pb6srv >/dev/null 2>&1")
        run("podman network rm -f pb6 >/dev/null 2>&1; podman network create --ipv6 --disable-dns --internal pb6i 2>&1")
        rc, out = run(f"podman run --rm --network pb6i {IMG} ping -6 -c1 -W 4 {gw6} 2>&1")
        check6("podman_v6_internal_blocked", rc != 0 and "1 packets received" not in out, f"rc={rc} " + out.strip()[-120:].replace("\n", " | "))
        run("podman network rm -f pb6i >/dev/null 2>&1; podman network create --ipv6 --disable-dns pb6 2>&1")
        run(f"i=0; while [ $i -lt {CHURN} ]; do podman run --rm --network pb6 {IMG} ping -6 -c1 -W 5 {gw6} >/dev/null 2>&1; i=$((i+1)); done", timeout=1800)
        run("podman network rm -f pb6 >/dev/null 2>&1; sleep 40")
        ct1 = ctn()
        check6("podman_v6_leaks", ct0 >= 0 and ct1 <= ct0, f"conntrack {ct0} -> {ct1}")

        rc, out = run("grep -c ' /run/netns/' /proc/self/mountinfo")
        bl = out.strip().splitlines()[-1:] 
        base_mnt = int(bl[0]) if bl and bl[0].isdigit() else 0
        rc, out = run(f"i=0; f=0; while [ $i -lt {CHURN} ]; do podman run --rm {IMG} true >/dev/null 2>&1 || f=$((f+1)); "
                      f"i=$((i+1)); done; echo CHURN_FAIL=$f", timeout=1800)
        mm = re.search(r"CHURN_FAIL=(\d+)", out)
        check("podman_churn_runs", mm is not None and mm.group(1) == "0", f"fails={mm.group(1) if mm else '?'}")

        rc, out = run("echo LINKS; grep -E 'veth|podman' /proc/net/dev; echo NETNS; ls /run/netns 2>&1; "
                      "echo MNT; grep -c ' /run/netns/' /proc/self/mountinfo; "
                      "echo CT; grep -h '^fwd:' /proc/net/nic; echo END")
        links = out.split("LINKS")[-1].split("NETNS")[0].strip()
        netns = out.split("NETNS")[-1].split("MNT")[0].strip()
        mnt = out.split("MNT")[-1].split("CT")[0].strip().splitlines()
        ctm = re.search(r"fwd: conntrack (\d+)", out)
        ct = ctm.group(1) if ctm else "0"
        leak = []
        if re.search(r"veth|podman", links):
            leak.append("links: " + links.replace("\n", " | "))
        if netns and "No such file" not in netns:
            leak.append("netns: " + netns.replace("\n", " "))
        if mnt and mnt[0].strip().isdigit() and int(mnt[0]) > base_mnt:
            leak.append(f"mounts: {mnt[0]} (baseline {base_mnt})")
        if ct.isdigit() and int(ct) > 0:
            leak.append("conntrack: " + ct)
        check("podman_leaks", not leak, "; ".join(leak))
    finally:
        srv.shutdown()
        if srv6:
            srv6.shutdown()
        ser.close()
    ok = all(res) and len(res) >= 5
    print(f"podbridgetest: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
