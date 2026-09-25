#!/usr/bin/env bash
# Build a standalone, CLI-driven QEMU for Apple Silicon macOS with host-GPU
# virgl rendering: HVF + virtio-gpu-gl / virtio-vga-gl, virglrenderer on
# ANGLE (OpenGL ES over Metal), egl-headless for offscreen use (pixels over
# VNC: a GL scanout has no surface for QMP screendump). Homebrew's qemu has no
# virglrenderer and no *-gl devices; this does not use UTM's app or binaries.
#
#   GL stack:  guest Mesa virgl -> virtio-gpu-gl -> virglrenderer 1.3.0
#              (patched: eventfd stand-in, so fences retire on virgl's thread)
#              -> libepoxy 1.5.10 (patched: EGL on macOS) -> ANGLE libEGL/
#              libGLESv2 (Metal backend, built here from source) -> Metal
#   QEMU:      11.1.1 + one patch to ui/egl-helpers.c: an ANGLE EGL display
#              on macOS (egl-headless gets a context without GBM), and a
#              BGRA_EXT readback texture (strict ES rejects RGBA+BGRA, which
#              made every egl-headless frame black)
#
# Usage: scripts/mac-qemu-gpu/build.sh [stage...]
#   stages: gn angle epoxy virgl qemu   (default: all, in order; each stage is
#           skipped when its stamp exists — pass --force to rebuild)
#           venus: EXPERIMENTAL extra QEMU in $PREFIX-venus (see stage_venus)
# Env:    LEANDROS_QEMU_PREFIX  install prefix   (default ~/.local/qemu-gpu)
#         LEANDROS_QEMU_WORK    sources + builds (default ~/.cache/leandros-qemu-gpu)
#         SDKROOT               (default: newest CLT SDK that has libc++ headers)
# Host needs: Command Line Tools (no Xcode required), Homebrew with
#   meson ninja pkgconf python3 glib pixman libslirp libpng jpeg-turbo zstd
#   libusb snappy lzo gnutls capstone dtc
# Everything installs under the prefix; nothing touches Homebrew or /usr/local.
set -euo pipefail

PREFIX="${LEANDROS_QEMU_PREFIX:-$HOME/.local/qemu-gpu}"
WORK="${LEANDROS_QEMU_WORK:-$HOME/.cache/leandros-qemu-gpu}"
HERE="$(cd "$(dirname "$0")" && pwd)"
PATCHES="$HERE/patches"
NCPU="$(sysctl -n hw.ncpu)"
FORCE=0

# ── pinned sources ──────────────────────────────────────────────────────────
QEMU_VER=11.1.1
QEMU_URL=https://download.qemu.org/qemu-$QEMU_VER.tar.xz
QEMU_SHA=079ffbff8a7111bbc89022107cbabf3bbfd614d5fc9d7cc675991196aca12482
VIRGL_VER=1.3.0
VIRGL_URL=https://gitlab.freedesktop.org/virgl/virglrenderer/-/archive/$VIRGL_VER/virglrenderer-$VIRGL_VER.tar.bz2
VIRGL_SHA=088040d130eaa0458a978fe7867fbfb1fcf1fdff52bf3b27a00658828bc4189f
EPOXY_VER=1.5.10
EPOXY_URL=https://github.com/anholt/libepoxy/archive/refs/tags/$EPOXY_VER.tar.gz
EPOXY_SHA=a7ced37f4102b745ac86d6a70a9da399cc139ff168ba6b8002b4d8d43c900c15
# Experimental Venus (see `venus` stage): UTM's virglrenderer fork, which ports
# vkr to macOS (no epoll/memfd/eventfd), over MoltenVK from Homebrew.
VENUS_VIRGL_REPO=https://github.com/utmapp/virglrenderer.git
VENUS_VIRGL_COMMIT=5d26f605f50f8e22002ec6db5fb775e1992d4e96
GN_REPO=https://gn.googlesource.com/gn
GN_COMMIT=2dfb8cbd3b749242ee4492a7f15eba6889d52c7f
# ANGLE: the same revision + dependency set as MacPorts' angle 2.1.28727
# (chromium stable), fetched as GitHub tarballs — no depot_tools, no Xcode.
ANGLE_REV=72b8f72a7587ec776d7d2a57d275a6e9b1781b1d
ANGLE_POS=28727
ANGLE_DEPS=(
  # name  url  sha256  dest (relative to the ANGLE tree)
  "angle https://github.com/google/angle/archive/$ANGLE_REV.tar.gz 658f054a48f0a45d0817307448a006450e36b472f77cd44c92fa484432c2b45c ."
  "build https://github.com/gsource-mirror/chromium-src-build/archive/18940f0d92f236dd7b8672516700afa2b1f3d123.tar.gz e481a6d424cb9ca66b8fe7592924346d31658360998d6d246bcb7369b9df09a1 build"
  "testing https://github.com/gsource-mirror/chromium-src-testing/archive/491a1efb39359ee1e82d063ab3c32016f2fee111.tar.gz 5a1957de8ad7ffdc7925ae2d6dc0bcea36779ca2b0839b31b0be8c0a6b26b0ad testing"
  "crjsoncpp https://github.com/gsource-mirror/chromium-src-third_party-jsoncpp/archive/f62d44704b4da6014aa231cfc116e7fd29617d2a.tar.gz 7360eff9ce58208c68da260db23bdc29bbc00c769905af10aa45af0dd308aba4 third_party/jsoncpp"
  "rust https://github.com/gsource-mirror/chromium-src-third_party-rust/archive/4dec6f65bcb34c4c289736b0e6fd571b35c6c665.tar.gz afd7eec4003255bad8a37b8f557e7bd508e1ec3eb5b2e37027098dae417c1a69 third_party/rust"
  "zlib https://github.com/gsource-mirror/chromium-src-third_party-zlib/archive/5eb4d7ed380f214e7a0a23c18f629048d3ba9e00.tar.gz 1bd98d6024de834deb46caf5087b5d5408a94cd2d6cdf8d508ce1e7f9ddefead third_party/zlib"
  "spvh https://github.com/KhronosGroup/SPIRV-Headers/archive/496543121ce6419f23d6fa5d7194ba66c36212d2.tar.gz a9bb9c48713245eacf97cc539b6f1d45405a92d8813f8b82e635f0a085ee9898 third_party/spirv-headers/src"
  "spvt https://github.com/KhronosGroup/SPIRV-Tools/archive/b40380bfa431d028fb7ca8eb375e4d21ea98a70e.tar.gz ebc06345ece01d6c7ec0b3d0c9f6a27eb2e59ac4d97376f5e4d5ca38abf096c7 third_party/spirv-tools/src"
  "vkh https://github.com/KhronosGroup/Vulkan-Headers/archive/31386378257ac8653ce5b32c93baec385259ebbe.tar.gz efe79256adb6f2a5112bdcd03712d3aeb59a2e1a54a9e97650223dfe57b12067 third_party/vulkan-headers/src"
  "astc https://github.com/ARM-software/astc-encoder/archive/2319d9c4d4af53a7fc7c52985e264ce6e8a02a9b.tar.gz 8b5068ef28a8db1cb354d89d9cefd19d43eddfc72c3468fce7ebb92b2431d4c4 third_party/astc-encoder/src"
  "jsoncpp https://github.com/open-source-parsers/jsoncpp/archive/42e892d96e47b1f6e29844cc705e148ec4856448.tar.gz 0b40e4598d68d3dbd8cab90b249e18f1363ecc694c38f727851f4db34b6887ec third_party/jsoncpp/source"
)

# ── toolchain ───────────────────────────────────────────────────────────────
[ "$(uname -s)/$(uname -m)" = "Darwin/arm64" ] || { echo "Apple Silicon macOS only" >&2; exit 1; }
if [ -z "${SDKROOT:-}" ]; then
    # The CLT "MacOSX.sdk" symlink may point at an SDK whose libc++ headers or
    # .tbd stubs are broken (seen: MacOSX27.0 "tapi error: malformed file");
    # prefer the newest 26.x SDK that has libc++ headers.
    for s in /Library/Developer/CommandLineTools/SDKs/MacOSX26*.sdk \
             /Library/Developer/CommandLineTools/SDKs/MacOSX*.sdk; do
        [ -f "$s/usr/include/c++/v1/map" ] && { SDKROOT="$s"; break; }
    done
fi
# Build against a private snapshot of that SDK: a Command Line Tools update
# replaces /Library/Developer/CommandLineTools/SDKs in place, and a build that
# straddles one fails mid-way ("library 'System' not found").
mkdir -p "$WORK/sdk"
SDKROOT="$(cd "$SDKROOT" && pwd -P)"
_snap="$WORK/sdk/$(basename "$SDKROOT")"
if [ ! -e "$_snap/usr/lib/libSystem.tbd" ]; then
    rsync -a "$SDKROOT/" "$_snap/"
fi
SDKROOT="$_snap"
export SDKROOT
export CC=/usr/bin/clang CXX=/usr/bin/clang++ OBJC=/usr/bin/clang
export MACOSX_DEPLOYMENT_TARGET=14.0
BREW="$(brew --prefix)"
export PATH="$BREW/bin:/usr/bin:/bin:/usr/sbin:/sbin"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig:$BREW/lib/pkgconfig:$BREW/share/pkgconfig"
mkdir -p "$WORK/dl" "$PREFIX/lib"
echo "prefix=$PREFIX work=$WORK SDKROOT=$SDKROOT"

fetch() { # url sha256 file
    local f="$WORK/dl/$3"
    if [ ! -f "$f" ]; then curl -fsSL --retry 3 -o "$f.part" "$1"; mv "$f.part" "$f"; fi
    echo "$2  $f" | shasum -a 256 -c --status || { echo "checksum mismatch: $f" >&2; exit 1; }
}
unpack() { # file dest
    rm -rf "$2"; mkdir -p "$2"; tar xf "$WORK/dl/$1" -C "$2" --strip-components=1
}
stamp() { [ "$FORCE" = 0 ] && [ -f "$WORK/.stamp-$1" ]; }
done_stamp() { touch "$WORK/.stamp-$1"; echo "== $1 done"; }

stage_gn() {
    stamp gn && return 0
    rm -rf "$WORK/gn"; git clone -q "$GN_REPO" "$WORK/gn"
    git -C "$WORK/gn" checkout -q "$GN_COMMIT"
    (cd "$WORK/gn" && AR=/usr/bin/ar python3 build/gen.py >/dev/null && ninja -C out gn)
    done_stamp gn
}

stage_angle() {
    stamp angle && return 0
    local src="$WORK/angle" d name url sha dest
    for d in "${ANGLE_DEPS[@]}"; do
        read -r name url sha dest <<<"$d"; fetch "$url" "$sha" "angle-$name.tgz"
    done
    unpack angle-angle.tgz "$src"
    for d in "${ANGLE_DEPS[@]:1}"; do
        read -r name url sha dest <<<"$d"; unpack "angle-$name.tgz" "$src/$dest"
    done
    cp "$PATCHES/angle-macports/gclient_args.gni" "$src/build/config/"
    (cd "$src"
     for p in patch-commit-id.diff patch-apple-toolchain.diff patch-src-common-platform.diff; do
         patch -s -p0 < "$PATCHES/angle-macports/$p"
     done
     patch -s -p1 < "$PATCHES/angle-clt-no-xcode.patch"
     sed -i '' "s|@COMMIT_POSITION@|$ANGLE_POS|" src/commit_id.py
     "$WORK/gn/out/gn" gen out --script-executable=python3 --args="
        mac_sdk_min=\"14.0\" mac_deployment_target=\"14.0\" target_cpu=\"arm64\"
        install_prefix=\"$PREFIX\"
        is_official_build=true is_clang=false use_custom_libcxx=false
        treat_warnings_as_errors=false fatal_linker_warnings=false
        enable_rust=false angle_build_tests=false
        angle_enable_metal=true angle_enable_gl=true angle_enable_vulkan=false"
     ANGLE_UPSTREAM_HASH="${ANGLE_REV:0:12}" ninja -C out angle
     ninja -C out install_angle >/dev/null)
    local f
    for f in libEGL libGLESv2; do
        install_name_tool -id "$PREFIX/lib/$f.dylib" "$PREFIX/lib/$f.dylib"
    done
    # GLES 1 is never used (virglrenderer needs ES 3), and its load command
    # has no header room for an absolute libGLESv2 path: drop it.
    rm -f "$PREFIX/lib/libGLESv1_CM.dylib" "$PREFIX/lib/pkgconfig/glesv1_cm.pc"
    # ANGLE installs headers the rest of the stack must not pick up
    rm -rf "$PREFIX/include/"{CL,GLX,WGL,GLSLANG,vulkan,platform} "$PREFIX/include/"*.h
    sed -i '' "s|^prefix=.*|prefix=$PREFIX|" "$PREFIX/lib/pkgconfig/"{egl,glesv2}.pc
    done_stamp angle
}

stage_epoxy() {
    stamp epoxy && return 0
    fetch "$EPOXY_URL" "$EPOXY_SHA" "libepoxy-$EPOXY_VER.tar.gz"
    unpack "libepoxy-$EPOXY_VER.tar.gz" "$WORK/libepoxy"
    (cd "$WORK/libepoxy"
     patch -s -p1 < "$PATCHES/libepoxy-1.5.10-angle-macos.patch"
     # dlopen ANGLE by absolute path: no DYLD_* environment needed at runtime
     sed -i '' "s|@ANGLE_LIBDIR@|$PREFIX/lib/|" src/dispatch_common.c
     meson setup build --prefix="$PREFIX" --buildtype=release \
        -Degl=yes -Dglx=no -Dx11=false -Dtests=false \
        -Dc_args="-I$PREFIX/include" >/dev/null
     meson compile -C build && meson install -C build >/dev/null)
    done_stamp epoxy
}

stage_virgl() {
    stamp virgl && return 0
    fetch "$VIRGL_URL" "$VIRGL_SHA" "virglrenderer-$VIRGL_VER.tar.bz2"
    unpack "virglrenderer-$VIRGL_VER.tar.bz2" "$WORK/virglrenderer"
    # eventfd stand-in (an O_RDWR FIFO) so VIRGL_RENDERER_THREAD_SYNC works:
    # fences retire from virgl's sync thread instead of QEMU's 10 ms poll
    (cd "$WORK/virglrenderer" && patch -s -p1 < "$PATCHES/virglrenderer-1.3.0-macos-eventfd.patch")
    # virglrenderer's code generators want PyYAML
    [ -x "$WORK/venv/bin/python3" ] || python3 -m venv "$WORK/venv"
    "$WORK/venv/bin/pip" -q install pyyaml
    (cd "$WORK/virglrenderer"
     PATH="$WORK/venv/bin:$PATH" meson setup build --prefix="$PREFIX" --buildtype=release \
        -Dplatforms=egl -Dtests=false -Dvenus=false \
        -Dc_args="-I$PREFIX/include" >/dev/null
     PATH="$WORK/venv/bin:$PATH" meson compile -C build && meson install -C build >/dev/null)
    done_stamp virgl
}

qemu_build() { # <build dir name> <install prefix>
    (cd "$WORK/qemu"
     rm -rf "$1" && mkdir -p "$1" && cd "$1"
     ../configure --prefix="$2" \
        --target-list=aarch64-softmmu,x86_64-softmmu \
        --enable-hvf --enable-cocoa --enable-opengl --enable-virglrenderer \
        --enable-slirp --enable-vnc --enable-png --enable-zstd \
        --disable-sdl --disable-gtk --disable-docs --disable-werror \
        --extra-cflags="-I$PREFIX/include" --extra-ldflags="-L$PREFIX/lib"
     make -j"$NCPU" && make install >/dev/null)
}

stage_qemu() {
    stamp qemu && return 0
    fetch "$QEMU_URL" "$QEMU_SHA" "qemu-$QEMU_VER.tar.xz"
    unpack "qemu-$QEMU_VER.tar.xz" "$WORK/qemu"
    (cd "$WORK/qemu" && patch -s -p1 < "$PATCHES/qemu-11.1-egl-angle-macos.patch")
    qemu_build build "$PREFIX"
    done_stamp qemu
}

# EXPERIMENTAL, not part of the default stages. A second QEMU in
# "$PREFIX-venus" whose virglrenderer has Venus. Reaches: venus capset, a
# Venus context, host-visible ring blobs mapped into the guest. Stops at: the
# render server rejects the guest's first command, vkCreateInstance ("CS
# error"), so zink/vktest fail. Needs `brew install molten-vk vulkan-loader`.
stage_venus() {
    stamp venus && return 0
    [ -f "$WORK/.stamp-qemu" ] || { echo "venus: build the qemu stage first" >&2; exit 1; }
    for f in molten-vk vulkan-loader; do
        [ -d "$BREW/opt/$f" ] || { echo "venus: brew install $f" >&2; exit 1; }
    done
    local src="$WORK/virglrenderer-venus"
    [ -d "$src/.git" ] || git clone -q "$VENUS_VIRGL_REPO" "$src"
    git -C "$src" checkout -q "$VENUS_VIRGL_COMMIT"
    (cd "$src" && rm -rf build
     PATH="$WORK/venv/bin:$PATH" meson setup build --prefix="$PREFIX-venus" --buildtype=release \
        -Dplatforms=egl -Dtests=false -Dvenus=true -Dneptune=false -Dvtest=false \
        -Dcheck-gl-errors=false -Dvulkan-dload=false -Drender-server-mode=process \
        -Dc_args="-I$PREFIX/include" >/dev/null
     PATH="$WORK/venv/bin:$PATH" meson compile -C build && meson install -C build >/dev/null)
    PKG_CONFIG_PATH="$PREFIX-venus/lib/pkgconfig:$PKG_CONFIG_PATH" qemu_build build-venus "$PREFIX-venus"
    done_stamp venus
}

STAGES=()
for a in "$@"; do
    case "$a" in
        --force) FORCE=1 ;;
        gn|angle|epoxy|virgl|qemu|venus) STAGES+=("$a") ;;
        *) echo "unknown stage/flag: $a" >&2; exit 2 ;;
    esac
done
[ ${#STAGES[@]} -gt 0 ] || STAGES=(gn angle epoxy virgl qemu)
for s in "${STAGES[@]}"; do "stage_$s"; done

cat <<EOF
== installed: $PREFIX/bin/qemu-system-aarch64 (+ x86_64)
   check:  $PREFIX/bin/qemu-system-aarch64 -device help | grep -- -gl
   run:    LEANDROS_QEMU_PREFIX=$PREFIX ./scripts/run-qemu.sh aarch64   (auto-detected at the default prefix)
EOF
