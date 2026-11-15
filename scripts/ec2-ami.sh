#!/usr/bin/env bash
# Turn build/ec2-<arch>.img (scripts/ec2-image.sh) into an EC2 AMI and, when
# asked, boot, watch and tear down a throwaway instance.
#
#   ./scripts/ec2-ami.sh [--dry-run] [--region R] [--name N] [--image F] <cmd> <x86_64|aarch64>
#
#   upload    coldsnap upload (EBS direct APIs, no S3 bucket/vmimport role) + wait
#   register  aws ec2 register-image (UEFI, ENA, NVMe root)
#   all       upload + register
#   run       run-instances (spot, needs SNAPSHOT/AMI via --ami, KEY_NAME, SG_ID)
#   console   poll get-console-output
#   teardown  terminate instance, deregister AMI, delete snapshot
#
# --dry-run prints every aws/coldsnap command that would run and runs NONE of
# them: it reads no credentials and needs neither aws nor coldsnap installed.
# A real run additionally needs LEANDROS_EC2_REAL=1 in the environment (it
# spends money on the caller's AWS account); see docs/hardware/aws-ec2.md.
#
# Environment: AWS_REGION (or --region), EC2_AMI_NAME, EC2_KEY_NAME, EC2_SG_ID,
#   EC2_INSTANCE_TYPE (t3.micro | t4g.small), EC2_MAX_MINUTES (required for
#   run), EC2_SNAPSHOT_ID / EC2_AMI_ID / EC2_INSTANCE_ID (steps after upload).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."
die() { echo "ec2-ami: $*" >&2; exit 1; }

DRY=0; REGION="${AWS_REGION:-}"; NAME="${EC2_AMI_NAME:-}"; IMAGE=""; AMI="${EC2_AMI_ID:-}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) DRY=1; shift ;;
        --region) REGION="${2:?}"; shift 2 ;;
        --name) NAME="${2:?}"; shift 2 ;;
        --image) IMAGE="${2:?}"; shift 2 ;;
        --ami) AMI="${2:?}"; shift 2 ;;
        -h|--help) sed -n 2,24p "$0"; exit 0 ;;
        *) break ;;
    esac
done
CMD="${1:-}"; ARCH="${2:-}"
case "$CMD" in upload|register|all|run|console|teardown) ;; *) die "usage: $0 [--dry-run] <upload|register|all|run|console|teardown> <x86_64|aarch64>" ;; esac
case "$ARCH" in
    x86_64)  EC2ARCH=x86_64; ITYPE="${EC2_INSTANCE_TYPE:-t3.micro}" ;;
    aarch64) EC2ARCH=arm64;  ITYPE="${EC2_INSTANCE_TYPE:-t4g.small}" ;;
    *) die "arch must be x86_64 or aarch64" ;;
esac
[[ -n "$IMAGE" ]] || IMAGE="build/ec2-$ARCH.img"
[[ -n "$REGION" ]] || REGION="us-east-1"
SHA="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
[[ -n "$NAME" ]] || NAME="leandros-$ARCH-$SHA"
ROOTDEV=/dev/xvda

if (( ! DRY )); then
    [[ "${LEANDROS_EC2_REAL:-}" == 1 ]] || die "refusing a real AWS call without LEANDROS_EC2_REAL=1 (use --dry-run to preview)"
    command -v aws >/dev/null || die "aws CLI not found"
fi

exec 3>&1   # dry-run lines go to the real stdout even inside $(...)
# Every external command goes through here.
run() {
    if (( DRY )); then { printf '+'; printf ' %q' "$@"; printf '\n'; } >&3; else "$@"; fi
}
# Capture a command's output; in a dry run print it and yield a placeholder.
cap() {  # cap <placeholder> cmd...
    local ph="$1"; shift
    if (( DRY )); then { printf '+'; printf ' %q' "$@"; printf '\n'; } >&3; echo "$ph"; else "$@"; fi
}

SNAP="${EC2_SNAPSHOT_ID:-}"
INST="${EC2_INSTANCE_ID:-}"

do_upload() {
    if (( ! DRY )); then
        command -v coldsnap >/dev/null || die "coldsnap not found (cargo install coldsnap)"
        [[ -f "$IMAGE" ]] || die "missing $IMAGE - run scripts/ec2-image.sh $ARCH"
    fi
    SNAP="$(cap snap-DRYRUN coldsnap --region "$REGION" upload "$IMAGE")"
    run coldsnap --region "$REGION" wait "$SNAP"
}
do_register() {
    [[ -n "$SNAP" ]] || die "no snapshot: run upload first or set EC2_SNAPSHOT_ID"
    AMI="$(cap ami-DRYRUN aws ec2 register-image --region "$REGION" --name "$NAME" \
        --architecture "$EC2ARCH" --boot-mode uefi --ena-support --sriov-net-support simple \
        --virtualization-type hvm --imds-support v2.0 --root-device-name "$ROOTDEV" \
        --block-device-mappings "DeviceName=$ROOTDEV,Ebs={SnapshotId=$SNAP,VolumeType=gp3,DeleteOnTermination=true}" \
        --query ImageId --output text)"
    echo "ami: $AMI"
}
do_run() {
    [[ -n "$AMI" ]] || die "no AMI: pass --ami or set EC2_AMI_ID"
    : "${EC2_MAX_MINUTES:?EC2_MAX_MINUTES is required (hard cap; the instance is terminated at the cap)}"
    : "${EC2_KEY_NAME:?EC2_KEY_NAME is required}"; : "${EC2_SG_ID:?EC2_SG_ID is required}"
    INST="$(cap i-DRYRUN aws ec2 run-instances --region "$REGION" --image-id "$AMI" \
        --instance-type "$ITYPE" --count 1 --instance-market-options MarketType=spot \
        --instance-initiated-shutdown-behavior terminate \
        --metadata-options HttpTokens=required,HttpPutResponseHopLimit=1 \
        --key-name "$EC2_KEY_NAME" --security-group-ids "$EC2_SG_ID" \
        --tag-specifications "ResourceType=instance,Tags=[{Key=lane,Value=ec2}]" \
        --query 'Instances[0].InstanceId' --output text)"
    echo "instance: $INST"
}
do_console() {
    [[ -n "$INST" ]] || die "no instance: set EC2_INSTANCE_ID"
    run aws ec2 get-console-output --region "$REGION" --instance-id "$INST" --latest --output text
}
do_teardown() {
    [[ -n "$INST" ]] && run aws ec2 terminate-instances --region "$REGION" --instance-ids "$INST"
    [[ -n "$AMI" ]] && run aws ec2 deregister-image --region "$REGION" --image-id "$AMI"
    [[ -n "$SNAP" ]] && run aws ec2 delete-snapshot --region "$REGION" --snapshot-id "$SNAP"
    return 0
}

case "$CMD" in
    upload) do_upload ;;
    register) do_register ;;
    all) do_upload; do_register ;;
    run) do_run ;;
    console) do_console ;;
    teardown) do_teardown ;;
esac
