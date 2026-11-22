#!/usr/bin/env python3
"""flatpaktest.py -- network-free Flatpak install/run/uninstall in the guest (lane flatrun).

Against the ALREADY-RUNNING guest (serial shell, root), using only what the image ships
(/usr/bin/flatpak, /usr/bin/ostree, /usr/bin/bwrap from ports/libflatpak; no apk, no network):
build a local OSTree repo holding a minimal runtime (busybox.static from the image) and an app
whose command prints a marker, add it as a --user file:// remote (no GPG), install both through
flatpak's FlatpakTransaction, RUN the app through flatpak's real bwrap sandbox as an unprivileged
user (user + mount + pid namespaces, pivot_root, /app + /usr from the runtime), check the marker,
then uninstall and check nothing is left.

    LEANDROS_RUN_ID=x LEANDROS_TEST_ARCH=x86_64 python3 flatpaktest.py
"""
import base64
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

GUEST = r'''A=$(uname -m)
T=/tmp/flr; rm -rf $T; mkdir -p $T
RT=$T/rt; mkdir -p $RT/usr/bin $RT/usr/lib $RT/files $RT/var
cp /bin/busybox.static $RT/usr/bin/busybox
for a in sh echo id cat ls uname test; do ln -s busybox $RT/usr/bin/$a; done
cat > $RT/metadata <<M
[Runtime]
name=org.leandros.Platform
runtime=org.leandros.Platform/$A/1
sdk=org.leandros.Platform/$A/1
M
flatpak build-export $T/repo $RT 1 --runtime >/dev/null 2>&1; echo EXPORT_RT_RC=$?
mkdir -p $T/app/files/bin $T/app/files/share $T/app/var
printf '[Application]\nname=org.leandros.Hello\nruntime=org.leandros.Platform/%s/1\nsdk=org.leandros.Platform/%s/1\n' $A $A > $T/app/metadata
printf '#!/usr/bin/sh\necho FLATPAK_RUN_MARKER_OK uid=$(id -u) arch=$(uname -m)\ncat /app/share/marker\nls /usr/bin/busybox\n' > $T/app/files/bin/hello.sh
chmod 755 $T/app/files/bin/hello.sh
echo APPDATA_OK > $T/app/files/share/marker
flatpak build-finish --command=hello.sh $T/app >/dev/null 2>&1; echo FINISH_RC=$?
flatpak build-export $T/repo $T/app >/dev/null 2>&1; echo EXPORT_APP_RC=$?
flatpak build-update-repo $T/repo >/dev/null 2>&1; echo UPDATE_REPO_RC=$?
chmod -R a+rX $T
cat > $T/user.sh <<U
flatpak --user remote-add --if-not-exists --no-gpg-verify lrepo file://$T/repo; echo ADD_RC=\$?
flatpak --user install -y --noninteractive lrepo org.leandros.Platform//1 org.leandros.Hello >/dev/null 2>&1; echo INSTALL_RC=\$?
echo LIST1=\$(flatpak --user list --columns=application | tr '\n' ,)
flatpak --user run org.leandros.Hello; echo RUN_RC=\$?
flatpak --user uninstall -y --noninteractive org.leandros.Hello org.leandros.Platform >/dev/null 2>&1; echo UNINSTALL_RC=\$?
echo LIST2=[\$(flatpak --user list --columns=application | tr '\n' ,)]
U
su leandro -c "sh $T/user.sh" 2>&1
echo FLATPAK_DONE
'''


def main():
    arch = os.environ.get("LEANDROS_TEST_ARCH")
    ser = driver._Serial()
    b = base64.b64encode(GUEST.encode()).decode()
    try:
        r = driver.serial_run(f"echo {b} | base64 -d > /tmp/flatpak-guest.sh; /bin/sh /tmp/flatpak-guest.sh 2>&1 | grep -v '^\\[ *[0-9]' | tail -n 40",
                              timeout=900, arch=arch, ser=ser)
    finally:
        ser.close()
    out = r.get("output", "")
    print(out.strip()[-2500:])
    need = ["EXPORT_RT_RC=0", "EXPORT_APP_RC=0", "ADD_RC=0", "INSTALL_RC=0",
            "LIST1=org.leandros.Hello,org.leandros.Platform,", "FLATPAK_RUN_MARKER_OK uid=1000",
            "APPDATA_OK", "RUN_RC=0", "UNINSTALL_RC=0", "LIST2=[]", "FLATPAK_DONE"]
    missing = [n for n in need if n not in out]
    if missing:
        print("missing:", missing)
    print("flatpak", "FAIL" if missing else "PASS")
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main())
