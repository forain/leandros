#!/usr/bin/env python3
"""podsmoketest.py -- the cheapest root `podman run` (regress gate, lane procbind).

Runs in the running VM (same LEANDROS_RUN_ID).  `podman run --rm alpine:3.21 echo hi`
must print hi with rc=0: it exercises crun's whole container setup, including the
OCI masked-path binds of /dev/null over /proc files.  Skipped (exit 0, "SKIP") when
the image has no alpine archive (build without LEANDROS_IMAGE_ARCHIVE).

    LEANDROS_RUN_ID=x python3 podsmoketest.py
"""
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

IMG = "alpine:3.21"


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    ser = driver._Serial()

    def run(cmd, timeout=240):
        r = driver.serial_run(cmd, timeout=timeout, arch=arch, ser=ser)
        lines = [l for l in r.get("output", "").splitlines() if not re.match(r"^(0x)*\[", l)]
        return r.get("rc"), "\n".join(lines)

    try:
        rc, _ = run(f"podman image exists {IMG}")
        if rc != 0:
            rc, _ = run("podman load -i /alpine-oci*.tar >/dev/null 2>&1 && podman image exists " + IMG)
        if rc != 0:
            print("podsmoke: SKIP no alpine image in this build")
            return 0
        run("podman rm -af >/dev/null 2>&1")
        rc, out = run(f"podman run --rm {IMG} echo hi 2>&1")
        ok = rc == 0 and re.search(r"^hi\s*$", out, re.M) is not None and "ERRO" not in out and "Rolling back" not in out
        print(f"podman_run_smoke: {'PASS' if ok else 'FAIL'} " + ("" if ok else f"rc={rc} " + out.strip()[-300:].replace("\n", " | ")), flush=True)
        return 0 if ok else 1
    finally:
        try:
            ser.close()
        except Exception:
            pass


if __name__ == "__main__":
    sys.exit(main())
