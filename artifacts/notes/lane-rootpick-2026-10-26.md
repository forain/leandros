# lane/rootpick (2026-10-26) - FIXED (openbugs #93)

Problem: with no root= the kernel guessed /dev/root from probe order ("/dev/root -> vdb"). On x86_64 the kernel probes
modern virtio-blk (0x1042) before the transitional base disks (0x1001), so any extra `disable-legacy=on` disk shifted
the base disks and the boot failed ("Failed to mount the root filesystem").

Fix:
- drivers/src/blkdev.rs: resolve_root accepts `root=LABEL=<f2fs label>` (before: /dev/<name>, PARTUUID= only). A tie
  between equally-labelled volumes goes to the single whole virtio disk, else Ambiguous; no match = NotFound (init
  waits 30 s, then its old /dev/vdb fallback). The no-hint guess still works for old images but logs a one-time
  "[BLK] warning: no root= on the kernel command line".
- mkfs-f2fs-populated.py + f2fslxpop.py: root volume labelled `leandros-root` (both SB copies).
- build-all.sh: the QEMU boot image's limine.conf gets `cmdline: root=LABEL=leandros-root`. The repo
  limine/limine.conf, write-usb.sh (root=PARTUUID=) and laptop/espctl paths are unchanged.
- set-image-cmdline.sh keeps an existing root= unless the new line brings one (test knobs no longer drop it).
- New guard rootpicktest.py (runtests --suite rootpick): one 64M disable-legacy=on f2fs disk ahead of the base disks.
  x86_64: PASS (/dev/root -> vdc); with a cmdline-stripped boot image (old behaviour) it FAILS (root mount fails).
- blkmanytest.py: on x86_64 the modern extras are vda.. (base disks follow); it expected vdd..vdo. Now PASS 12/12.

Evidence: aarch64 regress PASS, x86_64 regress PASS; stdev + blkmany + rootpick PASS on both arches.
Open: stdevtest still omits disable-legacy on x86_64 for its device naming (harmless now).
