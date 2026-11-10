#!/usr/bin/env python3
import os, re, sys
import driver
arch = os.environ.get("LEANDROS_TEST_ARCH")
ser = driver._Serial()
N = int(os.environ.get("CHURN_N", "40"))
def run(cmd, timeout=240):
    r = driver.serial_run(cmd, timeout=timeout, arch=arch, ser=ser)
    lines = [l for l in r.get("output", "").splitlines() if not re.match(r"^(0x)*\[", l)]
    return r.get("rc"), "\n".join(lines)
run("podman rm -af >/dev/null 2>&1; podman run --rm alpine:3.21 true >/dev/null 2>&1")
script = ("i=0; f=0; while [ $i -lt %d ]; do podman --log-level=debug run --rm alpine:3.21 true >/tmp/it.log 2>&1; rc=$?; "
          "if [ $rc -ne 0 ]; then f=$((f+1)); echo ITER=$i RC=$rc; if [ $f -gt 1 ]; then cp /tmp/it.log /tmp/prev.log; i=$((i+1)); continue; fi; echo PREV; tail -n 14 /tmp/prev.log; echo FAILLOG; grep -ni 'lock' /tmp/it.log | tail -n 12; echo FAILFULL; tail -n 40 /tmp/it.log | cut -c1-170; grep -i 'level=error\\|level=warn\\|exit' /tmp/it.log | tail -n 8; echo EVENTS; ls /run/libpod/events 2>&1; tail -n 7 /run/libpod/events/events.log 2>&1; echo PREVEV; grep -i 'level=error\\|level=warn\\|exit' /tmp/prev.log | tail -n 8; ls /run/libpod/exits /var/lib/containers/storage/vfs-containers 2>&1 | head; fi; cp /tmp/it.log /tmp/prev.log; i=$((i+1)); done; echo CHURN_FAIL=$f I=$i" % N)
rc, out = run(script, timeout=3000)
print('\n'.join(l[:190] for l in out.splitlines()))
run("ls /var/lib/containers/storage/overlay-containers 2>&1 | head -3")
