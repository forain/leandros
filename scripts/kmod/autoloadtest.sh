#!/usr/bin/env bash
# autoloadtest.sh [arch] [virtio_rng|e1000e|virtio_net|virtio_snd|uvc|of]: host-side test of the modules.alias autoload path.
# Builds with CONFIG_MODULES=y CONFIG_VIRTIO_RNG=m (the driver is then ONLY a module, found via
# modules.alias, no modules.load), boots under QEMU and checks that it autoloads, is listed, feeds
# the CSPRNG, and that modules.blacklist=virtio_rng boots without it. Restores .config afterwards
# (the next build-all then rebuilds the =y kernel). Takes a full build-all: run it by hand or in CI.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
arch="${1:-aarch64}"
mod="${2:-virtio_rng}"
cd "$ROOT"
saved=""
if [[ -f .config ]]; then saved="$(mktemp)"; cp .config "$saved"; fi
restore() { if [[ -n "$saved" ]]; then cp "$saved" .config; rm -f "$saved"; else rm -f .config; fi; }
trap restore EXIT
if [[ "$mod" == "e1000e" ]]; then
    # CONFIG_E1000E=m: the NIC is ONLY a module (alias 8086:10d3 of QEMU's 82574L). x86_64 is the
    # useful arch (the aarch64 =m kernel only has to compile); QEMU gets LEANDROS_NIC=e1000e.
    printf 'CONFIG_MODULES=y\nCONFIG_E1000E=m\n' > .config
    ./scripts/build-all.sh --arch "$arch" > "${TMPDIR:-/tmp}/autoloadtest-build.log" 2>&1 || { tail -n 20 "${TMPDIR:-/tmp}/autoloadtest-build.log"; exit 1; }
    if find "target/kmod/$arch" -name modules.load | grep -q .; then echo "FAIL: modules.load present, alias path not isolated"; exit 1; fi
    RT=.claude/skills/run-leandros/runtests.py
    fail=0
    check() { if grep -aq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3"; fail=1; fi; }
    export LEANDROS_RUN_ID="autoloadtest-$arch" LEANDROS_NIC=e1000e
    python3 "$RT" "$arch" alt1 "cat /proc/modules" "kmodctl list" "/bin/nettest" > /dev/null 2>&1 || true
    L=/tmp/ffsession/tests-alt1/serial-full.log
    check "$L" '^\[kmod\] e1000e loaded id' "e1000e autoloaded"
    check "$L" '^\[kmod e1000e\].*found 8086:10d3' "probe line"
    check "$L" '^\[kmod e1000e\].*interrupt: MSI vector' "MSI"
    check "$L" '^\[NET\] DHCP configured' "DHCP"
    check "$L" '^nettest: .* 0 failed' "nettest passes"
    check "$L" '^e1000e [0-9]* [0-9]* - Live' "listed in /proc/modules"
    check "$L" '^e1000e .* Live ' "listed by kmodctl list"
    python3 "$RT" "$arch" alt2 --cmdline "modules.blacklist=e1000e" "ip link" > /dev/null 2>&1 || true
    L=/tmp/ffsession/tests-alt2/serial-full.log
    check "$L" 'alias pci 8086:10d3 e1000e: Blacklisted' "blacklist refuses the alias load"
    if grep -aq 'DHCP configured' "$L"; then echo "FAIL eth up despite blacklist"; fail=1; fi
    [[ $fail -eq 0 ]] && echo "autoloadtest e1000e: PASS" || { echo "autoloadtest e1000e: FAIL"; exit 1; }
    exit 0
fi
if [[ "$mod" == "virtio_snd" ]]; then
    # CONFIG_VIRTIO_SND=m: the sound device is ONLY a module (alias 1af4:1059). No built-in backend
    # exists, so the module's attach registers /dev/pipewire lazily; `sndtest all` plays through it.
    printf 'CONFIG_MODULES=y\nCONFIG_VIRTIO_SND=m\n' > .config
    ./scripts/build-all.sh --arch "$arch" > "${TMPDIR:-/tmp}/autoloadtest-build.log" 2>&1 || { tail -n 20 "${TMPDIR:-/tmp}/autoloadtest-build.log"; exit 1; }
    if find "target/kmod/$arch" -name modules.load | grep -q .; then echo "FAIL: modules.load present, alias path not isolated"; exit 1; fi
    RT=.claude/skills/run-leandros/runtests.py
    fail=0
    check() { if grep -aq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3"; fail=1; fi; }
    export LEANDROS_RUN_ID="autoloadtest-$arch"
    python3 "$RT" "$arch" alt1 "cat /proc/modules" "kmodctl list" "sndtest all" > /dev/null 2>&1 || true
    L=/tmp/ffsession/tests-alt1/serial-full.log
    check "$L" '^\[kmod\] virtio_snd loaded id' "virtio_snd autoloaded"
    check "$L" '^\[kmod virtio_snd\].*virtio-sound attached' "backend attached by the module"
    check "$L" '^virtio_snd [0-9]* [0-9]* - Live' "listed in /proc/modules"
    check "$L" '^virtio_snd .* Live ' "listed by kmodctl list"
    check "$L" 'both channels done: [0-9]* B' "sndtest played through the module"
    python3 "$RT" "$arch" alt2 --cmdline "modules.blacklist=virtio_snd" "cat /proc/modules" > /dev/null 2>&1 || true
    L=/tmp/ffsession/tests-alt2/serial-full.log
    check "$L" 'alias pci 1af4:1059 virtio_snd: Blacklisted' "blacklist refuses the alias load"
    [[ $fail -eq 0 ]] && echo "autoloadtest virtio_snd: PASS" || { echo "autoloadtest virtio_snd: FAIL"; exit 1; }
    exit 0
fi
if [[ "$mod" == "virtio_net" ]]; then
    # CONFIG_VIRTIO_NET=m (WIFI_SIM needs =y so it drops out): the QEMU NIC is ONLY a module
    # (alias 1af4:1041). Both arches boot with the default virtio NIC.
    printf 'CONFIG_MODULES=y\nCONFIG_VIRTIO_NET=m\n' > .config
    ./scripts/build-all.sh --arch "$arch" > "${TMPDIR:-/tmp}/autoloadtest-build.log" 2>&1 || { tail -n 20 "${TMPDIR:-/tmp}/autoloadtest-build.log"; exit 1; }
    if find "target/kmod/$arch" -name modules.load | grep -q .; then echo "FAIL: modules.load present, alias path not isolated"; exit 1; fi
    RT=.claude/skills/run-leandros/runtests.py
    fail=0
    check() { if grep -aq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3"; fail=1; fi; }
    export LEANDROS_RUN_ID="autoloadtest-$arch"
    python3 "$RT" "$arch" alt1 "cat /proc/modules" "kmodctl list" "/bin/nettest" > /dev/null 2>&1 || true
    L=/tmp/ffsession/tests-alt1/serial-full.log
    check "$L" '^\[kmod\] virtio_net loaded id' "virtio_net autoloaded"
    check "$L" '^\[kmod virtio_net\].*registered netif' "netif registered by the module"
    check "$L" '^\[NET\] DHCP configured' "DHCP"
    check "$L" '^nettest: .* 0 failed' "nettest passes"
    check "$L" '^virtio_net [0-9]* [0-9]* - Live' "listed in /proc/modules"
    check "$L" '^virtio_net .* Live ' "listed by kmodctl list"
    python3 "$RT" "$arch" alt2 --cmdline "modules.blacklist=virtio_net" "ip link" > /dev/null 2>&1 || true
    L=/tmp/ffsession/tests-alt2/serial-full.log
    check "$L" 'alias pci 1af4:10\(41\|00\) virtio_net: Blacklisted' "blacklist refuses the alias load"
    if grep -aq 'DHCP configured' "$L"; then echo "FAIL eth up despite blacklist"; fail=1; fi
    [[ $fail -eq 0 ]] && echo "autoloadtest virtio_net: PASS" || { echo "autoloadtest virtio_net: FAIL"; exit 1; }
    exit 0
fi
if [[ "$mod" == "of" ]]; then
    # Platform registry: hello is built as an extra module carrying `of: ["leandros,test"]`;
    # plat.alias_selftest=1 announces that compatible with no built-in driver, so the registry's
    # autoload hook must load hello through the `of` alias row (no modules.load).
    printf 'CONFIG_MODULES=y\n' > .config
    KMOD_EXTRA=hello ./scripts/build-all.sh --arch "$arch" > "${TMPDIR:-/tmp}/autoloadtest-build.log" 2>&1 || { tail -n 20 "${TMPDIR:-/tmp}/autoloadtest-build.log"; exit 1; }
    if find "target/kmod/$arch" -name modules.load | grep -q .; then echo "FAIL: modules.load present, alias path not isolated"; exit 1; fi
    if ! grep -rqa 'of leandros,test hello' "target/kmod/$arch/initrd/lib/modules" 2>/dev/null; then echo "FAIL: no 'of leandros,test hello' alias row"; exit 1; fi
    RT=.claude/skills/run-leandros/runtests.py
    fail=0
    check() { if grep -aq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3"; fail=1; fi; }
    export LEANDROS_RUN_ID="autoloadtest-$arch"
    python3 "$RT" "$arch" alt1 --cmdline "plat.alias_selftest=1" "cat /proc/modules" > /dev/null 2>&1 || true
    L=/tmp/ffsession/tests-alt1/serial-full.log
    check "$L" '^\[plat\] DT compatibles' "DT walker ran"
    check "$L" '^\[kmod\] of leandros,test -> hello' "of alias autoload via the platform registry"
    check "$L" '^\[kmod\] hello loaded id' "hello loaded"
    check "$L" '^hello [0-9]* [0-9]* - Live' "listed in /proc/modules"
    python3 "$RT" "$arch" alt2 --cmdline "plat.alias_selftest=1 modules.blacklist=hello" "cat /proc/modules" > /dev/null 2>&1 || true
    L=/tmp/ffsession/tests-alt2/serial-full.log
    check "$L" 'alias of leandros,test hello: Blacklisted' "blacklist refuses the of load"
    python3 "$RT" "$arch" alt3 --cmdline "kmod=0 plat.alias_selftest=1" "cat /proc/modules" > /dev/null 2>&1 || true
    L=/tmp/ffsession/tests-alt3/serial-full.log
    if grep -aq '^\[kmod\] hello loaded' "$L"; then echo "FAIL kmod=0 still loaded hello"; fail=1; else echo "ok   kmod=0 blocks the of hook"; fi
    [[ $fail -eq 0 ]] && echo "autoloadtest of: PASS" || { echo "autoloadtest of: FAIL"; exit 1; }
    exit 0
fi
if [[ "$mod" == "uvc" ]]; then
    # CONFIG_USB_VIDEO_CLASS=m: the camera driver is ONLY a module. QEMU has no UVC device, so the
    # `usb 0e 02` alias is exercised by usb.alias_selftest=1 (the enumerate hook on a synthetic
    # interface), and the camera path by the fake camera (uvc.fake=1, registered by the module's init).
    printf 'CONFIG_MODULES=y\nCONFIG_USB_VIDEO_CLASS=m\n' > .config
    ./scripts/build-all.sh --arch "$arch" > "${TMPDIR:-/tmp}/autoloadtest-build.log" 2>&1 || { tail -n 20 "${TMPDIR:-/tmp}/autoloadtest-build.log"; exit 1; }
    if find "target/kmod/$arch" -name modules.load | grep -q .; then echo "FAIL: modules.load present, alias path not isolated"; exit 1; fi
    RT=.claude/skills/run-leandros/runtests.py
    fail=0
    check() { if grep -aq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3"; fail=1; fi; }
    export LEANDROS_RUN_ID="autoloadtest-$arch"
    python3 "$RT" "$arch" alt1 --cmdline "usb.alias_selftest=1" "cat /proc/modules" > /dev/null 2>&1 || true
    L=/tmp/ffsession/tests-alt1/serial-full.log
    check "$L" '^\[kmod\] usb 1234:5678 0e0200 -> uvc' "usb alias autoload at enumerate"
    check "$L" '^\[kmod\] uvc loaded id' "uvc loaded"
    check "$L" 'usb alias selftest: class driver wants the interface: Some(14)' "module's class driver registered"
    check "$L" '^uvc [0-9]* [0-9]* - Live' "listed in /proc/modules"
    python3 "$RT" "$arch" alt2 --cmdline "uvc.fake=1 modules.load=uvc" "/bin/camtest --probe" "/bin/camtest 8" > /dev/null 2>&1 || true
    L=/tmp/ffsession/tests-alt2/serial-full.log
    check "$L" 'camtest: probe: camera present, ioctls PASS' "camtest --probe"
    check "$L" 'camtest: SUMMARY PASS frames 8/8' "camtest 8"
    python3 "$RT" "$arch" alt3 --cmdline "usb.alias_selftest=1 modules.blacklist=uvc" "cat /proc/modules" > /dev/null 2>&1 || true
    L=/tmp/ffsession/tests-alt3/serial-full.log
    check "$L" 'alias usb 1234:5678 0e0200 uvc: Blacklisted' "blacklist refuses the alias load"
    if grep -aq '^\[kmod\] uvc loaded' "$L"; then echo "FAIL blacklisted module loaded"; fail=1; fi
    [[ $fail -eq 0 ]] && echo "autoloadtest uvc: PASS" || { echo "autoloadtest uvc: FAIL"; exit 1; }
    exit 0
fi
printf 'CONFIG_MODULES=y\nCONFIG_VIRTIO_RNG=m\n' > .config
./scripts/build-all.sh --arch "$arch" > "${TMPDIR:-/tmp}/autoloadtest-build.log" 2>&1 || { tail -n 20 "${TMPDIR:-/tmp}/autoloadtest-build.log"; exit 1; }
if find "target/kmod/$arch" -name modules.load | grep -q .; then echo "FAIL: modules.load present, alias path not isolated"; exit 1; fi
RT=.claude/skills/run-leandros/runtests.py
fail=0
check() { # check <log> <pattern> <what>
    if grep -aq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3"; fail=1; fi
}
export LEANDROS_RUN_ID="autoloadtest-$arch"
python3 "$RT" "$arch" alt1 "cat /proc/modules" "kmodctl list" "head -c 16 /dev/urandom | wc -c" > /dev/null 2>&1 || true
L=/tmp/ffsession/tests-alt1/serial-full.log
check "$L" '^\[kmod\] virtio_rng loaded id' "virtio_rng autoloaded"
check "$L" '^\[kmod virtio_rng\].*virtio-rng bound' "module bound"
check "$L" '^\[RANDOM\] reseeded (late source) from: virtio-rng' "CSPRNG reseeded from the module"
check "$L" '^virtio_rng [0-9]* 1 - Live' "listed in /proc/modules"
check "$L" '^virtio_rng .* Live ' "listed by kmodctl list"
if grep -aq '^\[RNG\] virtio-rng bound' "$L"; then echo "FAIL built-in driver also bound"; fail=1; fi
python3 "$RT" "$arch" alt2 --cmdline "modules.blacklist=virtio_rng" "kmodctl list" > /dev/null 2>&1 || true
L=/tmp/ffsession/tests-alt2/serial-full.log
check "$L" 'alias pci 1af4:1044 virtio_rng: Blacklisted' "blacklist refuses the alias load"
if grep -aq '^\[kmod\] virtio_rng loaded' "$L"; then echo "FAIL blacklisted module loaded"; fail=1; fi
[[ $fail -eq 0 ]] && echo "autoloadtest: PASS" || { echo "autoloadtest: FAIL"; exit 1; }
