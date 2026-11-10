#!/usr/bin/env python3
import os, re
import driver
arch = os.environ.get("LEANDROS_TEST_ARCH")
ser = driver._Serial()
def run(cmd, timeout=240):
    r = driver.serial_run(cmd, timeout=timeout, arch=arch, ser=ser)
    return "\n".join(l for l in r.get("output", "").splitlines() if not re.match(r"^(0x)*\[", l))
print(run("grep -E ' / | /var| /run' /proc/mounts; ls -la /var/lib/containers/storage | head -20; podman run --rm alpine:3.21 true; ls -la /var/lib/containers/storage /run/libpod | head -30; grep -c -a 'journal_mode' /usr/bin/podman; grep -a -o 'journal_mode=[A-Za-z]*' /usr/bin/podman | head -3; grep -a -o '_busy_timeout=[0-9]*[^ ]\\{0,40\\}' /usr/bin/podman | head -3"))
