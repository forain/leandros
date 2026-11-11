#!/usr/bin/env python3
"""c127 E2b: 2 s poststop hook; sample conmon/podman liveness every 100 ms."""
import os, re
import driver
arch = os.environ.get("LEANDROS_TEST_ARCH")
ser = driver._Serial()
def run(cmd, timeout=240):
    r = driver.serial_run(cmd, timeout=timeout, arch=arch, ser=ser)
    lines = [l for l in r.get("output", "").splitlines() if not re.match(r"^(0x)*\[", l)]
    return r.get("rc"), "\n".join(lines)
run("podman rm -af >/dev/null 2>&1; podman run --rm alpine:3.21 true >/dev/null 2>&1")
hook = '{"version":"1.0.0","hook":{"path":"/bin/sleep","args":["sleep","2"]},"when":{"always":true},"stages":["poststop"]}'
sc = ("mkdir -p /usr/share/containers/oci/hooks.d; echo '%s' > /usr/share/containers/oci/hooks.d/zz-sleep.json; "
      "for k in 1 2 3; do podman --log-level=debug run --rm alpine:3.21 true >/tmp/it.log 2>&1 & P=$!; n=0; "
      "while kill -0 $P 2>/dev/null; do c=$(ls /proc | while read d; do [ -r /proc/$d/comm ] && grep -l conmon /proc/$d/comm 2>/dev/null; done | wc -l); echo RUN$k t=$n conmon=$c; n=$((n+1)); sleep 0.1; done; "
      "wait $P; echo RUN$k rc=$?; grep -n 'Removing container\\|already removed\\|Got Conmon' /tmp/it.log | cut -c1-120; done; "
      "rm -f /usr/share/containers/oci/hooks.d/zz-sleep.json" % hook)
rc, out = run(sc, timeout=600)
print('\n'.join(l[:190] for l in out.splitlines()))
