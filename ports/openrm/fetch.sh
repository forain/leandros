#!/bin/bash
# Fetch the pinned OpenRM source tree and its matching GSP firmware into the
# shared cache ($OPENRM_CACHE, default ~/.cache/leandros/openrm -- outside the
# repo, so worktrees share one copy and nothing NVIDIA ships is committed).
# Every download is verified against the sha256 pinned in version.sh.
#
#   $OPENRM_CACHE/open-gpu-kernel-modules-<tag>/   source (MIT/GPLv2 dual)
#   $OPENRM_CACHE/firmware/<tag>/gsp_ga10x.bin     GSP-RM for Ampere/Ada (RTX 3070)
#   $OPENRM_CACHE/firmware/<tag>/gsp_tu10x.bin     GSP-RM for Turing / GA100
#
# The firmware is NVIDIA-proprietary (redistributable under the driver's
# license, as linux-firmware and nvidia-haiku do); it is extracted from the
# official .run installer (a makeself archive whose payload is a zstd tar).
# The 358 MiB .run is deleted afterwards unless KEEP_RUN=1.
#
# Needs: curl, shasum (or sha256sum), tar, zstd.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=ports/openrm/version.sh
source "$HERE/version.sh"
mkdir -p "$OPENRM_CACHE"

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'
    else shasum -a 256 "$1" | awk '{print $1}'; fi
}

verify() { # file expected
    local got; got="$(sha256 "$1")"
    if [[ "$got" != "$2" ]]; then
        echo "fetch.sh: sha256 mismatch for $1" >&2
        echo "  expected $2" >&2
        echo "  got      $got" >&2
        exit 1
    fi
}

fetch() { # url dest sha
    if [[ -f "$2" ]] && [[ "$(sha256 "$2")" == "$3" ]]; then return; fi
    echo "=== [openrm] downloading $1"
    curl -fL --retry 3 -o "$2.part" "$1"
    mv "$2.part" "$2"
    verify "$2" "$3"
}

# ---- source -----------------------------------------------------------------
if [[ ! -d "$OPENRM_SRC/src/nvidia" ]]; then
    TARBALL="$OPENRM_CACHE/open-gpu-kernel-modules-$OPENRM_TAG.tar.gz"
    fetch "$OPENRM_TARBALL_URL" "$TARBALL" "$OPENRM_TARBALL_SHA256"
    tar -xzf "$TARBALL" -C "$OPENRM_CACHE"
fi
echo "=== [openrm] source: $OPENRM_SRC"

# ---- GSP firmware -------------------------------------------------------------
GA="$OPENRM_FW_DIR/gsp_ga10x.bin"
TU="$OPENRM_FW_DIR/gsp_tu10x.bin"
if [[ ! -f "$GA" || "$(sha256 "$GA")" != "$GSP_GA10X_SHA256" ||
      ! -f "$TU" || "$(sha256 "$TU")" != "$GSP_TU10X_SHA256" ]]; then
    RUN="$OPENRM_CACHE/NVIDIA-Linux-x86_64-$OPENRM_TAG.run"
    fetch "$NVRUN_URL" "$RUN" "$NVRUN_SHA256"
    SKIP="$(head -n 20 "$RUN" | LC_ALL=C sed -n "s/^skip=//p")"
    mkdir -p "$OPENRM_FW_DIR"
    tail -n +"$SKIP" "$RUN" | zstd -dc | \
        tar -x -C "$OPENRM_FW_DIR" --strip-components 1 firmware/gsp_ga10x.bin firmware/gsp_tu10x.bin
    [[ "${KEEP_RUN:-0}" == 1 ]] || rm -f "$RUN"
fi
verify "$GA" "$GSP_GA10X_SHA256"
verify "$TU" "$GSP_TU10X_SHA256"
echo "=== [openrm] firmware: $OPENRM_FW_DIR"
ls -l "$OPENRM_FW_DIR"
