#!/usr/bin/env bash
# dualboot-reinstall-test.sh [aarch64|x86_64]  -- `install --mode dualboot --reinstall` in QEMU.
#
#  1. v1a free install onto the gap fixture; boot the installed LeandrOS once and write
#     /home/keep.txt (the persistent home);
#  2. damage the installed root on the host (garbage over main-area blocks) and snapshot:
#     GPT areas, Linux p2, home, the ESP files that are not LeandrOS's;
#  3. negative cases (clones; nothing may be written): ESP marker missing -> R4, a duplicate
#     root type (boot partition retyped) -> R3, both refused and the clone is byte-identical;
#  4. real run: dry run prints the plan and writes nothing; `--reinstall --yes` rewrites;
#  5. host asserts: GPT byte-identical, p2 + home unchanged, foreign ESP files unchanged,
#     root == the stick's root (done.root_sha256), fsck-f2fs clean, cp.next_free_nid > max
#     live nid, fixture checker passes; the target is read-only again;
#  6. boot LeandrOS (same PARTUUIDs, /home/keep.txt still there) and Linux (LINUX-DUALBOOT-OK).
# Env: FX_DIR, LEANDROS_INSTALL_BIN.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
ARCH="${1:-aarch64}"
case "$ARCH" in aarch64|x86_64) ;; *) echo "usage: $0 [aarch64|x86_64]"; exit 2 ;; esac
FXDIR="${FX_DIR:-$HOME/code/leandros-artifacts/dualboot}"
WORK="$ROOT/target/dualboot-reinstall-test-$ARCH"
IMG="$WORK/disk.img"
RUN_ID="dbreinst-$ARCH"
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
pent() { sgdisk -i "$2" "$1" | sed -n "s/^$3: *\([0-9]*\).*/\1/p"; }   # <img> <n> "First sector"|"Last sector"
hrange() { python3 -c '
import sys, hashlib
f = open(sys.argv[1], "rb"); f.seek(int(sys.argv[2]) * 512); n = int(sys.argv[3]) * 512; h = hashlib.sha256()
while n:
    b = f.read(min(n, 1 << 24)); assert b; h.update(b); n -= len(b)
print(h.hexdigest())' "$1" "$2" "$3"; }
hpart() { local a b; a=$(pent "$1" "$2" "First sector"); b=$(pent "$1" "$2" "Last sector"); hrange "$1" "$a" $((b - a + 1)); }
hgpt() { echo "$(dd if="$1" bs=1m count=1 2>/dev/null | shasum -a 256 | cut -d' ' -f1) $(dd if="$1" bs=1m skip=$(( $(stat -f %z "$1") / 1048576 - 1 )) 2>/dev/null | shasum -a 256 | cut -d' ' -f1)"; }
esp_snap() { # foreign ESP files only: names + hashes
    local d="$WORK/espsnap"; rm -rf "$d"; mkdir -p "$d"; mcopy -s -n -i "$1@@2M" :: "$d" 2>/dev/null
    (cd "$d" && find . -type f ! -path './EFI/leandros/*' ! -name leandros.conf ! -name NvVars | LC_ALL=C sort | xargs shasum -a 256) | shasum -a 256 | cut -d' ' -f1
}
use_loader() { # <img> <default entry or orig>
    local ESP="$1@@2M"
    [ -f "$WORK/loader.conf.orig" ] || mtype -i "$ESP" ::/loader/loader.conf > "$WORK/loader.conf.orig"
    if [ "$2" = orig ]; then mcopy -o -i "$ESP" "$WORK/loader.conf.orig" ::/loader/loader.conf
    else printf 'default %s\ntimeout 1\nconsole-mode keep\n' "$2" > "$WORK/loader.conf.test"; mcopy -o -i "$ESP" "$WORK/loader.conf.test" ::/loader/loader.conf; fi
}
extract_root() { # <img> <out>: the f2fs volume of p5 as a sparse file
    local a; a=$(pent "$1" 5 "First sector")
    python3 - "$1" "$a" "$2" <<'PY'
import sys, struct
img, a, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
f = open(img, "rb"); f.seek(a * 512 + 1024); sb = f.read(100)
assert struct.unpack_from("<I", sb, 0)[0] == 0xF2F52010, "p5 is not f2fs"
blocks = struct.unpack_from("<Q", sb, 36)[0]
f.seek(a * 512)
with open(out, "wb") as o:
    o.truncate(blocks * 4096)
    for i in range(blocks):
        d = f.read(4096)
        if d.count(0) != 4096: o.seek(i * 4096); o.write(d)
print(blocks * 4096)
PY
}
cursor() { python3 - "$ROOT" "$1" <<'PY'
import sys, importlib.util
root, img = sys.argv[1], sys.argv[2]
sys.path.insert(0, root + "/.claude/skills/run-leandros")
spec = importlib.util.spec_from_file_location("f2fsnidtest", root + "/.claude/skills/run-leandros/f2fsnidtest.py")
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
cur, top = m.cursor_state(img)
print(cur, top)
PY
}

echo "== images"
rm -rf "$WORK"; mkdir -p "$WORK"
SRC="$FXDIR/fxgap-$ARCH.img"
[ -f "$SRC" ] || fail "missing $SRC"
df -g "$HOME" | awk 'NR==2 && $4 < 60 { exit 1 }' || fail "less than 60 GiB free"
for f in "$SRC"*; do cp -c "$f" "$WORK/$(basename "$f" | sed "s/fxgap-$ARCH/disk/")"; done
STICK="$WORK/stick.img"
"$ROOT/scripts/write-usb.sh" "$ARCH" --image "$STICK" --gpt >"$WORK/write-usb.log" 2>&1 || { tail -5 "$WORK/write-usb.log"; fail "write-usb.sh"; }

echo "== phase 1: v1a free install"
LEANDROS_USB_IMAGE="$STICK" boot_vm stick --usb-stick=gpt --install-target="$IMG,nvme"
[ -f "$BIN" ] && { $REMOTE push "$BIN" /tmp/leandros-install >/dev/null; r 'chmod +x /tmp/leandros-install'; }
LI=/tmp/leandros-install
r "$LI install --mode dualboot --use-free --target /dev/nvme0n1 --yes --json" > "$WORK/install.log" || { tail -5 "$WORK/install.log"; fail "free install failed"; }
DONE="$(grep '"ev":"done"' "$WORK/install.log" | tail -1)"; [ -n "$DONE" ] || fail "no done event"
ROOT_PU=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['root_partuuid'])" "$DONE")
ok "installed (root PARTUUID $ROOT_PU)"
stop_vm

echo "== phase 2: boot the install, write the home file"
use_loader "$IMG" leandros.conf
boot_vm inst --boot-disk="$IMG,nvme"
r 'mount | grep -q " /home " && echo HOMEMNT' | grep -q HOMEMNT || fail "/home is not a mount in the installed system"
r 'echo reinstall-keep > /home/keep.txt && sync && cat /home/keep.txt' | grep -q reinstall-keep || fail "write /home/keep.txt"
stop_vm
use_loader "$IMG" orig
ok "home file written"

echo "== phase 3: damage the installed root, snapshot the rest"
P5=$(pent "$IMG" 5 "First sector")
python3 - "$IMG" "$P5" <<'PY'
import sys, os
img, a = sys.argv[1], int(sys.argv[2])
f = open(img, "r+b")
for mib in (3, 5, 9, 14, 20, 40):
    f.seek(a * 512 + mib * 1048576); f.write(os.urandom(262144))
PY
G0=$(hgpt "$IMG"); P2_0=$(hpart "$IMG" 2); H0=$(hpart "$IMG" 6); E0=$(esp_snap "$IMG"); R_DAMAGED=$(hpart "$IMG" 5)
cp -c "$IMG" "$WORK/pre.img"
ok "snapshots taken (gpt $G0)"

# Everything the installer could touch: the GPT areas, partitions 2-7 byte for byte, and the ESP by
# file content (OVMF writes its NvVars file and FAT bookkeeping on the first ESP it sees, so those are ignored).
fullsnap() {
    local d="$WORK/fullsnap"; rm -rf "$d"; mkdir -p "$d"; mcopy -s -n -i "$1@@2M" :: "$d" 2>/dev/null
    echo "$(hgpt "$1") $(for n in 2 3 4 5 6 7; do hpart "$1" "$n"; done | tr '\n' ' ') $(cd "$d" && find . -type f ! -name NvVars | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256)"
}
neg() { # <name> <img> <gate>
    local n=$1 img=$2 gate=$3
    LEANDROS_USB_IMAGE="$STICK" boot_vm "neg-$n" --usb-stick=gpt --install-target="$img,nvme"
    $REMOTE push "$BIN" /tmp/leandros-install >/dev/null; r 'chmod +x /tmp/leandros-install'
    r "$LI install --mode dualboot --target /dev/nvme0n1 --reinstall --yes --json; echo rc=\$?" > "$WORK/neg-$n.out"
    stop_vm
    grep -q "\"id\":\"$gate\".*\"status\":\"fail\"" "$WORK/neg-$n.out" || { tail -5 "$WORK/neg-$n.out"; fail "negative $n: gate $gate did not fail"; }
    grep -q '"ev":"done"' "$WORK/neg-$n.out" && fail "negative $n: install reported done"
    grep -q 'rc=1' "$WORK/neg-$n.out" || fail "negative $n: exit code"
    [ "$(fullsnap "$img")" = "$(cat "$WORK/neg-$n.snap")" ] || fail "negative $n: the disk was modified"
    ok "negative $n refused ($gate), nothing written"
}
echo "== phase 4: negative cases"
cp -c "$IMG" "$WORK/negA.img"; mdel -i "$WORK/negA.img@@2M" ::/EFI/leandros/.leandros-owned; fullsnap "$WORK/negA.img" > "$WORK/neg-A.snap"
neg A "$WORK/negA.img" R4
cp -c "$IMG" "$WORK/negB.img"; sgdisk -t 4:3F2D991B-D385-4FFF-9CF1-A0DB03062DE5 "$WORK/negB.img" >/dev/null; fullsnap "$WORK/negB.img" > "$WORK/neg-B.snap"
neg B "$WORK/negB.img" R3
rm -f "$WORK/negA.img" "$WORK/negB.img" "$WORK"/neg-?.snap

echo "== phase 5: dry run, then the reinstall"
LEANDROS_USB_IMAGE="$STICK" boot_vm re --usb-stick=gpt --install-target="$IMG,nvme"
$REMOTE push "$BIN" /tmp/leandros-install >/dev/null; r 'chmod +x /tmp/leandros-install'
r "$LI install --mode dualboot --target /dev/nvme0n1 --reinstall --dry-run --json" > "$WORK/dry.log"
grep -q '"ev":"done","dry_run":true' "$WORK/dry.log" || { tail -3 "$WORK/dry.log"; fail "dry run did not finish"; }
grep -q '"mode":"reinstall"' "$WORK/dry.log" && grep -q '"action":"keep"' "$WORK/dry.log" && grep -q 'EFI/leandros' "$WORK/dry.log" || fail "dry run plan lacks partitions/actions/esp files"
r "$LI install --mode dualboot --target /dev/nvme0n1 --reinstall --json; echo rc=\$?" | grep -q 'rc=1' && ok "refused without --yes" || fail "ran without --yes"
r "$LI install --mode dualboot --target /dev/nvme0n1 --reinstall --yes --json" > "$WORK/reinstall.log" || { tail -5 "$WORK/reinstall.log"; fail "reinstall failed"; }
DONE2="$(grep '"ev":"done"' "$WORK/reinstall.log" | tail -1)"; [ -n "$DONE2" ] || { tail -3 "$WORK/reinstall.log"; fail "no done event"; }
for s in backup boot log root verify esp; do grep -q "\"name\":\"$s\",\"pct\":100" "$WORK/reinstall.log" || fail "step $s did not finish"; done
grep -q '"name":"gpt"' "$WORK/reinstall.log" && fail "a gpt step ran"
grep -q '"name":"home"' "$WORK/reinstall.log" && fail "a home step ran without --wipe-home"
ok "all steps finished; no gpt/home step"
RO=$(r "lsblk 2>/dev/null | grep '^/dev/nvme0n1 ' | tr -s ' ' | cut -d' ' -f3; true"); [ "$RO" = "1" ] && ok "nvme0n1 read-only again" || fail "nvme0n1 left writable (RO=$RO)"
RSHA=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['root_sha256'])" "$DONE2")
stop_vm

echo "== phase 6: host asserts"
[ "$(hgpt "$IMG")" = "$G0" ] && ok "GPT areas byte-identical" || fail "GPT changed"
[ "$(hpart "$IMG" 2)" = "$P2_0" ] && ok "Linux p2 unchanged" || fail "Linux p2 changed"
[ "$(hpart "$IMG" 6)" = "$H0" ] && ok "home p6 unchanged" || fail "home p6 changed"
[ "$(esp_snap "$IMG")" = "$E0" ] && ok "foreign ESP files unchanged" || fail "foreign ESP files changed"
[ "$(hpart "$IMG" 5)" != "$R_DAMAGED" ] && ok "root p5 was rewritten"
BYTES=$(extract_root "$IMG" "$WORK/root.img")
[ "$(head -c "$BYTES" "$WORK/root.img" | shasum -a 256 | cut -d' ' -f1)" = "$RSHA" ] || fail "the installed root differs from the copy source (done.root_sha256)"
ok "installed root == the stick's root (sha256 $RSHA)"
python3 "$ROOT/scripts/fsck-f2fs.py" "$WORK/root.img" > "$WORK/fsck.log" 2>&1 || { tail -8 "$WORK/fsck.log"; fail "fsck-f2fs.py on the reinstalled root"; }
ok "fsck-f2fs.py: $(tail -1 "$WORK/fsck.log" | cut -c1-90)"
read -r CUR TOP <<<"$(cursor "$WORK/root.img")"
[ "$CUR" -gt "$TOP" ] && ok "cp.next_free_nid $CUR > max live nid $TOP" || fail "cp.next_free_nid $CUR <= max live nid $TOP"
"$ROOT/scripts/check-dualboot-fixture.sh" "$IMG" --installed | tee "$WORK/check.log" | grep -E "^FAIL" || true
grep -q "^RESULT installed fail=0" "$WORK/check.log" || fail "check-dualboot-fixture.sh --installed"
ok "check-dualboot-fixture.sh --installed"

echo "== phase 7: LeandrOS boots, home kept; Linux still the default"
use_loader "$IMG" leandros.conf
boot_vm after --boot-disk="$IMG,nvme"
CMD=$(r 'cat /proc/cmdline'); echo "$CMD" | grep -qi "root=PARTUUID=$ROOT_PU" && ok "root=PARTUUID=$ROOT_PU (unchanged)" || fail "cmdline root"
echo "$CMD" | grep -q 'blk.rw=' && ok "blk.rw= on the cmdline" || fail "blk.rw= missing"
r 'cat /home/keep.txt' | grep -q reinstall-keep && ok "/home/keep.txt survived" || fail "home file lost"
r 'echo ok > /root/re.txt && sync && cat /root/re.txt' | grep -q ok && ok "reinstalled root is writable"
stop_vm
use_loader "$IMG" orig
"$ROOT/scripts/check-dualboot-fixture.sh" "$IMG" --boot "$ARCH" | tail -3 | grep -q "PASS boot" || fail "Linux boot after the reinstall"
ok "systemd-boot default boots Linux (LINUX-DUALBOOT-OK)"
echo "PASS: dualboot-reinstall-test $ARCH"
