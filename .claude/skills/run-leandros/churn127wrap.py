#!/usr/bin/env python3
# Capture `podman container cleanup` stderr: wrap conmon so --exit-command is the C shim /bin/exitwrap
# (no sh delay), which logs to /tmp/cleanup.log and execs podman.real. Prints 127 count + log excerpts.
import os, re
import driver
arch = os.environ.get("LEANDROS_TEST_ARCH")
ser = driver._Serial()
N = int(os.environ.get("CHURN_N", "40"))
def run(cmd, timeout=240):
    r = driver.serial_run(cmd, timeout=timeout, arch=arch, ser=ser)
    lines = [l for l in r.get("output", "").splitlines() if not re.match(r"^(0x)*\[", l)]
    return r.get("rc"), "\n".join(lines)
W = r'''#!/bin/sh
prev=; n=$#; i=0
while [ $i -lt $n ]; do a="$1"; shift
 if [ "$prev" = "--exit-command" ]; then set -- "$@" /bin/exitwrap; else set -- "$@" "$a"; fi
 prev="$a"; i=$((i+1)); done
exec /usr/bin/conmon.real "$@"
'''
run("[ -e /usr/bin/conmon.real ] || mv /usr/bin/conmon /usr/bin/conmon.real; [ -e /usr/bin/podman.real ] || cp /usr/bin/podman /usr/bin/podman.real")
run("cp /bin/exitwrap /usr/bin/conmon; rm -f /tmp/cleanup.log")
run("podman rm -af >/dev/null 2>&1; podman run --rm alpine:3.21 true >/dev/null 2>&1")
script = ("i=0; f=0; while [ $i -lt %d ]; do : > /tmp/cleanup.log; podman run --rm alpine:3.21 true >/tmp/it.log 2>&1; rc=$?; "
          "if [ $rc -ne 0 ]; then f=$((f+1)); echo ITER=$i RC=$rc; cat /tmp/it.log | tail -n 3; echo CLEANUPLOG; grep -v 'level=debug' /tmp/cleanup.log | tail -n 6; tail -n 5 /tmp/cleanup.log | cut -c1-200; fi; i=$((i+1)); done; echo CHURN_FAIL=$f I=$i" % N)
rc, out = run(script, timeout=3000)
print('\n'.join(l[:200] for l in out.splitlines()[:45]))
