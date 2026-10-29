#!/usr/bin/env bash
# Kernel configuration TUI (vendored kconfiglib). Edits ./.config.
#   ./scripts/menuconfig.sh [--arch x86_64|aarch64]    interactive
#   ./scripts/menuconfig.sh defconfig <file> [--arch A]  .config from configs/<file>_defconfig
#   ./scripts/menuconfig.sh olddefconfig [--arch A]      fill new symbols with defaults
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARCH="x86_64"; ARGS=()
while [[ $# -gt 0 ]]; do
    case "$1" in --arch) ARCH="$2"; shift 2 ;; *) ARGS+=("$1"); shift ;; esac
done
CMD="${ARGS[0]:-menuconfig}"
export ARCH srctree="$ROOT" KCONFIG_CONFIG="$ROOT/.config"
K="$ROOT/scripts/kconfig"
cd "$ROOT"
case "$CMD" in
    menuconfig)
        [[ -f .config ]] || python3 "$K/to-cargo.py" defconfig --arch "$ARCH" --defconfig "configs/${ARCH}_defconfig" --out .config
        exec python3 "$K/menuconfig.py" Kconfig ;;
    defconfig)
        f="${ARGS[1]:?usage: menuconfig.sh defconfig <name|path>}"
        [[ -f "$f" ]] || f="configs/${f}_defconfig"
        exec python3 "$K/to-cargo.py" defconfig --arch "$ARCH" --defconfig "$f" --out .config ;;
    olddefconfig)
        exec python3 "$K/to-cargo.py" olddefconfig --arch "$ARCH" --config .config ;;
    *) echo "unknown command $CMD" >&2; exit 2 ;;
esac
