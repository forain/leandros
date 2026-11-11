#!/usr/bin/env python3
"""podpausetest.py -- `podman pause` / `podman unpause` really freeze a container (lane nsfollow).

Runs in the running VM (same LEANDROS_RUN_ID); needs an image built with
LEANDROS_IMAGE_ARCHIVE=<alpine-oci.tar>.  A container counts in a bind-mounted
file; paused, the count must stop and `podman ps` report Paused; unpaused, it
must advance again.  Exit status 0 iff every check passed.
"""
import os
import re
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

IMG = "alpine:3.21"


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    ser = driver._Serial()
    res = []

    def run(cmd, timeout=240):
        r = driver.serial_run(cmd, timeout=timeout, arch=arch, ser=ser)
        lines = [l for l in r.get("output", "").splitlines() if not re.match(r"^(0x)*\[", l)]
        return r.get("rc"), "\n".join(lines)

    def check(name, ok, detail=""):
        res.append(ok)
        print(f"{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    def counter():
        # The writer appends, so the file size only grows while it runs.
        rc, out = run("wc -c < /tmp/pz/c 2>&1")
        m = re.findall(r"^\s*(\d+)\s*$", out, re.M)
        return int(m[-1]) if m else -1

    try:
        run("podman rm -f pzt >/dev/null 2>&1; rm -rf /tmp/pz; mkdir -p /tmp/pz")
        rc, out = run(f"podman run -d --name pzt --network=none -v /tmp/pz:/pz {IMG} "
                      "sh -c 'while :; do echo x >> /pz/c; done' 2>&1")
        check("pause_run", rc == 0, f"rc={rc} " + out.strip()[-200:].replace("\n", " | "))
        run("sleep 3")
        c1 = counter()
        run("sleep 2")
        c2 = counter()
        check("pause_counting", c1 > 0 and c2 > c1, f"{c1}->{c2}")
        rc, out = run("podman pause pzt 2>&1")
        check("pause_cmd", rc == 0, f"rc={rc} " + out.strip()[-200:].replace("\n", " | "))
        run("sleep 2")
        a = counter()
        run("sleep 3")
        b = counter()
        check("pause_stopped", a > 0 and a == b, f"{a} vs {b}")
        rc, out = run("podman ps -a --format '{{.Names}} {{.Status}}' 2>&1")
        check("pause_status", "Paused" in out, out.strip()[-120:].replace("\n", " | "))
        rc, out = run("podman unpause pzt 2>&1")
        check("unpause_cmd", rc == 0, f"rc={rc} " + out.strip()[-200:].replace("\n", " | "))
        run("sleep 3")
        d = counter()
        check("unpause_resumed", d > b, f"{b}->{d}")
        run("podman rm -f pzt >/dev/null 2>&1; rm -rf /tmp/pz")
    finally:
        pass
    sys.exit(0 if res and all(res) else 1)


if __name__ == "__main__":
    main()
