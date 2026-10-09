#!/usr/bin/env bash
# Fetch the Realtek RTL8168-family PHY/MAC patch files ("rtl_nic/*.fw") from
# linux-firmware into $RTL_NIC_CACHE (default ~/.cache/leandros/rtl_nic).
# scripts/mkfs-f2fs-populated.py stages whatever is there as
# /lib/firmware/rtl_nic/ on the root image, where the kernel's r8169 driver
# loads the one its chip needs (drivers/src/r8169.rs, `r8169.fw=`).
#
# The files are redistributable (linux-firmware LICENCE.rtl_nic) but never
# committed here. Optional: the driver brings the link up without them.
#
#   ports/rtl_nic/fetch.sh            fetch missing files (idempotent)
#   ports/rtl_nic/fetch.sh --force    re-download everything
#
# Pinned to one linux-firmware tag; each file is checked against its sha256.

set -euo pipefail

TAG="20240909"
BASE="https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git/plain/rtl_nic"
CACHE="${RTL_NIC_CACHE:-$HOME/.cache/leandros/rtl_nic}"
FORCE=0
[[ "${1:-}" == "--force" ]] && FORCE=1

# name sha256 (of the file at $TAG)
FILES=(
"rtl8168d-1.fw ded779f55b3fc5bb8576b2d2b6c724581c72160b3c2f88e33b42dc6c2c4e70b0"
"rtl8168d-2.fw 35dd68c01a7b24f6e554bd2c0b9cf57cf962a826651f66e75ffcea37cf2c3bb9"
"rtl8168e-1.fw c38d6818a99336c8e43883bbc136f9fab3b1b470ec0e86c61bf5dac617fe94bd"
"rtl8168e-2.fw 0a489929b270df1533645a75d55ff357fd66e42057aa4e198ab738930de39386"
"rtl8168e-3.fw 1287fdd82ff2f60d75c9970c108b49cfd11456ad3fe29c9666b8539a35777dbf"
"rtl8168f-1.fw ed3794c651d38b807168c7c49431bb8036922b16766588c520a20287f277615d"
"rtl8168f-2.fw 5586b2093502f7dec762f23091fa90d6d8731e7d873509072ce2eae23ab8ee99"
"rtl8411-1.fw e66a3183787c80987e1e36c9f7d4b2c3cd5d976640b83f462702f914febc4b38"
"rtl8411-2.fw 80dbdacba4d5dfee46b6c2b4e433f6f2eeb756d29ad46bf2362ba20c8ec594b1"
"rtl8168g-2.fw f878ec719f78e0b104bc73362a9862611858bec893f58a1b3b289a0a57d71d25"
"rtl8168g-3.fw e4d5e62b0a5a097af34c7aa3c55f634fd5be2c232e02c6b983ebd7e699cb27e9"
"rtl8168h-2.fw 0b4beab008e308f28296c13188ee23edfeff7d865e398a7ade170495c8cec7e6"
"rtl8168fp-3.fw c526c06aa0ab8d9b57ba2b730bb1e40a1c3513f8591e238dceb2e0f45210ed27"
"rtl8105e-1.fw 03f7ee92f1a7c1723ac7a7447ae006930c9bb07e1d77c3d12bc70cc0d855bb00"
"rtl8402-1.fw a14adee93b7c7ecc738137d910334761ecf9c64d3addeccbd024a2f902f7f808"
"rtl8106e-1.fw b249074519d87ff8bde17185894333c993f25f747f221305cf69a5d7b4519674"
"rtl8106e-2.fw ba4f7ed4bac4768810ba8a6c118457f5912f61a0380aa4fb322093caf52de0ef"
"rtl8107e-2.fw edc3a14a76852c411c4af5514dae5d55be37873f56a7563f80db7a76f2dc1ba7"
)

mkdir -p "$CACHE"
fail=0
for entry in "${FILES[@]}"; do
    name="${entry%% *}"
    sum="${entry##* }"
    out="$CACHE/$name"
    if [[ $FORCE == 0 && -f "$out" ]] && [[ "$(shasum -a 256 "$out" | cut -d' ' -f1)" == "$sum" ]]; then
        continue
    fi
    tmp="$out.part"
    if ! curl -sfL --retry 3 -o "$tmp" "$BASE/$name?h=$TAG"; then
        echo "rtl_nic: download of $name failed" >&2
        rm -f "$tmp"; fail=1; continue
    fi
    got="$(shasum -a 256 "$tmp" | cut -d' ' -f1)"
    if [[ "$got" != "$sum" ]]; then
        echo "rtl_nic: $name sha256 mismatch ($got)" >&2
        rm -f "$tmp"; fail=1; continue
    fi
    mv "$tmp" "$out"
    echo "rtl_nic: $name ($(wc -c < "$out" | tr -d ' ') bytes)"
done
echo "rtl_nic: $(ls "$CACHE"/*.fw 2>/dev/null | wc -l | tr -d ' ') files in $CACHE"
exit $fail
