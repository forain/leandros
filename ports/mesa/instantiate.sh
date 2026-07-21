#!/bin/sh
# Substitute the @DIRNAME@ placeholder in the meson ini templates with this
# ports directory absolute path, producing runnable *.ini in place. Idempotent.
set -e
ROOT="$(cd "$(dirname "$0")" && pwd)"
for f in cross-musl-x86_64.ini cross-musl-aarch64.ini native-host.ini; do
  sed "s#@DIRNAME@#$ROOT#g" "$ROOT/$f" > "$ROOT/$f.tmp" && mv "$ROOT/$f.tmp" "$ROOT/$f"
done
echo "instantiated cross/native ini files with ROOT=$ROOT"
