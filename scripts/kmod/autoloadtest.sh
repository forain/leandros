#!/usr/bin/env bash
# autoloadtest.sh [arch] [virtio_rng|e1000e]: host-side test of the modules.alias autoload path.
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
    check "$L" 'alias 8086:10d3 e1000e: Blacklisted' "blacklist refuses the alias load"
    if grep -aq 'DHCP configured' "$L"; then echo "FAIL eth up despite blacklist"; fail=1; fi
    [[ $fail -eq 0 ]] && echo "autoloadtest e1000e: PASS" || { echo "autoloadtest e1000e: FAIL"; exit 1; }
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
check "$L" 'alias 1af4:1044 virtio_rng: Blacklisted' "blacklist refuses the alias load"
if grep -aq '^\[kmod\] virtio_rng loaded' "$L"; then echo "FAIL blacklisted module loaded"; fail=1; fi
[[ $fail -eq 0 ]] && echo "autoloadtest: PASS" || { echo "autoloadtest: FAIL"; exit 1; }
