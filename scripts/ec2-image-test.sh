#!/usr/bin/env bash
# Tests for the EC2 image flow (scripts/ec2-image.sh, scripts/ec2-ami.sh).
#
#   scripts/ec2-image-test.sh dryrun            golden test of the aws argv; makes no AWS call
#   scripts/ec2-image-test.sh boot <arch>       boot build/ec2-<arch>.img in QEMU as a plain UEFI
#                                               NVMe-only disk (q35+OVMF / virt+AAVMF) to "login:"
#   scripts/ec2-image-test.sh all               dryrun + boot of both arches
#
# The boot test needs build/ec2-<arch>.img (scripts/ec2-image.sh <arch>). On a
# Mac hold the QEMU lock (lock.sh qemu) around it. TIMEOUT=seconds (default 420).
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."
fail=0
ok()  { echo "PASS: $*"; }
bad() { echo "FAIL: $*"; fail=1; }

test_dryrun() {
    local tmp; tmp="$(mktemp -d)"
    # Stub aws/coldsnap that record any call; a dry run must never reach them.
    for t in aws coldsnap; do
        printf '#!/bin/sh\necho "$0 $*" >> "%s/called"\nexit 1\n' "$tmp" > "$tmp/$t"; chmod +x "$tmp/$t"
    done
    local a ec2arch out
    for a in x86_64 aarch64; do
        [[ $a == x86_64 ]] && ec2arch=x86_64 || ec2arch=arm64
        out="$(env -i PATH="$tmp:$PATH" HOME="$tmp" "$SCRIPT_DIR/ec2-ami.sh" --dry-run --region eu-west-1 --name t-$a all $a 2>&1)"
        grep -qF "+ coldsnap --region eu-west-1 upload build/ec2-$a.img" <<<"$out" && ok "$a: coldsnap upload" || bad "$a: coldsnap upload"
        grep -qF "+ coldsnap --region eu-west-1 wait snap-DRYRUN" <<<"$out" && ok "$a: coldsnap wait" || bad "$a: coldsnap wait"
        for want in "ec2 register-image" "--name t-$a" "--architecture $ec2arch" "--boot-mode uefi" "--ena-support" \
                    "--root-device-name /dev/xvda" "SnapshotId=snap-DRYRUN" "--imds-support v2.0"; do
            grep -qF -- "$want" <<<"$out" && ok "$a: register-image has '$want'" || bad "$a: register-image lacks '$want'"
        done
        out="$(env -i PATH="$tmp:$PATH" HOME="$tmp" EC2_AMI_ID=ami-1 EC2_SNAPSHOT_ID=snap-1 EC2_INSTANCE_ID=i-1 \
               EC2_MAX_MINUTES=5 EC2_KEY_NAME=k EC2_SG_ID=sg-1 sh -c "
               '$SCRIPT_DIR/ec2-ami.sh' --dry-run run $a; '$SCRIPT_DIR/ec2-ami.sh' --dry-run console $a;
               '$SCRIPT_DIR/ec2-ami.sh' --dry-run teardown $a" 2>&1)"
        for want in "ec2 run-instances" "MarketType=spot" "HttpTokens=required" "get-console-output" \
                    "terminate-instances" "deregister-image" "delete-snapshot"; do
            grep -qF -- "$want" <<<"$out" && ok "$a: '$want'" || bad "$a: missing '$want'"
        done
    done
    [[ ! -e "$tmp/called" ]] && ok "dry run called neither aws nor coldsnap" || bad "dry run called: $(cat "$tmp/called")"
    # A real call without the explicit opt-in must be refused.
    env -i PATH="$tmp:$PATH" HOME="$tmp" "$SCRIPT_DIR/ec2-ami.sh" upload x86_64 >/dev/null 2>&1 \
        && bad "real call without LEANDROS_EC2_REAL=1 was not refused" || ok "real call refused without LEANDROS_EC2_REAL=1"
    [[ ! -e "$tmp/called" ]] && ok "refusal called nothing" || bad "refusal called: $(cat "$tmp/called")"
    rm -rf "$tmp"
}

test_boot() {
    local arch="$1" img="build/ec2-$1.img" tmo="${TIMEOUT:-420}"
    [[ -f "$img" ]] || { bad "$arch: missing $img (run scripts/ec2-image.sh $arch)"; return; }
    local w; w="$(mktemp -d)"; local log="$w/serial.log"
    cp -c "$img" "$w/disk.img" 2>/dev/null || cp "$img" "$w/disk.img"
    local share=/opt/homebrew/share/qemu code vars
    local -a q
    if [[ $arch == x86_64 ]]; then
        for code in "$share/edk2-x86_64-code.fd" /usr/share/OVMF/OVMF_CODE.fd /usr/share/edk2/x64/OVMF_CODE.4m.fd /usr/share/edk2/x64/OVMF_CODE.fd; do [[ -f $code ]] && break; done
        for vars in "$share/edk2-i386-vars.fd" /usr/share/OVMF/OVMF_VARS.fd /usr/share/edk2/x64/OVMF_VARS.4m.fd /usr/share/edk2/x64/OVMF_VARS.fd; do [[ -f $vars ]] && break; done
        cp "$vars" "$w/vars.fd"
        q=(qemu-system-x86_64 -machine q35,i8042=off -cpu max -smp 2 -m 2048)
        [[ -w /dev/kvm ]] && q+=(-accel kvm) || q+=(-accel tcg)
    else
        for code in "$share/edk2-aarch64-code.fd" /usr/share/AAVMF/AAVMF_CODE.fd /usr/share/edk2/aarch64/QEMU_CODE.4m.fd /usr/share/edk2/aarch64/QEMU_CODE.fd; do [[ -f $code ]] && break; done
        for vars in "$share/edk2-arm-vars.fd" /usr/share/AAVMF/AAVMF_VARS.fd /usr/share/edk2/aarch64/QEMU_VARS.4m.fd /usr/share/edk2/aarch64/QEMU_VARS.fd; do [[ -f $vars ]] && break; done
        cp "$vars" "$w/vars.fd"
        q=(qemu-system-aarch64 -machine virt,gic-version=3 -smp 2 -m 2048)
        if [[ "$(uname -sm)" == "Darwin arm64" ]]; then q+=(-accel hvf -cpu host); else q+=(-accel tcg -cpu max); fi
    fi
    [[ -f $code && -f $vars ]] || { bad "$arch: no UEFI firmware found"; return; }
    # The disk is NVMe only (no virtio-blk), no VGA, no default devices: like a Nitro instance.
    q+=(-nographic -nodefaults -vga none -serial "file:$log"
        -drive "if=pflash,format=raw,readonly=on,file=$code" -drive "if=pflash,format=raw,file=$w/vars.fd"
        -drive "file=$w/disk.img,format=raw,if=none,id=d0" -device nvme,drive=d0,serial=vol0123456789abcdef0)
    "${q[@]}" >/dev/null 2>&1 &
    local pid=$! t=0
    while (( t < tmo )) && kill -0 $pid 2>/dev/null && ! grep -aq 'login: ' "$log" 2>/dev/null; do sleep 5; t=$((t+5)); done
    kill $pid 2>/dev/null; wait $pid 2>/dev/null
    grep -aq 'login: ' "$log" && ok "$arch: reached login on serial after ${t}s" || bad "$arch: no login prompt in ${tmo}s (log $log)"
    grep -aq 'READ-WRITE (nvme.rw=1)' "$log" && ok "$arch: NVMe root is read-write" || bad "$arch: NVMe not read-write"
    grep -aq 'F2FS mounted successfully' "$log" && ok "$arch: f2fs root mounted" || bad "$arch: f2fs root not mounted"
    (( fail )) || rm -rf "$w"
}

case "${1:-}" in
    dryrun) test_dryrun ;;
    boot) test_boot "${2:?arch}" ;;
    all) test_dryrun; test_boot x86_64; test_boot aarch64 ;;
    *) echo "usage: $0 dryrun | boot <arch> | all" >&2; exit 2 ;;
esac
exit $fail
