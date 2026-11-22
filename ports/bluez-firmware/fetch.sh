#!/usr/bin/env bash
. "$(cd "$(dirname "$0")/../.." && pwd)/scripts/lib/netretry.sh"
# Fetch the Bluetooth patch firmware the Raspberry Pi 5's CYW43455 (BCM4345C0)
# needs from RPi-Distro/bluez-firmware into $BTFW_CACHE (default
# ~/.cache/leandros/bluez-firmware). scripts/mkfs-f2fs-populated.py stages it
# as /lib/firmware/brcm/BCM4345C0.hcd on the aarch64 root image, where the
# kernel's Pi 5 Bluetooth bring-up (drivers/src/bt/boot.rs) loads it.
#
# Broadcom/Cypress redistributable firmware (see LICENCE.broadcom_bcm43xx in
# the upstream repo); never committed here.
#
#   ports/bluez-firmware/fetch.sh            fetch if missing (idempotent)
#   ports/bluez-firmware/fetch.sh --force    re-download
#
# Pinned to one bluez-firmware commit; checked against its sha256.
sha256_of() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi; }

set -euo pipefail

COMMIT="cdf61dc691a49ff01a124752bd04194907f0f9cd"
BASE="https://raw.githubusercontent.com/RPi-Distro/bluez-firmware/$COMMIT/debian/firmware/broadcom"
CACHE="${BTFW_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/leandros/bluez-firmware}"
FORCE=0
[[ "${1:-}" == "--force" ]] && FORCE=1

# out-name sha256 source-path
FILES=(
"BCM4345C0.hcd 51c45e77ddad91a19e96dc8fb75295b2087c279940df2634b23baf71b6dea42c BCM4345C0.hcd"
)

mkdir -p "$CACHE"
fail=0
for entry in "${FILES[@]}"; do
    read -r name sum src <<<"$entry"
    out="$CACHE/$name"
    if [[ $FORCE == 0 && -f "$out" ]] && [[ "$(sha256_of "$out" | cut -d' ' -f1)" == "$sum" ]]; then
        continue
    fi
    tmp="$out.part"
    if ! net_curl -sfL --retry 3 -o "$tmp" "$BASE/$src"; then
        echo "bluez-firmware: download of $name failed" >&2
        rm -f "$tmp"; fail=1; continue
    fi
    got="$(sha256_of "$tmp" | cut -d' ' -f1)"
    if [[ "$got" != "$sum" ]]; then
        echo "bluez-firmware: $name sha256 mismatch ($got)" >&2
        rm -f "$tmp"; fail=1; continue
    fi
    mv "$tmp" "$out"
    echo "bluez-firmware: $name ($(wc -c < "$out" | tr -d ' ') bytes)"
done
echo "bluez-firmware: $(ls "$CACHE" | grep -vc '\.part$') files in $CACHE"
exit $fail
