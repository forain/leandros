#!/usr/bin/env bash
# dualboot-gui-test.sh [aarch64|x86_64]  -- the installer GUI's dual-boot code paths, headless, in QEMU.
#
#  1. boot the LeandrOS stick with the gap fixture (fxgap-<arch>.img) attached as NVMe;
#  2. fake backend (ports/leandros-installer/test/fake-backend.sh): --self-test-dualboot
#     reaches page Done; with FAKE_GATE_FAIL=1 it is blocked at the dry run and reports
#     the failed gate; --self-test-shrink reaches ShrinkDone, and a gate failure is an error;
#  3. real backend: --self-test-dualboot /dev/nvme0n1 reaches page Done;
#  4. on the host: check-dualboot-fixture.sh --installed passes.
# Env: GUI_BIN (default m6-session-bins/out/leandros-installer-<arch>), LEANDROS_INSTALL_BIN, FX_DIR.
. "$(cd "$(dirname "$0")" && pwd)/lib/clonecp.sh"
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
# QEMU-only test: the stick needs no Intel iris/ANV ship-set (write-usb.sh refuses otherwise)
export LEANDROS_USB_ALLOW_GENERIC_GPU="${LEANDROS_USB_ALLOW_GENERIC_GPU:-1}"
ARCH="${1:-aarch64}"
case "$ARCH" in aarch64|x86_64) ;; *) echo "usage: $0 [aarch64|x86_64]"; exit 2 ;; esac
FXDIR="${FX_DIR:-$HOME/code/leandros-artifacts/dualboot}"
WORK="$ROOT/target/dualboot-gui-test-$ARCH"
IMG="$WORK/disk.img"
RUN_ID="dbgui-$ARCH"
PORT=$((2500 + $(printf '%s' "$RUN_ID" | cksum | cut -d' ' -f1) % 60))
STATE="/tmp/leandros-$RUN_ID-ssh"
export LEANDROS_REMOTE="qemu:$RUN_ID" MTOOLS_SKIP_CHECK=1
REMOTE="python3 $ROOT/scripts/remote.py"
BIN="${LEANDROS_INSTALL_BIN:-$ROOT/userland/leandros-install/target/$ARCH-unknown-linux-musl/release/leandros-install}"
QPID=""
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }
cleanup() { [ -n "$QPID" ] && kill "$QPID" 2>/dev/null || true; rm -f "$STATE"; }
trap cleanup EXIT

boot_vm() { # <log> <run-qemu args...>
    local log="$WORK/$1.log"; shift
    echo "127.0.0.1 $PORT" > "$STATE"
    LEANDROS_NET=user LEANDROS_SSH_PORT=$PORT "$ROOT/scripts/run-qemu.sh" "$ARCH" --uefi --no-gpu "$@" -display none \
        > "$log" 2>&1 < /dev/null &
    QPID=$!
    local i
    for i in $(seq 1 150); do
        kill -0 "$QPID" 2>/dev/null || { tail -5 "$log"; fail "QEMU exited early ($log)"; }
        if $REMOTE run true >/dev/null 2>&1; then ok "ssh up (${i}x2s) $log"; return 0; fi
        sleep 2
    done
    fail "no ssh within 300 s ($log)"
}
stop_vm() {
    $REMOTE run 'sync; poweroff' >/dev/null 2>&1 || true
    local i
    for i in $(seq 1 60); do kill -0 "$QPID" 2>/dev/null || { QPID=""; return 0; }; sleep 1; done
    kill "$QPID" 2>/dev/null || true; QPID=""
}
r() { $REMOTE run "$1" | tr -d '\r'; }

. "$(dirname "${BASH_SOURCE[0]}")/lib/m6out.sh"
GUI_BIN="${GUI_BIN:-$M6_OUT/leandros-installer-$ARCH}"; [ -e "$GUI_BIN" ] || GUI_BIN="$M6_OUT_SHARED/leandros-installer-$ARCH"
FAKE="$ROOT/ports/leandros-installer/test/fake-backend.sh"
echo "== images"
rm -rf "$WORK"; mkdir -p "$WORK"
SRC="$FXDIR/fxgap-$ARCH.img"
[ -f "$SRC" ] || fail "missing $SRC"
df -Pk "$HOME" | awk 'NR==2 && $4 < 30*1048576 { exit 1 }' || fail "less than 30 GiB free"
for f in "$SRC"*; do clone_cp "$f" "$WORK/$(basename "$f" | sed "s/fxgap-$ARCH/disk/")"; done
STICK="$WORK/stick.img"
"$ROOT/scripts/write-usb.sh" "$ARCH" --image "$STICK" --gpt >"$WORK/write-usb.log" 2>&1 || { tail -5 "$WORK/write-usb.log"; fail "write-usb.sh"; }

echo "== boot the stick with the fixture as an NVMe target"
LEANDROS_USB_IMAGE="$STICK" boot_vm stick --usb-stick=gpt --install-target="$IMG,nvme"
[ -f "$BIN" ] && { $REMOTE push "$BIN" /tmp/leandros-install >/dev/null; r 'chmod +x /tmp/leandros-install'; }
$REMOTE push "$GUI_BIN" /tmp/leandros-installer >/dev/null; r 'chmod +x /tmp/leandros-installer'
$REMOTE push "$FAKE" /tmp/fake-backend.sh >/dev/null; r 'chmod +x /tmp/fake-backend.sh'
G='LEANDROS_INSTALL_BIN=/tmp/fake-backend.sh FAKE_DELAY=0 /tmp/leandros-installer'

echo "== fake backend"
r "$G --self-test-dualboot /dev/nvme0n1" > "$WORK/fake-db.log"; grep -q "page Done" "$WORK/fake-db.log" && grep -q "new=6" "$WORK/fake-db.log" || { tail -5 "$WORK/fake-db.log"; fail "fake dualboot did not reach Done with a plan"; }
ok "fake: dual-boot install reaches Done (gates, plan, steps)"
r "FAKE_GATE_FAIL=1 $G --self-test-dualboot /dev/nvme0n1; echo rc=\$?" > "$WORK/fake-gate.log"
grep -q "G8 power fail" "$WORK/fake-gate.log" && grep -q "blocked" "$WORK/fake-gate.log" && grep -q "rc=1" "$WORK/fake-gate.log" || { tail -5 "$WORK/fake-gate.log"; fail "gate failure not reported"; }
ok "fake: gate failure blocks the install"
r "$G --self-test-reinstall /dev/nvme0n1" > "$WORK/fake-re.log"; grep -q "page Done" "$WORK/fake-re.log" || { tail -5 "$WORK/fake-re.log"; fail "fake reinstall did not reach Done"; }
ok "fake: reinstall reaches Done"
r "FAKE_GATE_FAIL=1 $G --self-test-reinstall /dev/nvme0n1; echo rc=\$?" | grep -q "rc=1" && ok "fake: reinstall gate failure blocks" || fail "fake reinstall gate failure"
r "$G --self-test-shrink /dev/nvme1n1 120G" > "$WORK/fake-shrink.log"; grep -q "page ShrinkDone" "$WORK/fake-shrink.log" || { tail -5 "$WORK/fake-shrink.log"; fail "fake shrink"; }
ok "fake: shrink reaches ShrinkDone"
r "FAKE_GATE_FAIL=1 $G --self-test-shrink /dev/nvme1n1 120G; echo rc=\$?" | grep -q "rc=1" && ok "fake: shrink gate failure is an error" || fail "shrink gate failure"

r "rm -f /tmp/fake-recovered; FAKE_INTERRUPTED=1 $G --self-test-recover; echo rc=\$?" > "$WORK/fake-recover.log"
grep -q "banner interrupted phase=fscking" "$WORK/fake-recover.log" && grep -q "banner cleared" "$WORK/fake-recover.log" && grep -q "rc=0" "$WORK/fake-recover.log" || { tail -6 "$WORK/fake-recover.log"; fail "fake recover flow"; }
ok "fake: interrupted shrink -> banner -> Recover -> cleared"

echo "== real backend"
r "LEANDROS_INSTALL_BIN=/tmp/leandros-install /tmp/leandros-installer --self-test-dualboot /dev/nvme0n1" > "$WORK/real-db.log" || { tail -8 "$WORK/real-db.log"; fail "real dualboot self-test"; }
grep -q "page Done" "$WORK/real-db.log" || fail "real: no page Done"
ok "real: GUI dual-boot path reached Done"
r "LEANDROS_INSTALL_BIN=/tmp/leandros-install /tmp/leandros-installer --self-test-list" | grep -q '"reinstall":true\|reinstall' || true
r "LEANDROS_INSTALL_BIN=/tmp/leandros-install /tmp/leandros-installer --self-test-reinstall /dev/nvme0n1" > "$WORK/real-re.log" || { tail -8 "$WORK/real-re.log"; fail "real reinstall self-test"; }
grep -q "page Done" "$WORK/real-re.log" || fail "real: reinstall did not reach Done"
ok "real: GUI reinstall path reached Done"
stop_vm

echo "== host checks"
"$ROOT/scripts/check-dualboot-fixture.sh" "$IMG" --installed | tee "$WORK/check.log" | grep -E "^FAIL" || true
grep -q "^RESULT installed fail=0" "$WORK/check.log" || fail "check-dualboot-fixture.sh --installed"
ok "check-dualboot-fixture.sh --installed"
rm -f "$IMG" "$STICK"
echo "PASS: dualboot-gui-test $ARCH"
