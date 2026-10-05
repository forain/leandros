# Lane usbharness (2026-10-05): boot LeandrOS from an xHCI USB stick in QEMU

Branch `lane/usbharness`, base main `7d114ce`. Mac (aarch64 host), GPU QEMU
`~/.local/qemu-gpu-gles31`. Harness only: the kernel has no USB storage driver
yet, so the expected end state is the same failure real hardware shows today.

## What was built

- `scripts/write-usb.sh <arch> (<device> | --image <out.img>) [--yes] [--gpt]`
  generalises the uncommitted `write-usb-x86_64.sh` (now a 3-line wrapper).
  - x86_64: `BOOTX64.EFI` + `limine bios-install`; aarch64: `BOOTAA64.EFI`, no
    BIOS stage.
  - Default MBR: p1 0xEF FAT32 512 MiB ESP (active), p2 0x83 f2fs root, 1 MiB
    aligned, random disk signature.
  - `--gpt`: protective MBR, primary + backup GPT (CRC32s verified with
    `sgdisk -v`: "No problems found"), random disk and partition GUIDs, ESP type
    C12A7328-... and Linux-data type 0FC63DAF-.... Backup entries + header sit
    at the last 33 sectors: inside the file for `--image`, written with a
    separate `dd` after the image for a real device (p2 then ends before them).
  - f2fs magic check at p2+1024 and all device safety checks are unchanged.
- `scripts/run-qemu.sh --usb-stick[=mbr|gpt]` (UEFI mode only): builds
  `leandros-usb-<arch>-<mbr|gpt>.img` (gitignored `*.img`, sparse, regenerated
  when kernel/initrd/data0/limine.conf/write-usb.sh are newer) and attaches ONLY
  `-device qemu-xhci,id=xhci -drive if=none,id=stick,... -device
  usb-storage,bus=xhci.0,drive=stick,bootindex=0`. No virtio-blk boot or data
  disks (data-disk auto-creation is skipped too). Display, GPU, input, sound,
  net, rng, serial are as in the default mode. The default command lines are
  unchanged: the three virtio-blk drive pairs moved into a `DISK_ARGS` array
  that `--usb-stick` swaps for the stick arguments.

## Findings

- Limine's BIOS installer REFUSES a GPT disk that has no BIOS boot partition
  ("Installing to a GPT device, but no BIOS boot partition specified or
  detected"). So x86_64 `--gpt` adds a third entry, a ~992 KiB BIOS boot
  partition (21686148-6449-6E6F-744E-656564454649, EF02) in the free gap at LBA
  64..2047, listed last so the ESP and root stay partitions 1 and 2. aarch64
  `--gpt` has only the two partitions.
- macOS BSD `dd conv=sparse` truncates the output at the last block it wrote.
  The sparse f2fs copy therefore cut the GPT backup tail off the image; the
  script now re-extends the file and writes the backup tail afterwards.
- The f2fs root image is ~3.4 GiB, so every stick image is a ~4 GiB sparse file.
- OVMF and AAVMF both enumerate the stick as "UEFI QEMU QEMU USB HARDDRIVE" and
  boot Limine from it on the first try; aarch64 virt + `qemu-xhci` needed
  nothing extra (it is a plain PCI device).
- The kernel prints `Failed to mount /dev/vdb` and goes on (DHCP etc. still
  runs); there is no `/dev/vdc` mount line either, as expected with no disks.

## Evidence (serial logs, 80 s timeout, headless, `LEANDROS_NO_VIEWER=1`)

All four stick boots:

    BdsDxe: starting Boot0001 "UEFI QEMU QEMU USB HARDDRIVE 1-0000:00:01.0-1" from PciRoot(0x0)/Pci(0x1,0x0)/USB(0x0,0x0)   (x86_64)
    BdsDxe: starting Boot0001 "UEFI QEMU QEMU USB HARDDRIVE 1-..." ...                                                      (aarch64)
    [MAIN] Limine boot info parsed. Memmap len: ...        (Limine loaded the kernel)
    Mounting F2FS disk /dev/vdb to /mnt...
    ERROR: Failed to mount /dev/vdb to /mnt!

Default modes still boot normally (`Mounting F2FS disk /dev/vdb`, then
`Mounting /dev/vdc at /data (f2fs)...`, `graphical login started`, `login:`).

## Commands

    ./scripts/write-usb.sh x86_64 --image out.img [--gpt]
    ./scripts/write-usb.sh aarch64 --image out.img [--gpt]
    ./scripts/run-qemu.sh x86_64  --usb-stick        # or --usb-stick=gpt
    ./scripts/run-qemu.sh aarch64 --usb-stick=gpt
