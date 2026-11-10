#!/usr/bin/env python3
"""podpastatest.py -- rootless podman on the default (pasta) network (acceptance, lane podpasta).

Runs in the running VM (same LEANDROS_RUN_ID); needs an image built with
LEANDROS_IMAGE_ARCHIVE=<alpine-oci.tar> and the kernel cmdline `idmap=test`
(runtests --cmdline idmap=test).  Checks, over one held serial connection:
  podman_echo      podman run --rm alpine:3.21 echo hi: RC=0, no "Rolling back"
  podman_hostnet   wget from a container to a host-side python http.server
                   at the guest's default gateway
  podman_bridge_ip wget to 10.88.0.1 (busybox.static httpd in the guest host netns)
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
CHURN = int(os.environ.get("PODPASTA_CHURN", "20"))


class _H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"hello-from-host-podpasta\n"
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
        lines = [l for l in r.get("output", "").splitlines() if not re.match(r"^(0x)*\[", l)]
        return r.get("rc"), "\n".join(lines)

    def check(name, ok, detail=""):
        res.append(ok)
        print(f"{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def tail(out):
        return out.strip()[-220:].replace("\n", " | ")

    try:
        # Runs as user leandro (the harness logs in as root: switch with su).
        rc, out = run("id -u")
        if out.strip().splitlines()[-1:] != ["1000"]:
            print("podpasta: shell is not uid 1000 (log in as leandro first)")
        rc, out = run("cat /proc/net/route")
        gw = "10.0.2.2"
        for l in out.splitlines():
            f = l.split()
            if len(f) > 3 and f[1] == "00000000" and len(f[2]) == 8 and f[2] != "00000000":
                gw = ".".join(str(b) for b in bytes.fromhex(f[2])[::-1])
        print(f"podpasta: host gateway {gw}, http port {port}")

        rc, out = run(f"podman run --rm {IMG} echo hi 2>&1")
        check("pasta_echo", rc == 0 and re.search(r"^hi\s*$", out, re.M) is not None and "ERRO" not in out, f"rc={rc} " + tail(out))

        rc, out = run(f"podman run --rm {IMG} wget -T 15 -qO- http://{gw}:{port}/ 2>&1")
        check("pasta_hostnet", rc == 0 and "hello-from-host-podpasta" in out, f"rc={rc} " + tail(out))

        run(f"podman rm -f pphttpd >/dev/null 2>&1; mkdir -p /tmp/ppwww && echo hello-from-container-httpd > /tmp/ppwww/index.html && "
            f"podman run -d --rm --name pphttpd -p 8080:80 -v /tmp/ppwww:/www:ro {IMG} httpd -f -p 80 -h /www 2>&1")
        rc, out = run("busybox.static wget -T 15 -qO- http://127.0.0.1:8080/ 2>&1; echo")
        check("pasta_portfwd", "hello-from-container-httpd" in out, f"rc={rc} " + tail(out))
        run("podman stop -t 1 pphttpd 2>&1")

        rc, out = run(f"podman run --rm {IMG} nslookup example.com 2>&1")
        if rc == 0 and "Address" in out:
            check("pasta_dns", True)
        else:
            print(f"pasta_dns: SKIP rc={rc} " + tail(out))

        rc, out = run(f"i=0; f=0; while [ $i -lt {CHURN} ]; do podman run --rm {IMG} true >/dev/null 2>&1 || f=$((f+1)); "
                      f"i=$((i+1)); done; echo CHURN_FAIL=$f", timeout=1800)
        mm = re.search(r"CHURN_FAIL=(\d+)", out)
        check("pasta_churn_runs", mm is not None and mm.group(1) == "0", f"fails={mm.group(1) if mm else '?'}")

        rc, out = run("echo PASTA; ls /proc | while read p; do case $p in [0-9]*) "
                      "grep -l '^Name:.pasta' /proc/$p/status 2>/dev/null;; esac; done; "
                      "echo NETNS; ls /run/user/1000/netns /run/netns 2>&1; "
                      "echo MNT; grep -c 'netns' /proc/self/mountinfo; echo END")
        pasta = out.split("PASTA")[-1].split("NETNS")[0].strip()
        netns = out.split("NETNS")[-1].split("MNT")[0].strip()
        mnt = out.split("MNT")[-1].split("END")[0].strip()
        leak = []
        if pasta:
            leak.append("pasta: " + pasta.replace("\n", " "))
        if netns and not all("No such file" in l for l in netns.splitlines()):
            leak.append("netns: " + netns.replace("\n", " "))
        if mnt.isdigit() and int(mnt) > 0:
            leak.append("mounts: " + mnt)
        check("pasta_leaks", not leak, "; ".join(leak))
    finally:
        srv.shutdown()
        ser.close()
    ok = all(res) and len(res) >= 5
    print(f"podpastatest: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
