#!/bin/sh
# Furthest-working Mesa configure (surfaceless variant, configure-only).
# Run from $ROOT/src/mesa; see build-mesa-surfaceless.sh for the full driver.
ROOT="$(cd "$(dirname "$0")" && pwd)"
export PYTHONPATH="$(echo "$ROOT"/.venv/lib/python3.*/site-packages)"
meson setup "$ROOT/build/mesa-surfaceless" \
  --cross-file "$ROOT/cross-musl-x86_64.ini" \
  --prefix=/usr --buildtype=release \
  --wrap-mode=default --force-fallback-for=zlib,expat \
  '-Dplatforms=[]' \
  -Degl=enabled -Dgles2=enabled -Dgbm=enabled -Dopengl=true \
  -Dglx=disabled -Dgallium-drivers=softpipe '-Dvulkan-drivers=[]' \
  -Dllvm=disabled -Dshared-glapi=enabled -Dglvnd=disabled \
  '-Dtools=[]' -Dvalgrind=disabled
