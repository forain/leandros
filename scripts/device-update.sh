#!/bin/bash
# device-update.sh <laptop|pi> <expected-tag> [--via-linux USER@HOST]
#
# Deploy the kernel built in THIS tree to the laptop or the Pi, boot it once,
# verify it, and promote it, all inside ONE lock3.sh hold (the wrong-promote
# rule: verify the trial id + uname and promote in the same hold).
#
#   laptop: remote.py deploy-kernel --reboot [--via-linux ...]  (needs LEANDROS_REMOTE)
#           build: ./scripts/build-all.sh --arch x86_64
#   pi:     remote.py pi-deploy (kexec)   (LEANDROS_REMOTE=pi, LEANDROS_PI_HOST)
#           build: ./scripts/build-all.sh --arch aarch64 --rpi5   (never RPI5_USBROOT)
#
# <expected-tag> must appear in `uname -v` of the booted kernel.
# Promotes only if every check passed; otherwise exits 1 and the previous good
# stays. Never changes the default boot entry, never sets tryboot_a_b=1, never
# prints cmdlines or secrets.
#   LEANDROS_REMOTE=qemu uses the qemu lock kind and no netwatch wait (QEMU test).
#   DEVUPDATE_NETWATCH_WAIT=S overrides the netwatch observation window (laptop).
set -u
usage() { echo "usage: $0 <laptop|pi> <expected-tag> [--via-linux USER@HOST]" >&2; exit 2; }
[ $# -ge 2 ] || usage
DEV=$1; TAG=$2; shift 2
VIA=""
while [ $# -gt 0 ]; do
  case $1 in
    --via-linux) [ $# -ge 2 ] || usage; VIA=$2; shift 2;;
    *) usage;;
  esac
done
case $DEV in laptop|pi) ;; *) usage;; esac
[ -n "$TAG" ] || usage

REPO=$(cd "$(dirname "$0")/.." && pwd)
LOCK=/home/forain/code/leandros-artifacts/orch/box/lock3.sh
RM="python3 $REPO/scripts/remote.py"
OUT=$(mktemp -d "${TMPDIR:-/tmp}/device-update.XXXXXX")
fail() { echo "DEVICE-UPDATE FAILED ($DEV): $*" >&2; echo "NOT promoted; the previous good kernel stays." >&2; exit 1; }

if [ "$DEV" = pi ]; then
  export LEANDROS_REMOTE=${LEANDROS_REMOTE:-pi}
  KERNEL=$REPO/target/final-aarch64/kernel-direct; INITRD=$REPO/initrd-aarch64.cpio
  BUILD="./scripts/build-all.sh --arch aarch64 --rpi5"
else
  [ -n "${LEANDROS_REMOTE:-}" ] || fail "LEANDROS_REMOTE is not set (laptop address, or qemu)"
  KERNEL=$REPO/target/final-x86_64/kernel; INITRD=$REPO/initrd-x86_64.cpio
  BUILD="./scripts/build-all.sh --arch x86_64"
fi

# 1. build artifacts must exist and be newer than HEAD's commit time
HEADT=$(git -C "$REPO" log -1 --format=%ct)
for f in "$KERNEL" "$INITRD"; do
  [ -f "$f" ] || fail "missing $f: run $BUILD"
  [ "$(stat -c %Y "$f")" -ge "$HEADT" ] || fail "$f is older than HEAD's commit: run $BUILD"
done

# 2. re-exec once under the lock
if [ -z "${DEVUPDATE_LOCKED:-}" ]; then
  kind=$DEV
  case ${LEANDROS_REMOTE:-} in qemu*) kind=qemu;; esac
  export DEVUPDATE_LOCKED=1
  exec "$LOCK" "$kind" "$0" "$DEV" "$TAG" ${VIA:+--via-linux "$VIA"}
fi

ok() { echo "ok: $*"; }
# klog check: no panic/WDOG/oops ([WDT] countdown ok allowed); prints counts only
klog_check() {
  local cmd=$1
  $RM run "sync; $cmd" > "$OUT/klog.txt" 2>/dev/null || fail "could not read the kernel log"
  [ -s "$OUT/klog.txt" ] || fail "kernel log is empty"
  local n
  n=$(grep -av '\[WDT\] countdown ok' "$OUT/klog.txt" | grep -acE 'WDOG|[Pp][Aa][Nn][Ii][Cc]|[Oo][Oo][Pp][Ss]')
  [ "$n" = 0 ] || fail "klog has $n panic/WDOG/oops line(s) (lines: $(grep -av '\[WDT\] countdown ok' "$OUT/klog.txt" | grep -anE 'WDOG|[Pp][Aa][Nn][Ii][Cc]|[Oo][Oo][Pp][Ss]' | cut -d: -f1 | head -5 | tr '\n' ' '))"
  ok "klog clean ($(wc -l < "$OUT/klog.txt") lines)"
}
uname_check() {
  local u
  u=$($RM run 'uname -v' 2>/dev/null) || fail "ssh failed (uname -v)"
  case $u in *"$TAG"*) ok "uname -v contains tag '$TAG'";; *) fail "uname -v '$u' lacks expected tag '$TAG'";; esac
}
st() { $RM espctl status 2>/dev/null | sed -n "s/^$1=//p" | head -1; }

if [ "$DEV" = laptop ]; then
  $RM run true >/dev/null 2>&1 || fail "ssh to the laptop does not work before deploy"
  OLD_GOOD=$(st good); echo "old good: ${OLD_GOOD:-?}"
  $RM deploy-kernel --reboot ${VIA:+--via-linux "$VIA"} > "$OUT/deploy.txt" 2>&1; rc=$?
  grep -aE '^(kernel |initrd |slot id=|back after|NEW KERNEL|NOT the new|REBOOT FAILED|f2fs rf)' "$OUT/deploy.txt" | cut -c1-200
  KID=$(sed -n 's/^slot id=\([^ ]*\).*/\1/p' "$OUT/deploy.txt" | head -1)
  if [ $rc -ne 0 ]; then
    echo "recovery: if the trial reached init a power cycle boots good $OLD_GOOD; else pick 'LeandrOS good' in the Limine menu; then remote.py logs + discard (laptop-remote-debug skill)." >&2
    fail "deploy-kernel --reboot exited $rc"
  fi
  [ -n "$KID" ] || fail "no deployed slot id in deploy-kernel output"
  # verify
  $RM run true >/dev/null 2>&1 || fail "ssh failed after boot"; ok "ssh works"
  BS=$(st booted_slot); BK=$(st booted_kid)
  [ "$BS" = trial ] && [ "$BK" = "$KID" ] || fail "booted $BS $BK, deployed trial $KID"
  ok "booted trial id $BK == deployed id"
  uname_check
  klog_check "espctl klog"
  # network proof: netwatch must not reboot the trial within its window
  W=${DEVUPDATE_NETWATCH_WAIT:-180}
  case ${LEANDROS_REMOTE:-} in qemu*) W=${DEVUPDATE_NETWATCH_WAIT:-0};; esac
  if [ "$W" -gt 0 ]; then
    up=$($RM run 'cut -d. -f1 /proc/uptime' 2>/dev/null) || fail "ssh failed (uptime)"
    need=$((W + 20 - up)); [ $need -gt 0 ] && { echo "netwatch window: waiting ${need}s"; sleep $need; }
    $RM run 'test -e /tmp/.remdbg-alive' >/dev/null 2>&1 || fail "netwatch window: the trial rebooted (marker gone) or ssh lost"
    ok "no netwatch reboot within ${W}s"
  else
    echo "netwatch window skipped (W=0)"
  fi
  [ "$(st booted_kid)" = "$KID" ] || fail "trial id changed during the netwatch window"
  # promote, same hold
  $RM promote > "$OUT/promote.txt" 2>&1 || fail "promote failed"
  NEW_GOOD=$(st good)
  [ "$(st good)" = "$KID" ] || fail "after promote good=$NEW_GOOD, expected $KID"
  echo "PROMOTED: old good ${OLD_GOOD:-?} -> new good $NEW_GOOD (uname -v tag '$TAG')"
else
  EXP=$(git -C "$REPO" rev-parse --short=12 HEAD)
  $RM run true >/dev/null 2>&1 || fail "ssh to the Pi does not work before deploy"
  OLD_GOOD=$($RM run 'cat /boot/pi/good/info 2>/dev/null' 2>/dev/null | head -1)
  echo "old good: ${OLD_GOOD:-none}"
  $RM pi-deploy > "$OUT/deploy.txt" 2>&1; rc=$?
  grep -aE '^(kexec into|back after|KEXEC|WARNING)' "$OUT/deploy.txt" | cut -c1-200
  if [ $rc -ne 0 ]; then
    echo "recovery: power-cycle the Pi (firmware boots the card kernel.img), then 'remote.py pi-boot good'. Previous good: ${OLD_GOOD:-none}." >&2
    fail "pi-deploy exited $rc"
  fi
  $RM run true >/dev/null 2>&1 || fail "ssh failed after kexec"; ok "ssh works"
  SLOT=$($RM run 'cat /tmp/.pi-booted-slot 2>/dev/null' 2>/dev/null | head -1)
  TRIAL=$($RM run 'cat /boot/pi/trial/info' 2>/dev/null | head -1)
  [ "$SLOT" = trial ] || fail "booted slot '$SLOT', not trial"
  case $TRIAL in "$EXP "*) ok "booted trial id $EXP == deployed id";; *) fail "trial info '$TRIAL' != HEAD $EXP";; esac
  uname_check
  klog_check "dmesg"
  echo "--- /proc/leandros_sd"; $RM run 'cat /proc/leandros_sd' 2>&1 | head -20
  $RM pi-promote > "$OUT/promote.txt" 2>&1 || fail "pi-promote failed"
  NEW_GOOD=$($RM run 'cat /boot/pi/good/info' 2>/dev/null | head -1)
  case $NEW_GOOD in "$EXP "*) ;; *) fail "after promote good='$NEW_GOOD', expected $EXP";; esac
  echo "PROMOTED: old good ${OLD_GOOD:-none} -> new good $NEW_GOOD (uname -v tag '$TAG')"
  echo "note: the card's kernel.img is unchanged; after a power cycle run remote.py pi-boot good."
fi
rm -rf "$OUT"
