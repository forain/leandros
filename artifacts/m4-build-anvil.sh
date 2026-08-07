#!/bin/bash
# Incremental rebuild of patched anvil (software-EGL allowed + ANVIL_DRM_DEVICE
# direct add) for both arches, in the isolated job build tree, then stage into
# the relocated m3-gl-stack/out so mkfs packs it. The whole build is the long
# command -> run this via run_in_background.
D="$HOME/.claude-forain/jobs/afde2e74/tmp/m3-gl-stack"
OUT="$HOME/code/leandros-artifacts/m3-gl-stack/out"
export PATH="/opt/homebrew/bin:$PATH"
rc_all=0
for arch in aarch64 x86_64; do
  triple="$arch-unknown-linux-musl"
  bin="$D/src/smithay/target/$triple/release/anvil"
  before=$(stat -f %m "$bin" 2>/dev/null || echo 0)
  echo "===== BUILD anvil $arch $(date +%T) (prev mtime $before) ====="
  sh "$D/build-rust.sh" "$D/src/smithay/anvil" "$arch" --no-default-features --features udev 2>&1 | tail -30
  after=$(stat -f %m "$bin" 2>/dev/null || echo 0)
  if [ -f "$bin" ] && [ "$after" != "$before" ]; then
    cp "$bin" "$OUT/anvil-$arch"; echo "COPIED anvil-$arch (mtime $after) $(date +%T)"
  else
    echo "!!! BUILD DID NOT PRODUCE A FRESH anvil FOR $arch (mtime unchanged=$after)"; rc_all=1
  fi
done
echo "===== BUILD ALL DONE rc=$rc_all $(date +%T) ====="
