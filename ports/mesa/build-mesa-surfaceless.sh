#!/bin/sh
# Furthest-working Mesa cross-build (surfaceless+drm, EGL+GLESv2+GBM, softpipe/kms_swrast,
# no LLVM, no glvnd) for x86_64-unknown-linux-musl on the macOS host.
# Prereqs on host: zig 0.16, meson, ninja, `brew install bison` (>=3.x), flex,
#   python venv at $ROOT/.venv with mako+packaging+pyyaml (+pyelftools for inspection).
# Layout under $ROOT (this script's directory): src/mesa (mesa 25.3.6 checkout),
#   sysroot/ (cross-built libdrm etc. installed with prefix /usr), build/, .venv/.
set -e
ROOT="$(cd "$(dirname "$0")" && pwd)"
export PATH="/opt/homebrew/opt/bison/bin:$PATH"   # modern bison ahead of Apple's 2.3
# mako/packaging for codegen (needed by BOTH meson and ninja)
export PYTHONPATH="$(echo "$ROOT"/.venv/lib/python3.*/site-packages)"
cd "$ROOT/src/mesa"
meson setup "$ROOT/build/mesa-surfaceless" \
  --cross-file "$ROOT/cross-musl-x86_64.ini" \
  --prefix=/usr --buildtype=release \
  --wrap-mode=default --force-fallback-for=zlib,expat \
  -Dplatforms=[] \
  -Degl=enabled -Dgles2=enabled -Dgbm=enabled -Dopengl=true \
  -Dglx=disabled -Dgallium-drivers=softpipe -Dvulkan-drivers=[] \
  -Dllvm=disabled -Dshared-glapi=enabled -Dglvnd=disabled \
  -Dtools=[] -Dvalgrind=disabled
ninja -C "$ROOT/build/mesa-surfaceless"
