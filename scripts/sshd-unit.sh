#!/usr/bin/env bash
# Host-side unit tests for sshd's userauth.rs (userland/sshd/unit). Runs on the
# build machine (Mac or Linux), no QEMU. Fails on a test failure; skips with a
# message only when the pinned toolchain is not installed.
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TC="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$ROOT_DIR/rust-toolchain.toml" | head -1)"
if ! rustc "+$TC" -vV >/dev/null 2>&1; then
    echo "sshd-unit: SKIPPED, pinned toolchain $TC is not installed"
    exit 0
fi
HOST="$(rustc "+$TC" -vV | sed -n 's/^host: //p')"
export CARGO_TARGET_DIR="${SSHD_UNIT_TARGET_DIR:-${TMPDIR:-/tmp}/leandros-sshd-unit-target-$(id -u)}"
echo "sshd-unit: cargo +$TC test --release --target $HOST"
LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT
if cargo "+$TC" test --release --target "$HOST" --manifest-path "$ROOT_DIR/userland/sshd/unit/Cargo.toml" >"$LOG" 2>&1; then
    grep -E "^test result" "$LOG" | head -1
else
    grep -E "FAILED|^error|panicked" "$LOG" | head -20
    echo "sshd-unit: FAILED"
    exit 1
fi
