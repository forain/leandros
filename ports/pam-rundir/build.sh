#!/usr/bin/env bash
# Build pam_rundir.so (dynamic musl PAM module) for both arches -> out/<arch>/usr/lib/security/pam_rundir.so (an overlay tree).
# Needs the dynamic sysroot (m3-gl-stack) for libc/headers; the PAM headers are
# API-identical to Linux-PAM's.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
TC="$HERE/../../scripts/toolchain/dyn"
for arch in ${ARCHES:-aarch64 x86_64}; do
  mkdir -p "$HERE/out/$arch/usr/lib/security"
  "$TC/$arch-linux-musl-cc" -shared -fPIC -O2 -Wall -o "$HERE/out/$arch/usr/lib/security/pam_rundir.so" "$HERE/pam_rundir.c"
  echo "built: $HERE/out/$arch/usr/lib/security/pam_rundir.so"
done
