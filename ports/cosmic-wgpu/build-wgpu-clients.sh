#!/bin/bash
# Rebuild the libcosmic/iced COSMIC client apps with iced's wgpu renderer.
#
# The shipped binaries in m6-session-bins/out were built WITHOUT libcosmic's
# `wgpu` feature (only applibrary, settings and term had it), so iced's
# renderer was tiny-skia only and every widget was rasterised on the CPU even
# though cosmic-comp composites on the GPU. This script turns the renderer on
# with CARGO FEATURE FLAGS ONLY -- no COSMIC source is modified (user rule:
# GPU rendering only, no COSMIC patches).
#
# With `libcosmic/wgpu` iced builds its fallback compositor
# (iced_renderer::fallback): wgpu first, tiny-skia second. wgpu 28 is built
# with its default native backends (vulkan + gles), both dlopen'd at run time
# (libvulkan.so.1 / libEGL.so.1), so no DT_NEEDED is added. Which backend and
# whether the tiny-skia fallback is allowed is decided at run time by
# /bin/gpu-env (WGPU_BACKEND / ICED_BACKEND).
#
# Usage: build-wgpu-clients.sh <x86_64|aarch64> [app ...]
#   apps: greeter launcher notifications osd workspaces files-applet
#   (default: those six), and "applets" (NOT in the default set, see below).
#   Output: $ART/m6-session-bins/out-wgpu/<name>-<arch>.
#
# applibrary, settings and term already ship with wgpu (their original
# recipes enable it) and need no rebuild.
#
# PANEL APPLETS STAY ON tiny-skia (cosmic-panel-button, applet-minimize,
# applet-tiling): they are clients of cosmic-panel's embedded Wayland server,
# which only creates its zwp_linux_dmabuf_v1 global if its GLES renderer
# already exists when bind_display() runs at startup
# (xdg_shell_wrapper/mod.rs:93 -> shared_state.rs:139); on LeandrOS the
# renderer comes up ~0.3 s later, so the applets' server has no dmabuf, and
# Mesa's Wayland WSI (Venus) / EGL (virgl) cannot present from a hardware
# driver without it: wgpu fails with ERROR_SURFACE_LOST_KHR and the applet
# exits. Fixing that needs a cosmic-panel source change, which is out of
# bounds. The original tiny-skia-only applet builds ignore ICED_BACKEND
# (only iced's fallback compositor reads it), so they keep working.
# Runs on any machine that has the m6-session-bins toolchain (Mac, linux
# desktop). The per-machine build-rust.sh / gen-cargo-config.sh are used.
set -uo pipefail
arch=${1:?usage: build-wgpu-clients.sh <x86_64|aarch64> [app ...]}; shift
ART=${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}
ART=$(cd "$ART" && pwd -P)
D=$ART/m6-session-bins
S=$ART/m3-gl-stack/sysroot-$arch
OUT=$D/out-wgpu
mkdir -p "$OUT"
triple=$arch-unknown-linux-musl
apps=("$@")
[ ${#apps[@]} -gt 0 ] || apps=(greeter launcher notifications osd workspaces files-applet)

# The cargo config carries absolute toolchain/sysroot paths; regenerate it
# if it names another machine's tree.
cfg() {
    if ! grep -q "$D/toolchain" "$D/src/$1/.cargo/config.toml" 2>/dev/null; then
        sh "$D/gen-cargo-config.sh" "$D/src/$1" >/dev/null
    fi
}

# build <src-dir> <cargo args...>; then copy <bin>=<outname> pairs from $pairs
fail=0
build() {
    local src=$1; shift
    cfg "$src"
    ( cd "$D" && sh "$D/build-rust.sh" "src/$src" "$arch" "$@" ) > "$D/logs/wgpu-$src-$arch.console" 2>&1
    local rc=$?
    # build-rust.sh names its log after the manifest dir; keep ours separate
    [ -f "$D/logs/$(basename "$src")-$arch.log" ] && \
        cp "$D/logs/$(basename "$src")-$arch.log" "$D/logs/wgpu-$(basename "$src")-$arch.log"
    if [ $rc -ne 0 ]; then echo "FAIL $src ($arch) rc=$rc -- see $D/logs/wgpu-$src-$arch.console"; fail=1; return 1; fi
    return 0
}
copy() { # <target-dir> <binary> <outname>
    local t=$D/src/$1/target/$triple/release/$2
    cp -f "$t" "$OUT/$3-$arch" && echo "OK   $3-$arch  ($(wc -c < "$OUT/$3-$arch") bytes)"
}

for a in "${apps[@]}"; do
    case $a in
    greeter)
        # same env as build-greeter.sh; same --no-default-features
        export BINDGEN_EXTRA_CLANG_ARGS="-I$S/usr/include" VERGEN_GIT_SHA=leandros VERGEN_GIT_COMMIT_DATE=2026-07-26
        extra=()
        if [ "$(uname -s)" = Linux ]; then
            # pam-sys's build.rs adds -lpam_misc when the BUILD HOST is Linux
            # (cfg! in a build script is the host). Nothing references it
            # (our libpam shim has no pam_misc), so an empty archive satisfies it.
            stub=$D/toolchain/pam-misc-stub
            mkdir -p "$stub"; [ -f "$stub/libpam_misc.a" ] || printf '!<arch>\n' > "$stub/libpam_misc.a"
            extra=(--config "target.$triple.rustflags=[\"-L\",\"$stub\"]")
        fi
        build cosmic-greeter --no-default-features --features libcosmic/wgpu "${extra[@]}" \
            && copy cosmic-greeter cosmic-greeter cosmic-greeter ;;
    launcher)
        build cosmic-launcher --features wgpu && copy cosmic-launcher cosmic-launcher cosmic-launcher ;;
    notifications)
        build cosmic-notifications --features libcosmic/wgpu && copy cosmic-notifications cosmic-notifications cosmic-notifications ;;
    osd)
        build cosmic-osd --features libcosmic/wgpu && copy cosmic-osd cosmic-osd cosmic-osd ;;
    workspaces)
        # keep force-shm-screencopy (wl_shm capture only), as shipped
        build cosmic-workspaces-epoch --no-default-features --features wgpu,force-shm-screencopy \
            && copy cosmic-workspaces-epoch cosmic-workspaces cosmic-workspaces ;;
    files-applet)
        build cosmic-files -p cosmic-files-applet --features cosmic-files/wgpu \
            && copy cosmic-files cosmic-files-applet cosmic-files-applet ;;
    applets)
        build cosmic-applets -p cosmic-applet-minimize -p cosmic-applet-tiling -p cosmic-panel-button \
              --features libcosmic/wgpu \
            && copy cosmic-applets cosmic-applet-minimize cosmic-applet-minimize \
            && copy cosmic-applets cosmic-applet-tiling cosmic-applet-tiling \
            && copy cosmic-applets cosmic-panel-button cosmic-panel-button ;;
    *) echo "unknown app $a"; fail=1 ;;
    esac
done

# Report which renderers each output actually contains.
for f in "$OUT"/*-"$arch"; do
    vk=$(strings -n 8 "$f" | grep -c 'wgpu_hal::vulkan')
    gl=$(strings -n 8 "$f" | grep -c 'wgpu_hal::gles')
    echo "renderers $(basename "$f"): wgpu-vulkan=$([ "$vk" -gt 0 ] && echo y || echo n) wgpu-gles=$([ "$gl" -gt 0 ] && echo y || echo n)"
done
exit $fail
