# Laptop stick root migration to the lx f2fs format (lane/lxmigrate, 2026-10-25/26)

Guest-side helpers used to move the USB-stick root (sda2) and /home (sda4) of the laptop to
volumes written by the lx mkfs (klog `[F2FS] Linux node format (lxnode)`). Run them on the guest
with `scripts/remote.py push` + `run`, detached (`nohup sh X > /root/X.log &`) because stick
writes run at ~750 kB/s. None of them print file contents.

- `s0v2.sh`  stage the root image on sda4 @0 (bootable) and @8 GiB (pristine copy), verify sha.
- `s1v2.sh`  with root = sda4: copy the pristine image to sda2, verify, copy host secrets.
- `s1c.sh`   finish the secrets copy by content (`cp -a` says "same file": every f2fs mount
             reports st_dev 1).
- `setroot.sh <from> <to>`  change only the `root=PARTUUID=...-<from>` token of the TRIAL entry
             (`espctl cmdline` replaces the whole extra string).
- `s3.sh`    with root = sda2: write the `mkfs-f2fs-home.py head` image to sda4, verify.
- `hv.sh`    hash a block range with full blocks.

Gotchas: guest `dd` needs `iflag=fullblock` with `count=`; readback counts must use a block size
that divides the image; the root image for an Intel laptop must be built with `INTEL=1`
(iris + ANV ship-set), else greetd refuses to start COSMIC (gpuprobe falls back to softpipe).

## Intel GPU stage knob (INTEL=1)

`INTEL=1 ./scripts/build-all.sh` (or `INTEL=1 python3 scripts/mkfs-f2fs-populated.py <img> x86_64`) selects
`gpu-stage-x86_64-intel` (build it with `INTEL=1 OUT_SUFFIX=-intel ports/mesa/build-gpu-stack.sh x86_64`).
mkfs fails if INTEL=1 is set and the stage lacks iris / `libvulkan_intel.so` / `intel_icd.x86_64.json`, and it
stamps `<img>.gpu` (`intel` or `generic`). `scripts/write-usb.sh x86_64` refuses an image whose stamp is not `intel`
(override: `LEANDROS_USB_ALLOW_GENERIC_GPU=1`). Before staging an image for migration (`s0v2.sh`), run
`cat f2fs-data0-x86_64.img.gpu` and expect `intel`. QEMU default builds are unchanged.
