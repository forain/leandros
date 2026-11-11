#!/usr/bin/env python3
"""c127 E1 (per-iteration parent-wins vs cleanup-wins) and E2 (2 s poststop hook)."""
import os, re
import driver
arch = os.environ.get("LEANDROS_TEST_ARCH")
ser = driver._Serial()
N = int(os.environ.get("CHURN_N", "40"))
M = int(os.environ.get("HOOK_N", "8"))
def run(cmd, timeout=240):
    r = driver.serial_run(cmd, timeout=timeout, arch=arch, ser=ser)
    lines = [l for l in r.get("output", "").splitlines() if not re.match(r"^(0x)*\[", l)]
    return r.get("rc"), "\n".join(lines)
run("podman rm -af >/dev/null 2>&1; podman run --rm alpine:3.21 true >/dev/null 2>&1; rm -f /run/libpod/events/events.log")
e1 = ("i=0; f=0; pw=0; cw=0; while [ $i -lt %d ]; do podman --log-level=debug run --rm alpine:3.21 true >/tmp/it.log 2>&1; rc=$?; "
      "a=$(grep -c 'Removing container' /tmp/it.log); b=$(grep -c 'already removed' /tmp/it.log); "
      "[ $rc -ne 0 ] && f=$((f+1)); [ \"$a\" != 0 ] && pw=$((pw+1)); [ \"$b\" != 0 ] && cw=$((cw+1)); "
      "echo IT=$i rc=$rc $(grep -o 'Conmon PID as [0-9]*' /tmp/it.log); if [ $rc -ne 0 -o $i -eq 99 ]; then echo LOG$i; grep -n 'Got Conmon\\|Removing container' /tmp/it.log | cut -c1-110; echo EXITS; ls /run/libpod/exits; echo PS; ps -e 2>&1 | grep -i 'conmon\\|podman' | head -5; echo DMESG; dmesg 2>&1 | grep -i 'conmon' | tail -n 6 | cut -c1-200; echo SYSLOG; ls /var/log 2>&1 | head -5; fi; i=$((i+1)); done; "
      "echo E1 FAIL=$f PARENT_WINS=$pw CLEANUP_WINS=$cw; "
      "echo DIED=$(grep -c '\"Status\":\"died\"' /run/libpod/events/events.log) REMOVE=$(grep -c '\"Status\":\"remove\"' /run/libpod/events/events.log); "
      "cat /proc/rqhold 2>/dev/null | grep -i pexit" % N)
rc, out = run(e1, timeout=3000)
print('\n'.join(l[:190] for l in out.splitlines()))
hook = '{"version":"1.0.0","hook":{"path":"/bin/sleep","args":["sleep","2"]},"when":{"always":true},"stages":["poststop"]}'
e2 = ("mkdir -p /usr/share/containers/oci/hooks.d; echo '%s' > /usr/share/containers/oci/hooks.d/zz-sleep.json; "
      "i=0; while [ $i -lt %d ]; do s=$(date +%%s); podman --log-level=debug run --rm alpine:3.21 true >/tmp/it.log 2>&1; rc=$?; e=$(date +%%s); "
      "echo H=$i rc=$rc secs=$((e-s)); if [ $rc -ne 0 -o $i -eq 99 ]; then echo HLOG$i; grep -n -i 'remov\\|exit\\|clean\\|wait\\|conmon\\|error\\|warn' /tmp/it.log | cut -c1-200 | tail -n 30; fi; i=$((i+1)); done; "
      "rm -f /usr/share/containers/oci/hooks.d/zz-sleep.json" % (hook, M))
rc, out = run(e2, timeout=1200)
print('\n'.join(l[:190] for l in out.splitlines()))
