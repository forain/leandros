#!/bin/sh
# Runs INSIDE Alpine 3.21. Enumerates DT_NEEDED for the produced Mesa libs,
# stages the NEW runtime deps llvmpipe drags in (libLLVM/libstdc++/libgcc_s/
# libzstd + their transitive closure), and patchelf-rewrites the Alpine musl
# soname libc.musl-<arch>.so.1 -> libc.so on every shipped lib so they load
# against LeandrOS vanilla musl. Emits '=== rc=N ===' trailer.
ARCH="$1"
ML="$ARCH"           # musl soname arch token == arch for x86_64/aarch64
S=/out/stage-$ARCH/usr/lib
D=/out/deps-$ARCH
(
  apk add --no-cache patchelf file binutils \
      llvm19-libs libstdc++ libgcc zstd-libs libffi libxml2 xz-libs >/dev/null 2>&1 &&
  echo "########## RAW DT_NEEDED + Type, per Mesa lib ##########" &&
  for f in "$S"/libEGL.so.1.0.0 "$S"/libGLESv2.so.2.0.0 "$S"/libGLESv1_CM.so.1.1.0 \
           "$S"/libgbm.so.1.0.0 "$S"/libgallium-25.3.6.so "$S"/gbm/dri_gbm.so; do
    echo "== $(basename $f) : $(readelf -h "$f" 2>/dev/null | sed -n 's/.*Type:[[:space:]]*//p') =="
    readelf -d "$f" 2>/dev/null | sed -n 's/.*NEEDED.*\[\(.*\)\]/  NEEDED \1/p'
  done &&
  echo "########## STAGE new deps + transitive closure into $D ##########" &&
  rm -rf "$D" && mkdir -p "$D" &&
  for lib in libLLVM.so.19.1 libstdc++.so.6 libgcc_s.so.1 libzstd.so.1 libxml2.so.2 liblzma.so.5; do
    src=$(find /usr/lib -maxdepth 1 -name "$lib" 2>/dev/null | head -1)
    [ -z "$src" ] && src=$(find /usr/lib -maxdepth 1 -name "${lib%.*}*" 2>/dev/null | head -1)
    real=$(readlink -f "$src")
    if [ -f "$real" ]; then
      base=$(basename "$real"); cp -a "$real" "$D/$base"
      son=$(readelf -d "$real" 2>/dev/null | sed -n 's/.*SONAME.*\[\(.*\)\]/\1/p')
      [ -n "$son" ] && [ "$son" != "$base" ] && ln -sf "$base" "$D/$son"
      echo "staged $lib -> $base (soname=$son)"
    else echo "!! MISSING $lib"; fi
  done &&
  echo "########## DT_NEEDED of staged deps (find hidden transitive deps) ##########" &&
  for f in "$D"/libLLVM* "$D"/libstdc++* "$D"/libgcc_s* "$D"/libzstd* "$D"/libxml2* "$D"/liblzma*; do
    [ -f "$f" ] || continue
    echo "== $(basename $f) =="
    readelf -d "$f" 2>/dev/null | sed -n 's/.*NEEDED.*\[\(.*\)\]/  NEEDED \1/p'
  done &&
  echo "########## PATCHELF libc.musl-$ML.so.1 -> libc.so on Mesa libs + staged deps ##########" &&
  for f in "$S"/libEGL.so.1.0.0 "$S"/libGLESv2.so.2.0.0 "$S"/libGLESv1_CM.so.1.1.0 \
           "$S"/libgbm.so.1.0.0 "$S"/libgallium-25.3.6.so "$S"/gbm/dri_gbm.so \
           "$D"/libLLVM.so.19.1 "$D"/libstdc++.so.6* "$D"/libgcc_s.so.1 "$D"/libzstd.so.1* \
           "$D"/libxml2.so.2* "$D"/liblzma.so.5*; do
    [ -f "$f" ] || continue
    if readelf -d "$f" 2>/dev/null | grep -q "libc.musl-$ML.so.1"; then
      patchelf --replace-needed libc.musl-$ML.so.1 libc.so "$f" && echo "patched: $(basename $f)"
    fi
  done &&
  echo "########## POST-PATCH union of ALL NEEDED across ship set + deps ##########" &&
  for f in "$S"/*.so.* "$S"/gbm/*.so "$D"/*.so*; do [ -f "$f" ] && readelf -d "$f" 2>/dev/null | grep NEEDED; done \
     | sed 's/.*\[//;s/\]//' | sort -u &&
  echo "########## staged deps dir listing ##########" &&
  ls -la "$D"
)
RC=$?
echo "=== rc=$RC arch=$ARCH verify ==="
