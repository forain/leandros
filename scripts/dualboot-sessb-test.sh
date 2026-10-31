#!/usr/bin/env bash
# dualboot-sessb-test.sh [aarch64|x86_64]  -- dual-boot v1b session B (finish after the ext4 shrink) in QEMU.
#
# ONE fixture copy (fx-<arch>, cp -c) goes through the whole story:
#   1. refusals before anything happened: no shrink session;
#   2. session A: shrink --part /dev/nvme0n1p2 --to 10G;
#   3. refusal: Linux was NOT booted between the sessions (GPT unchanged);
#   4. Linux boots the fixture in place once (LINUX-DUALBOOT-OK; sets the mount time);
#   5. refusal: the filesystem size does not match session A's record (state tampered);
#   6. session B through the GUI self-test against the real backend (dry run first);
#   7. host: check-dualboot-fixture.sh --installed --sessb (p2 start/GUID/attributes
#      unchanged, only its end moved), e2fsck clean and manifest OK are part of it;
#   8. Linux still boots by default; LeandrOS boots from the menu (scratch loader.conf
#      default) with blk.rw= on the command line.
# Disk: one 48 GiB APFS clone, deleted at the end. Env: FX_DIR, LEANDROS_INSTALL_BIN, GUI_BIN.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
ARCH="${1:-aarch64}"
case "$ARCH" in aarch64|x86_64) ;; *) echo "usage: $0 [aarch64|x86_64]"; exit 2 ;; esac
FXDIR="${FX_DIR:-$HOME/code/leandros-artifacts/dualboot}"
SRC="$FXDIR/fx-$ARCH.img"
WORK="$ROOT/target/dualboot-sessb-test-$ARCH"
IMG="$WORK/disk.img"; STICK="$WORK/stick.img"
RUN_ID="dbsessb-$ARCH"
PORT=$((2500 + $(printf '%s' "$RUN_ID" | cksum | cut -d' ' -f1) % 60))
STATE="/tmp/leandros-$RUN_ID-ssh"
export LEANDROS_REMOTE="qemu:$RUN_ID" MTOOLS_SKIP_CHECK=1
REMOTE="python3 $ROOT/scripts/remote.py"
BIN="${LEANDROS_INSTALL_BIN:-$ROOT/userland/leandros-install/target/$ARCH-unknown-linux-musl/release/leandros-install}"
GUI_BIN="${GUI_BIN:-$ROOT/ports/leandros-installer/target/$ARCH-unknown-linux-musl/release/leandros-installer}"
LI=/tmp/leandros-install
QPID=""
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }
cleanup() { [ -n "$QPID" ] && kill "$QPID" 2>/dev/null || true; rm -f "$STATE"; rm -f "${WORK:?}"/disk.img* "${WORK:?}"/stick.img* "${WORK:?}"/scratch.img*; }
trap cleanup EXIT
r() { $REMOTE run "$1" | tr -d '\r'; }
dfguard() { local g; g=$(df -g "$HOME" | awk 'NR==2{print $4}'); [ "$g" -ge 25 ] || fail "only ${g} GiB free on the host; stop"; }

boot_vm() { # <log> <run-qemu args...>
    local log="$WORK/$1.log"; shift
    echo "127.0.0.1 $PORT" > "$STATE"
    LEANDROS_NET=user LEANDROS_SSH_PORT=$PORT "$ROOT/scripts/run-qemu.sh" "$ARCH" --uefi --no-gpu "$@" -display none \
        > "$log" 2>&1 < /dev/null &
    QPID=$!
    local i
    for i in $(seq 1 150); do
        kill -0 "$QPID" 2>/dev/null || { tail -5 "$log"; fail "QEMU exited early ($log)"; }
        if $REMOTE run true >/dev/null 2>&1; then ok "ssh up ($log)"; return 0; fi
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
boot_stick() {
    LEANDROS_USB_IMAGE="$STICK" boot_vm "$1" --usb-stick=gpt --install-target="$IMG,nvme"
    $REMOTE push "$BIN" $LI >/dev/null; r "chmod +x $LI"
    $REMOTE push "$GUI_BIN" /tmp/leandros-installer >/dev/null; r 'chmod +x /tmp/leandros-installer'
}
gpt_sig() { { dd if="$1" bs=512 count=34 2>/dev/null; dd if="$1" bs=512 skip=$((51539607552 / 512 - 33)) 2>/dev/null; } | shasum -a 256 | cut -c1-64; }
B_ARGS="install --mode dualboot --target /dev/nvme0n1 --after-shrink --yes --json"

echo "== images (one fixture copy)"
[ -f "$SRC" ] || fail "missing $SRC"
[ -x "$GUI_BIN" ] || fail "missing GUI binary $GUI_BIN"
mkdir -p "$WORK"; rm -f "$WORK"/disk.img* "$WORK"/stick.img*
dfguard
for f in "$SRC"*; do cp -c "$f" "$WORK/$(basename "$f" | sed "s/fx-$ARCH/disk/")"; done
GPT0=$(gpt_sig "$IMG")
# Original p2 entry (GUID + attribute flags) straight from the primary GPT.
P2ENT=$(python3 - "$IMG" <<'PY'
import sys
f = open(sys.argv[1], "rb"); f.seek(2 * 512 + 128); e = f.read(128)
print(e[16:32].hex(), e[32:40].hex(), int.from_bytes(e[48:56], "little"))
PY
)
P2ATTRS=$(printf '%016X' "$(echo "$P2ENT" | cut -d' ' -f3)")
"$ROOT/scripts/write-usb.sh" "$ARCH" --image "$STICK" --gpt >"$WORK/write-usb.log" 2>&1 || { tail -5 "$WORK/write-usb.log"; fail "write-usb.sh"; }

echo "== no session yet"
boot_stick sess1
r "$LI $B_ARGS --dry-run; true" | grep -q 'no shrink session' && ok "session B refused: no shrink session" || fail "no-session refusal"
[ "$(gpt_sig "$IMG")" = "$GPT0" ] && ok "GPT untouched" || fail "GPT changed"

echo "== session A: shrink to 10 GiB"
r "$LI shrink --part /dev/nvme0n1p2 --to 10G --yes --json; true" > "$WORK/shrink.log"
grep -q '"ev":"done","part"' "$WORK/shrink.log" || { tail -4 "$WORK/shrink.log"; fail "shrink did not finish"; }
r "$LI shrink-status --json" | tee "$WORK/status1.log" | grep -q '"state":"boot_linux"' && ok "status boot_linux" || { cat "$WORK/status1.log"; fail "status after shrink"; }
[ "$(gpt_sig "$IMG")" = "$GPT0" ] && ok "session A left the GPT byte-identical" || fail "GPT changed in session A"

echo "== refusal: Linux was not booted between the sessions"
r "$LI $B_ARGS --dry-run; true" > "$WORK/nb.log"
grep -q '"id":"B1".*"status":"fail".*has not booted' "$WORK/nb.log" && ok "dry run refused: Linux not booted" || { tail -3 "$WORK/nb.log"; fail "not-booted refusal"; }
r "$LI $B_ARGS; true" | grep -q '"ev":"done"' && fail "real run accepted without a Linux boot"
[ "$(gpt_sig "$IMG")" = "$GPT0" ] && ok "GPT untouched after the refusals" || fail "GPT changed by a refused run"
stop_vm

echo "== boot Linux on the fixture once (in place)"
FX_BOOT_INPLACE=1 "$ROOT/scripts/check-dualboot-fixture.sh" "$IMG" --boot "$ARCH" | tail -3 | grep -q "PASS boot" && ok "LINUX-DUALBOOT-OK on the shrunk ext4" || fail "Linux did not boot the shrunk fixture"

boot_stick sess2
r "$LI shrink-status --json" | tee "$WORK/status2.log" | grep -q '"state":"ready_b"' && ok "status ready_b (mount time moved)" || { cat "$WORK/status2.log"; fail "Linux boot not detected"; }
echo "== refusal: filesystem size differs from session A's record"
BK=$(r "cat /home/dualboot-backup/shrink-latest")
r "cp $BK/state $BK/state.orig && grep -v '^new_blocks=' $BK/state.orig > $BK/state.new && echo new_blocks=2621441 >> $BK/state.new && cp $BK/state.new $BK/state && sync"
r "$LI $B_ARGS --dry-run; true" > "$WORK/mm.log"
grep -q '"id":"B1".*"status":"fail".*recorded' "$WORK/mm.log" && ok "dry run refused: size mismatch" || { tail -3 "$WORK/mm.log"; fail "size mismatch refusal"; }
r "cp $BK/state.orig $BK/state && sync"
[ "$(gpt_sig "$IMG")" = "$GPT0" ] && ok "GPT untouched" || fail "GPT changed"

echo "== session B: dry run, then the GUI self-test (real backend)"
r "$LI $B_ARGS --dry-run" > "$WORK/dry.log"
grep -q '"ev":"shrink_gpt"' "$WORK/dry.log" && grep -q '"ev":"plan".*"new_gpt"' "$WORK/dry.log" && grep -q '"dry_run":true' "$WORK/dry.log" || { tail -3 "$WORK/dry.log" | cut -c1-200; fail "dry run lacks shrink_gpt/plan"; }
grep '"ev":"shrink_gpt"' "$WORK/dry.log" | cut -c1-160
[ "$(gpt_sig "$IMG")" = "$GPT0" ] && ok "dry run wrote nothing" || fail "dry run changed the GPT"
r "LEANDROS_INSTALL_BIN=$LI /tmp/leandros-installer --self-test-after-shrink /dev/nvme0n1" > "$WORK/gui.log" || { tail -8 "$WORK/gui.log"; fail "GUI self-test"; }
grep -q "page Done" "$WORK/gui.log" || { tail -8 "$WORK/gui.log"; fail "GUI: no page Done"; }
ok "GUI: Finish installation reached Done"
r "$LI shrink-status --json" | grep -q '"state":"none"' && ok "status none after session B" || fail "status after session B"
stop_vm

echo "== host checks"
"$ROOT/scripts/check-dualboot-fixture.sh" "$IMG" --installed --sessb "$P2ATTRS" | tee "$WORK/check.log" | grep -E "^(FAIL|RESULT)" || true
grep -q "^RESULT installed fail=0" "$WORK/check.log" || fail "check-dualboot-fixture.sh --installed --sessb"
ok "installed layout, e2fsck clean, manifest OK, p2 start/GUID/attrs unchanged"

echo "== Linux still boots by default"
"$ROOT/scripts/check-dualboot-fixture.sh" "$IMG" --boot "$ARCH" | tail -3 | grep -q "PASS boot" && ok "LINUX-DUALBOOT-OK" || fail "Linux boot after session B"

echo "== choose LeandrOS (scratch copy)"
dfguard
SC="$WORK/scratch.img"; cp -c "$IMG" "$SC"; for x in meta manifest esp.sha256; do cp -c "$IMG.$x" "$SC.$x"; done
ESP="$SC@@2M"
printf 'default leandros.conf\ntimeout 1\nconsole-mode keep\n' > "$WORK/loader.conf.test"
mcopy -o -i "$ESP" "$WORK/loader.conf.test" ::/loader/loader.conf
boot_vm leandros --boot-disk="$SC,nvme"
CMD=$(r 'cat /proc/cmdline'); echo "$CMD" | cut -c1-240
echo "$CMD" | grep -q 'blk.rw=' && ok "blk.rw= on the cmdline" || fail "blk.rw= missing"
r 'cat /proc/mounts' | grep -Eq '^/dev/nvme0n1p5 / ' && ok "root is /dev/nvme0n1p5" || fail "root not on p5"
o=$(r "dd if=/dev/zero of=/dev/nvme0n1p2 bs=512 count=1 conv=notrunc 2>&1; echo rc=\$?")
echo "$o" | grep -qi 'read-only file system\|EROFS' && ok "write to p2: EROFS" || { echo "$o"; fail "write to p2 not refused"; }
stop_vm
echo "PASS: dualboot-sessb-test $ARCH"
