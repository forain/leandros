#!/usr/bin/env bash
. "$(cd "$(dirname "$0")/../.." && pwd)/scripts/lib/netretry.sh"
# Fetch the Wi-Fi firmware the Raspberry Pi 5's CYW43455 (brcmfmac family)
# needs from RPi-Distro/firmware-nonfree into $BRCMFMAC_CACHE (default
# ~/.cache/leandros/brcmfmac). scripts/mkfs-f2fs-populated.py stages whatever
# is there as /lib/firmware/brcm/* on the aarch64 root image.
#
# Redistributable under the Cypress EULA (binary-redist-Cypress, see
# LICENCE.cypress next to this script) and GPL-2 for the nvram text; never
# committed here.
#
#   ports/brcmfmac/fetch.sh            fetch missing files (idempotent)
#   ports/brcmfmac/fetch.sh --force    re-download everything
#
# Pinned to one firmware-nonfree commit (branch bookworm); every file is
# checked against its sha256. Git symlinks in the repo (the Pi 5 nvram txt is
# one, pointing at brcmfmac43455-sdio.txt) are resolved to the real file and
# the real bytes are stored under the requested name.
#
#   out name                                      source (debian/config/brcm80211/)
#   cyw43455-sdio.bin                             cypress/cyfmac43455-sdio-standard.bin
#   cyw43455-sdio.clm_blob                        cypress/cyfmac43455-sdio.clm_blob
#   brcmfmac43455-sdio.raspberrypi,5-model-b.txt  brcm/brcmfmac43455-sdio.raspberrypi,5-model-b.txt
sha256_of() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi; }

set -euo pipefail

COMMIT="c91cd2804cf7463aab913e7247c176049f16bbd6"
BASE="https://raw.githubusercontent.com/RPi-Distro/firmware-nonfree/$COMMIT/debian/config/brcm80211"
CACHE="${BRCMFMAC_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/leandros/brcmfmac}"
FORCE=0
[[ "${1:-}" == "--force" ]] && FORCE=1

# out-name sha256 source-path
FILES=(
"cyw43455-sdio.bin d608f866582519c0a28d86db43040f4f1b98dd1d153e72e9752586546b4a36c3 cypress/cyfmac43455-sdio-standard.bin"
"cyw43455-sdio.clm_blob 9823842cae9fb9a5dd1e5fb31f595516ec7deee341354bef30bb3026eee29cc1 cypress/cyfmac43455-sdio.clm_blob"
"brcmfmac43455-sdio.raspberrypi,5-model-b.txt ca709be81a78bdb6932936374f39943acbd7af07fae6151011127599a3ce9e3d brcm/brcmfmac43455-sdio.raspberrypi,5-model-b.txt"
)

# normalise dir/../x without realpath (macOS)
norm() { local IFS=/ p out=(); for p in $1; do case "$p" in ""|.) ;; ..) unset 'out[${#out[@]}-1]' ;; *) out+=("$p") ;; esac; done; echo "${out[*]}"; }

mkdir -p "$CACHE"
fail=0
for entry in "${FILES[@]}"; do
    read -r name sum src <<<"$entry"
    out="$CACHE/$name"
    if [[ $FORCE == 0 && -f "$out" ]] && [[ "$(sha256_of "$out" | cut -d' ' -f1)" == "$sum" ]]; then
        continue
    fi
    tmp="$out.part"
    ok=1
    for _hop in 1 2 3; do
        if ! net_curl -sfL --retry 3 -o "$tmp" "$BASE/$src"; then ok=0; break; fi
        # raw.githubusercontent serves a git symlink as its target path (one short line).
        if [[ $(wc -c < "$tmp") -lt 128 ]] && [[ $(wc -l < "$tmp") -eq 0 ]] && [[ "$(tr -d '[:print:]' < "$tmp")" == "" ]] && [[ "$(cat "$tmp")" == *[./]* ]] && [[ "$(cat "$tmp")" != *=* ]]; then
            src="$(norm "$(dirname "$src")/$(cat "$tmp")")"
            continue
        fi
        break
    done
    if [[ $ok == 0 ]]; then
        echo "brcmfmac: download of $name failed" >&2
        rm -f "$tmp"; fail=1; continue
    fi
    got="$(sha256_of "$tmp" | cut -d' ' -f1)"
    if [[ "$got" != "$sum" ]]; then
        echo "brcmfmac: $name sha256 mismatch ($got)" >&2
        rm -f "$tmp"; fail=1; continue
    fi
    mv "$tmp" "$out"
    echo "brcmfmac: $name ($(wc -c < "$out" | tr -d ' ') bytes)"
done
echo "brcmfmac: $(ls "$CACHE" | grep -vc '\.part$') files in $CACHE"
exit $fail
