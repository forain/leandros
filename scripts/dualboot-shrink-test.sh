#!/usr/bin/env bash
# dualboot-shrink-test.sh [aarch64]  -- dual-boot v1b session A (ext4 shrink) in QEMU.
#
#  (a) shrink p2 of a fx-aarch64 clone (NVMe target of the LeandrOS stick) to 10 GiB;
#      host: e2fsck -fn clean, manifest OK, GPT byte-identical, block count; the
#      fixture alone boots Linux (LINUX-DUALBOOT-OK); shrink-status flow.
#  (b) crash injection: kill -9 of qemu mid-resize2fs at three progress points, reboot
#      the stick, `shrink --recover`; p2 must be byte-identical to the pre-image (superblock
#      restored from sb.orig), fsck-clean, manifest intact.
#  (c) every refusal gate once (superblock gates on small ext4 files edited with
#      debugfs; hibernated swap, target size, stick space on the real fixture).
#
# Disk: every 48 GiB image is an APFS clone (cp -c) of $FX_DIR; one scratch disk is
# reused and the images are deleted at the end (logs stay).
# Env: FX_DIR, LEANDROS_INSTALL_BIN, CRASH_PCTS="10 40 75".
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
ARCH="${1:-aarch64}"
[ "$ARCH" = aarch64 ] || { echo "only aarch64 is wired here"; exit 2; }
FXDIR="${FX_DIR:-$HOME/code/leandros-artifacts/dualboot}"
SRC="$FXDIR/fx-$ARCH.img"
WORK="$ROOT/target/dualboot-shrink-test-$ARCH"
RUN_ID="dbshrink-$ARCH"
PORT=$((2500 + $(printf '%s' "$RUN_ID" | cksum | cut -d' ' -f1) % 60))
STATE="/tmp/leandros-$RUN_ID-ssh"
export LEANDROS_REMOTE="qemu:$RUN_ID" MTOOLS_SKIP_CHECK=1
REMOTE="python3 $ROOT/scripts/remote.py"
BIN="${LEANDROS_INSTALL_BIN:-$ROOT/userland/leandros-install/target/aarch64-unknown-linux-musl/release/leandros-install}"
CRASH_PCTS="${CRASH_PCTS:-10 40 75}"
PHASES="${PHASES:-f c a b d}"   # subset, e.g. PHASES=b CRASH_PCTS=40
phase() { case " $PHASES " in *" $1 "*) return 0;; esac; echo "-- phase $1 skipped (PHASES=$PHASES)"; return 1; }
DISK="$WORK/disk.img"; STICK="$WORK/stick.img"; QMP="$WORK/qmp.sock"
QPID=""
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }
rm_images() { [ "${KEEP_IMAGES:-0}" = 1 ] && return 0; rm -f "${WORK:?}/disk.img" "${WORK:?}"/disk.img.* "${WORK:?}/stick.img" "${WORK:?}/stick.base"; }
cleanup() { [ -n "$QPID" ] && kill "$QPID" 2>/dev/null || true; rm -f "$STATE" "$QMP"; rm_images; }
trap cleanup EXIT
r() { $REMOTE run "$1" | tr -d '\r'; }
LI=/tmp/leandros-install
P2=/dev/nvme0n1p2

dfguard() { local g; g=$(df -g "$HOME" | awk 'NR==2{print $4}'); [ "$g" -ge 30 ] || fail "only ${g} GiB free on the host; stop"; }
fresh_disk() { # cp -c clones only (APFS), never a plain copy
    dfguard; rm -f "${DISK:?}" "${DISK:?}".*
    local f; for f in "$SRC"*; do cp -c "$f" "$WORK/$(basename "$f" | sed "s/fx-$ARCH/disk/")"; done
}
fresh_stick() { dfguard; rm -f "${STICK:?}"; cp -c "$WORK/stick.base" "$STICK"; }

boot_vm() { # <log> <run-qemu args...>
    local log="$WORK/$1.log"; shift
    echo "127.0.0.1 $PORT" > "$STATE"; rm -f "$QMP"
    LEANDROS_NET=user LEANDROS_SSH_PORT=$PORT "$ROOT/scripts/run-qemu.sh" "$ARCH" --uefi --no-gpu "$@" -display none \
        -qmp "unix:$QMP,server=on,wait=off" > "$log" 2>&1 < /dev/null &
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
qmp_quit() { # abrupt end of the VM: no guest sync, no shutdown. kill -9 of the
    # qemu process (found by its unique QMP socket path); QMP "quit" if that fails.
    if pkill -9 -f "[q]emu-system.*$QMP" 2>/dev/null; then
        local j; for j in $(seq 1 30); do pgrep -f "[q]emu-system.*$QMP" >/dev/null || break; sleep 1; done
        kill -9 "$QPID" 2>/dev/null || true; QPID=""; return 0
    fi
    echo "(kill -9 of qemu not possible; using QMP quit)"
    python3 - "$QMP" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.recv(4096)
s.sendall(b'{"execute":"qmp_capabilities"}\n'); s.recv(4096)
s.sendall(b'{"execute":"quit"}\n')
try: s.recv(4096)
except Exception: pass
PY
    local i; for i in $(seq 1 30); do kill -0 "$QPID" 2>/dev/null || { QPID=""; return 0; }; sleep 1; done
    kill -9 "$QPID" 2>/dev/null || true; QPID=""
}
boot_stick() { LEANDROS_USB_IMAGE="$STICK" boot_vm "$1" --usb-stick=gpt --install-target="$DISK,nvme"; [ -f "$BIN" ] && { $REMOTE push "$BIN" $LI >/dev/null; r "chmod +x $LI"; }; return 0; }

gpt_sig() { { dd if="$1" bs=512 count=34 2>/dev/null; dd if="$1" bs=512 skip=$((51539607552 / 512 - 33)) 2>/dev/null; } | shasum -a 256 | cut -c1-64; }
p2_compare() { # $1 pre-image, $2 candidate: number of differing 1 MiB chunks of p2
    python3 - "$1" "$2" <<'PY'
import sys
a, b = open(sys.argv[1], "rb"), open(sys.argv[2], "rb")
off, n = 2101248 * 512, (96454621 - 2101248 + 1) * 512
a.seek(off); b.seek(off); diff = []; i = 0
while n > 0:
    k = min(n, 1 << 20); x, y = a.read(k), b.read(k)
    if x != y: diff.append(i)
    i += 1; n -= k
print(len(diff), diff[:8])
PY
}

echo "== images"
mkdir -p "$WORK"; rm_images
[ -f "$SRC" ] || fail "missing $SRC"
GPT0=$(gpt_sig "$SRC")
"$ROOT/scripts/write-usb.sh" "$ARCH" --image "$WORK/stick.base" --gpt >"$WORK/write-usb.log" 2>&1 || { tail -5 "$WORK/write-usb.log"; fail "write-usb.sh"; }

if phase f; then
echo "== (f) e2fsck -fn x${FSCK_LOOPS:-20} on the Linux root (threaded bitmap reads, one fd)"
fresh_disk; fresh_stick
boot_stick fsckloop
bad=0
for i in $(seq 1 "${FSCK_LOOPS:-20}"); do
    out=$(r "e2fsck -fn $P2 2>&1; echo rc=\$?" | tail -4)
    echo "$out" | grep -q 'rc=0' || { bad=$((bad+1)); echo "$out"; }
done
[ "$bad" = 0 ] && ok "e2fsck -fn rc 0 x${FSCK_LOOPS:-20}" || fail "$bad of ${FSCK_LOOPS:-20} e2fsck runs not clean"
stop_vm
fi

if phase c; then
echo "== (c) refusal gates"
fresh_disk; fresh_stick
python3 - "$DISK" <<'PY'
import sys
f = open(sys.argv[1], "r+b"); f.seek(96454622 * 512 + 4086); f.write(b"S1SUSPEND\0"); f.close()
PY
GPT1=$(gpt_sig "$DISK")
boot_stick gates
r "$LI shrink --part $P2 --to 10G --dry-run --json 2>&1; true" > "$WORK/gate-hib.log"
grep -q '"id":"S7","name":"swap","status":"fail"' "$WORK/gate-hib.log" && ok "S7 hibernated swap refused" || { tail -5 "$WORK/gate-hib.log"; fail "S7"; }
r "$LI shrink --part $P2 --to 3G --dry-run --json; true" > "$WORK/gate-size.log"
grep -q '"id":"S8","name":"target-size","status":"fail"' "$WORK/gate-size.log" && ok "S8 target below minimum+2GiB refused" || fail "S8"
r "env LEANDROS_SHRINK_FREE_BYTES=1000000 $LI shrink --part $P2 --to 10G --dry-run --json 2>&1; true" > "$WORK/gate-space.log"
grep -q '"id":"S9","name":"stick-space","status":"fail"' "$WORK/gate-space.log" && ok "S9 stick space refused" || fail "S9"
r "$LI shrink --part $P2 --to 10G --json; true" | grep -q '"ev":"error"' && ok "no --yes: refused" || fail "no --yes accepted"
r "cd /tmp && for v in base dirty state errors orphan mmp unknown; do rm -f g-\$v.img; mke2fs -q -t ext4 -F g-\$v.img 64M; done
 debugfs -w -R 'feature needs_recovery' g-dirty.img >/dev/null 2>&1
 debugfs -w -R 'ssv state 0' g-state.img >/dev/null 2>&1
 debugfs -w -R 'ssv error_count 3' g-errors.img >/dev/null 2>&1
 debugfs -w -R 'ssv last_orphan 12' g-orphan.img >/dev/null 2>&1
 debugfs -w -R 'ssv mmp_block 100' g-mmp.img >/dev/null 2>&1
 debugfs -w -R 'ssv feature_incompat 0x80000' g-unknown.img >/dev/null 2>&1
 true"
r "$LI shrink --check-sb /tmp/g-base.img; echo rc=\$?" | grep -q 'rc=0' && ok "clean superblock passes the gates" || fail "base fixture refused"
for pair in dirty:S2 state:S1 errors:S3 orphan:S4 mmp:S5 unknown:S6; do
    v=${pair%%:*}; g=${pair##*:}
    r "$LI shrink --check-sb /tmp/g-$v.img --json; true" | grep -q "\"id\":\"$g\"" && ok "$g ($v) refused" || fail "gate $g ($v) not triggered"
done
stop_vm
[ "$(gpt_sig "$DISK")" = "$GPT1" ] && ok "gate runs left the GPT untouched" || fail "GPT changed during gate tests"

fi
if phase a; then
echo "== (a) shrink to 10 GiB"
fresh_disk; fresh_stick
boot_stick shrink
r "$LI shrink --part $P2 --to 10G --dry-run --json 2>&1; true" > "$WORK/dry.log"
grep -q '"ev":"done","dry_run":true' "$WORK/dry.log" && ok "dry run" || { tail -3 "$WORK/dry.log"; fail "dry run"; }
grep '"ev":"shrink_info"' "$WORK/dry.log" | head -1 | cut -c1-220
r "$LI shrink-status --json" | grep -q '"state":"none"' && ok "status none before"
r "$LI shrink --part $P2 --to 10G --yes --json 2>&1; true" > "$WORK/shrink.log"
grep -q '"ev":"done","part"' "$WORK/shrink.log" || { tail -5 "$WORK/shrink.log"; fail "shrink did not finish"; }
for s in backup rehearse hash fsck resize verify; do grep -q "\"name\":\"$s\",\"pct\":100" "$WORK/shrink.log" || fail "step $s did not finish"; done
ok "all steps finished; $(grep -c '"name":"resize"' "$WORK/shrink.log") resize progress events"
ST=$(r "$LI shrink-status --json; true")
echo "$ST" | grep -q '"state":"boot_linux"' && ok "status boot_linux" || { echo "$ST" | tail -3; fail "status after shrink"; }
BK=$(r "cat /home/dualboot-backup/shrink-latest")
r "ls -l $BK | cut -c1-90; cd $BK && sha256sum -c SHA256SUMS | tail -3" | tail -14
stop_vm
echo "-- host checks"
[ "$(gpt_sig "$DISK")" = "$GPT0" ] && ok "GPT byte-identical" || fail "GPT changed"
"$ROOT/scripts/check-dualboot-fixture.sh" "$DISK" --shrunk | tee "$WORK/check-shrunk.log" | grep -E "^(FAIL|RESULT)|block count|sha256"
grep -q "^RESULT pristine fail=0" "$WORK/check-shrunk.log" || fail "check-dualboot-fixture.sh --shrunk"
grep -q "block count 2621440" "$WORK/check-shrunk.log" && ok "ext4 is 10 GiB" || fail "block count"
echo "-- Linux boots the shrunk fixture"
"$ROOT/scripts/check-dualboot-fixture.sh" "$DISK" --boot "$ARCH" | tail -3 | grep -q "PASS boot" && ok "LINUX-DUALBOOT-OK" || fail "Linux boot after the shrink"
# status flow: the stick that did the shrink still holds the session
cp -c "$STICK" "$WORK/stick.done" 2>/dev/null || true
boot_stick status
r "$LI shrink-status --mark-verified --json" | grep -q '"state":"ready_b"' && ok "mark-verified -> ready_b" || fail "mark-verified"
stop_vm
rm -f "${WORK:?}/stick.done"

fi
if phase b; then
echo "== (b) crash injection"
crash_case() {
    local PCT=$1 got REQ=$1
    # resize2fs -p reports pct 0..60 (passes 1-3, 40 X each of 200) and nothing during passes 4-5
    # before the installer's own 100. So a requested point above 60 means "the last progress
    # event resize2fs emits before completion" (60); the kill must still land before "done".
    [ "$PCT" -gt "${LAST_PROGRESS_PCT:-60}" ] && PCT=${LAST_PROGRESS_PCT:-60}
    [ "$PCT" = "$REQ" ] || echo "(requested ${REQ}% clamped to the last pre-completion progress event, ${PCT}%)"
    echo "-- crash at resize >= ${PCT}%"
    fresh_disk; fresh_stick
    boot_stick "crash$PCT"
    $REMOTE run "rm -f /tmp/s.log; LEANDROS_SHRINK_FREEZE_PCT=$PCT nohup $LI shrink --part $P2 --to 10G --yes --json > /tmp/s.log 2>&1 &" >/dev/null 2>&1 || true
    got=""
    # The installer freezes resize2fs (SIGSTOP) the instant its progress reaches PCT
    # (LEANDROS_SHRINK_FREEZE_PCT) and emits a "frozen" step; the host then kill -9s qemu.
    # PCT = the resize step pct (0..99) derived from resize2fs -p passes (40 X per pass, 5 passes).
    for i in $(seq 1 900); do
        f=$(r "grep -o '\"name\":\"frozen\",\"pct\":[0-9]*' /tmp/s.log 2>/dev/null | tail -1; true" || true); f=${f##*:}
        [ -n "$f" ] && { got=$f; break; }
        r "grep -q '\"ev\":\"error\"\\|\"ev\":\"done\"' /tmp/s.log" >/dev/null 2>&1 && { r "tail -c 600 /tmp/s.log; true"; fail "shrink ended before the crash point"; }
        sleep 1
    done
    [ -n "$got" ] || fail "resize never reached ${PCT}%"
    qmp_quit; ok "QEMU quit at resize ${got}%"
    boot_stick "recover$PCT"
    r "$LI shrink-status --json" > "$WORK/statout$PCT.log"
    if grep -q '"phase":"done"' "$WORK/statout$PCT.log"; then echo "shrink finished before the kill landed (resize is fast); retrying"; stop_vm; return 2; fi
    grep -q '"state":"interrupted"' "$WORK/statout$PCT.log" && ok "status: interrupted" || { cat "$WORK/statout$PCT.log"; fail "status after crash"; }
    r "$LI shrink --recover --yes --json; true" > "$WORK/recout$PCT.log"
    r "ls -la /home/dualboot-backup/\$(cat /home/dualboot-backup/shrink-latest 2>/dev/null | xargs basename 2>/dev/null) 2>/dev/null; ls -la \$(cat /home/dualboot-backup/shrink-latest); true" > "$WORK/bkls$PCT.log"
    grep -q '"recovered":true' "$WORK/recout$PCT.log" || { tail -4 "$WORK/recout$PCT.log"; fail "recover at ${PCT}%"; }
    ok "recovered"
    stop_vm
    [ "${KEEP_IMAGES:-0}" = 1 ] && { cp -c "$DISK" "$WORK/after$PCT.img"; cp -c "$STICK" "$WORK/stickafter$PCT.img"; }
    "$ROOT/scripts/check-dualboot-fixture.sh" "$DISK" | tee "$WORK/check-crash$PCT.log" | grep -E "^(FAIL|RESULT)"
    grep -q "^RESULT pristine fail=0" "$WORK/check-crash$PCT.log" || fail "post-recovery fixture check (crash $PCT)"
    [ "$(gpt_sig "$DISK")" = "$GPT0" ] && ok "GPT identical" || fail "GPT changed"
    D=$(p2_compare "$SRC" "$DISK"); echo "p2 differing 1 MiB chunks vs the pre-image: $D"
    [ "${D%% *}" = 0 ] || fail "p2 differs from the pre-image after recovery at ${PCT}%: $D"
    ok "p2 byte-identical to the pre-image"
    ! grep -q "Corrupt" "$WORK/recout$PCT.log" || fail "recovery log has corruption messages (${PCT}%)"
    # A superblock-vs-undo-header mismatch is tolerated only when the installer's e2undo -f retry
    # succeeded AND the byte comparison above found p2 identical to the pre-image.
    if grep -q "doesn't match" "$WORK/recout$PCT.log"; then
        grep -q 'e2undo -f .*: rc 0' "$WORK/recout$PCT.log" || fail "e2undo mismatch not resolved by the -f retry (${PCT}%)"
        echo "note: e2undo superblock mismatch at ${PCT}% resolved by e2undo -f (p2 byte-identical)"
    fi
}
for PCT in $CRASH_PCTS; do
    for ATT in 1 2 3; do
        rc=0; crash_case "$PCT" || rc=$?
        [ $rc = 2 ] || break
    done
    [ $rc = 0 ] || fail "crash case ${PCT}% (attempt rc $rc)"
done

fi
if phase d; then
echo "== (d) installer-test"
./scripts/installer-test.sh aarch64 virtio 2>&1 | tail -3
fi
echo "PASS: dualboot-shrink-test $ARCH"
