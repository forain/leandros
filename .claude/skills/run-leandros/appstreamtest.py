#!/usr/bin/env python3
"""appstreamtest.py -- libflatpak's appstream refresh in the guest (lane appstream).

Against the ALREADY-RUNNING guest (serial shell, root): `apk add flatpak ostree` (needs network, like
pk-test), build a local archive ostree repo with an `appstream2/<arch>` branch (one component),
`flatpak build-update-repo` it (summary + appstream branch), add it as a --user file:// remote and run
`flatpak --user update --appstream -v`. Pass = rc 0, the `active` symlink and the extracted
appstream.xml.gz exist. No flathub access, so no network flake beyond the apk fetch. The same
path against https://dl.flathub.org (HTTP/2 + ostree pull + 52 MB extraction) was verified by hand.

    LEANDROS_RUN_ID=x LEANDROS_TEST_ARCH=x86_64 python3 appstreamtest.py
"""
import base64
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

GUEST = 'A=$(uname -m)\napk add flatpak ostree > /dev/null 2>&1\nR=/tmp/lrepo\nrm -rf $R $R-src\nmkdir -p $R-src/icons\ncat > $R-src/appstream.xml <<XML\n<?xml version="1.0" encoding="UTF-8"?>\n<components version="0.16" origin="lrepo"><component type="desktop-application"><id>org.example.Local</id><name>Local App</name><summary>local appstream test</summary></component></components>\nXML\nostree init --repo=$R --mode=archive\nostree commit --repo=$R --branch=appstream2/$A --tree=dir=$R-src -s appstream\nflatpak build-update-repo $R\nchmod -R a+rX $R\nsu leandro -c "flatpak --user remote-add --if-not-exists --no-gpg-verify lrepo file://$R; flatpak --user update --appstream -v lrepo; echo UPDATE_RC=\\$?"\nls -l /home/leandro/.local/share/flatpak/appstream/lrepo/$A/active/\ntest -f /home/leandro/.local/share/flatpak/appstream/lrepo/$A/active/appstream.xml.gz && echo APPSTREAM_XML_OK || echo APPSTREAM_XML_MISSING\necho APPSTREAM_DONE\n'


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    ser = driver._Serial()
    b = base64.b64encode(GUEST.encode()).decode()
    try:
        r = driver.serial_run(f"echo {b} | base64 -d > /tmp/appstream-guest.sh; /bin/sh /tmp/appstream-guest.sh 2>&1 | tail -n 30",
                              timeout=600, arch=arch, ser=ser)
    finally:
        ser.close()
    out = r.get("output", "")
    print(out.strip()[-1500:])
    ok = ("UPDATE_RC=0" in out and "APPSTREAM_XML_OK" in out and "APPSTREAM_DONE" in out
          and "pull of ref appstream2/" in out)
    print("appstream", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
