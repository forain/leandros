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


class _H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"hello-from-host-podbridge\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    ser = driver._Serial()
    srv = socketserver.ThreadingTCPServer(("0.0.0.0", 0), _H)
    srv.daemon_threads = True
    port = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
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
        ser.close()
    ok = all(res) and len(res) >= 5
    print(f"podbridgetest: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
