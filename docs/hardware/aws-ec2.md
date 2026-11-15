# AWS EC2

Status: **image and AMI tooling only**. The disk image boots in QEMU as a plain UEFI NVMe disk on
both architectures. The EC2-specific kernel pieces (ENA network driver, Amazon EBS NVMe quirks,
Nitro platform without PIT/i8042/VGA, ACPI-only aarch64 boot, IMDS first-boot) are separate work
items and no real EC2 instance has been booted yet. The design is in `orch/designs/design-ec2.md`.

## Building the image

```bash
./scripts/build-all.sh --arch x86_64        # or aarch64
./scripts/ec2-image.sh x86_64               # -> build/ec2-x86_64.img
./scripts/ec2-image.sh aarch64              # -> build/ec2-aarch64.img
```

`ec2-image.sh` wraps `write-usb.sh --image --gpt`: a raw GPT disk with a FAT32 ESP
(`EFI/BOOT/BOOTX64.EFI` or `BOOTAA64.EFI`, Limine revision 6 or newer, unsigned: Secure Boot stays
off), the f2fs root, a 1 GiB `/home` and a 64 MiB log partition. The image file is sparse, about
8.5 GiB apparent. The kernel command line is `root=PARTUUID=... nvme.rw=1 console=ttyS0 ec2=1` on
x86_64 and `root=PARTUUID=... nvme.rw=1 ec2=1` on aarch64 (`nvme.rw=1` makes the EBS root writable;
NVMe is read-only by default; the controller holding the booted root is also made writable automatically on Amazon EBS controllers, `nvme.rw=0` opts out). Amazon EBS NVMe quirks are applied (48-bit DMA, 300 s I/O stall cap); disks appear as `/dev/disk/by-id/nvme-<model>_<serial>[_<nsid>][-part<N>]` (direct opens only, no directory listing) and expose `serial` / `ebs_device` in sysfs. Override with `LEANDROS_EC2_CMDLINE`, `LEANDROS_EC2_ROOT_GIB`,
`LEANDROS_EC2_HOME_GIB`; `--authorized-keys F` bakes an SSH key into the ESP.

## Testing without AWS

```bash
scripts/ec2-image-test.sh dryrun        # golden test of every aws/coldsnap command; no AWS call
scripts/ec2-image-test.sh boot x86_64   # q35 + OVMF, NVMe only, no VGA/default devices, to "login:"
scripts/ec2-image-test.sh boot aarch64  # virt + AAVMF, NVMe only
```

## AMI flow

```bash
scripts/ec2-ami.sh --dry-run all x86_64     # prints every command, runs none, reads no credentials
LEANDROS_EC2_REAL=1 scripts/ec2-ami.sh --region eu-west-1 all x86_64
```

Steps: `coldsnap upload` (EBS direct APIs, so no S3 bucket and no `vmimport` role) then
`aws ec2 register-image --boot-mode uefi --ena-support --architecture x86_64|arm64
--root-device-name /dev/xvda --block-device-mappings ...`. Further commands: `run` (spot
instance, IMDSv2 required, needs `EC2_MAX_MINUTES`, `EC2_KEY_NAME`, `EC2_SG_ID`), `console`
(`get-console-output`) and `teardown` (terminate, deregister, delete snapshot).
Instance types must be Nitro (`t3`, `m5`, `c6i`, `t4g`, `m7g`...); `t2`/`m4`/`c4` are Xen and
use neither NVMe nor ENA.

## What you must provide for a real run

A real run spends money on your account, so `ec2-ami.sh` refuses it unless `LEANDROS_EC2_REAL=1`.
You provide:

* AWS credentials for the AWS CLI (profile or environment) with `ebs:*` snapshot-write
  (`StartSnapshot`, `PutSnapshotBlock`, `CompleteSnapshot`), `ec2:RegisterImage`, `ec2:RunInstances`,
  `ec2:TerminateInstances`, `ec2:DeregisterImage`, `ec2:DeleteSnapshot`, `ec2:GetConsoleOutput`.
* [coldsnap](https://github.com/awslabs/coldsnap) installed (`cargo install coldsnap`).
* A region, an EC2 key pair (`EC2_KEY_NAME`) and a security group allowing 22/tcp from your IP
  only (`EC2_SG_ID`).
* A time cap (`EC2_MAX_MINUTES`) for any instance launch.

Until the ENA driver lands, an instance has no network: use `get-console-output` or the EC2 Serial
Console to observe it.
