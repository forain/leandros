#!/usr/bin/env bash
# Host-side unit tests of the vendored smoltcp (servers/net/vendor/smoltcp).
# The vendored Cargo.toml has no dev-dependencies and sits inside the kernel
# workspace, so the crate is copied to a temp dir with them added.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
cp -r "$root/servers/net/vendor/smoltcp/." "$tmp/"
printf '\n[dev-dependencies]\nrand = "0.8"\nrstest = "0.17"\n\n[workspace]\n' >> "$tmp/Cargo.toml"
cd "$tmp"
cargo test --lib --target "$(rustc -vV | sed -n 's/^host: //p')" \
  --target-dir "${SMOLTCP_TARGET_DIR:-${TMPDIR:-/tmp}/smoltcp-test-target}" "$@"
