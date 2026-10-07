# lane/sticklog — 2026-10-07: kernel log persisted on the USB stick

Branch `lane/sticklog`, base main `8177771`. Not pushed.

Goal: debugging on a real PC booted from a USB stick without photographing the
screen. The whole console log is kept on the stick; plug it into the Mac and
read it.

## Layout (scripts/write-usb.sh)

A fourth/third partition, **64 MiB raw, no filesystem**, last in the table so
the ESP and root stay partitions 1 and 2 and `root=PARTUUID` is unchanged.

| | MBR (default) | GPT (`--gpt`) |
|---|---|---|
| type | `0xDA` (non-FS data) | `042B7470-1302-4E61-9347-A0130B276754`, name `LEANDROS-LOG` |
| entry | p3 | p3 (aarch64), p4 (x86_64: p3 is the BIOS boot partition) |

The kernel and the host tool find it **by type**, never by number.

Placement:
- **device:** p2 is shrunk and the log partition ends where the device ends
  (before the GPT backup table), 1 MiB aligned. p2 no longer spans to the end.
  Over 2 TiB with MBR the log sits just below the 32-bit LBA limit instead.
  The 64 MiB is zeroed on the device explicitly.
- **image:** the log partition follows the f2fs image (1 MiB aligned); the
  image grows by 64 MiB (zeros, sparse).

`LEANDROS_USB_EXTRA_CMDLINE` appends kernel arguments to the stick's cmdline.

## Format (kernel/src/klog.rs)

4 slots of `min(16 MiB, size/4)`; boot N writes slot `(N-1) % 4`, so the last
four boots are kept. Slot = 4096-byte header page + plain text.
Header: magic `LEANLOG1`, boot counter, flush sequence, data length, slot size,
CRC32 of the data, flags (1 FULL, 2 PANIC, 4 LOST), CRC32 of the header. The
data is written before the header. The boot counter is max(valid headers)+1,
read at registration.

## Kernel

- Every console path (`serial_write_byte`, `_patient`, `_direct`,
  `arch_serial_putc_dump`) also pushes the byte into a lock-free 2 MiB RAM
  ring (one `fetch_add`, one store). That covers kernel diagnostics, staged
  lines, and userspace console output (getty/init/brush stdout, which goes
  through `serial_write_raw`).
- `klog::init()` runs in the init task right after `usb_storage::init()`
  (so USB disks are registered): finds the log partition **on the boot disk**
  (`blkdev::find_log_partition(boot_disk())`; if the boot disk is unknown,
  only a unique log partition is accepted), claims the slot, dumps everything
  so far, spawns a flusher task.
- Flush points: registration; every 2 s if there are new bytes; `sync`
  (`sync_all`); poweroff/reboot/halt (`machine_prepare`, after f2fs shutdown
  and the console drain); panic.
- New `drivers::blkdev` raw API: `find_log_partition`, `raw_geometry`,
  `raw_read`, `raw_write(…, panic_path)`; `BlockDevice::try_write_sectors`
  (default = `write_sectors`; `UsbDisk` overrides it with `try_lock`).
  rpi5/raspi4b `blkdev_sdhci` has stubs (not compiled in this lane — only the
  virt targets were built).
- I/O rules kept: the flusher masks IRQs around each flush, so the USB driver
  sees a syscall-like context (IoWindow, spin with `irq_window`, never yield).
  State lock: `try_lock` for periodic/sync; bounded spin for shutdown/panic.
  Nothing blocks under a lock another path could wait on.
- Default (virtio) boots print one extra line,
  `[KLOG] no log partition on the boot disk; log is RAM only`.
- Test hook: `klog.testpanic` on the kernel cmdline panics the flusher task
  ~10 s after the log starts.

## Panic path

Best effort, **works in QEMU on both arches** (message and `Location:` line
are in the log, header flag PANIC). Safe parts: lock-free ring, `try_lock` on
the registry and on the USB disk (a busy disk means the write is skipped, never
waited for), no allocation (buffers are preallocated at init). Not guaranteed:
the panic write polls xHCI without tick-driven deadlines (no IoWindow), so a
wedged controller can hang the panicking CPU after the panic text already hit
serial/screen; and if another CPU was mid-request on the stick, the panic text
is not stored (the 2 s periodic flush then bounds what is missing). Exception
handlers that halt without calling `panic!` do not flush.

## Host tool: scripts/read-usb-log.sh

    sudo ./scripts/read-usb-log.sh /dev/rdisk4          # macOS: latest boot
    sudo ./scripts/read-usb-log.sh /dev/rdisk4 --all    # all slots, oldest first, with banners
    ./scripts/read-usb-log.sh stick.img                 # an image
    sudo ./scripts/read-usb-log.sh /dev/sdX             # Linux

macOS: `diskutil unmountDisk /dev/disk4` first (the log partition itself is
never mounted). Finds the partition by type (MBR 0xDA or the GPT GUID).
Warnings (FULL / PANIC / LOST / CRC MISMATCH) go to stderr (stdout with
`--all`). NUL bytes are stripped. Terminal escape sequences from the console
are kept as written; pipe through `cat -v` or `sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g'`
if needed.

## Evidence (Mac, aarch64 HVF, x86_64 TCG; release build-all, RC=0)

- aarch64 MBR stick: boot 1 log has `[KLOG] logging to /dev/sda3 slot 0
  (boot 1)`, `[BLK] /dev/root -> sda2`, the typed `echo hello-from-userland`
  and its output, `[POWER] filesystems synced`, `reboot: Power down`. Boot 2
  → slot 1; `--all` shows both, no CRC warnings.
- x86_64 GPT stick: log partition is p4 (`sgdisk -p` agrees: ESP, root, BIOS
  boot, `LEANDROS-LOG`); boots 1 and 2 (slots 0 and 1) complete through
  `Power down`.
- Panic: `klog.testpanic` run on both arches (aarch64 MBR as boot 3, x86_64 GPT
  as boot 3): the slot header says PANIC and the log ends with
  `--- KERNEL PANIC --- / klog.testpanic: … / Location: kernel/src/klog.rs`.
- Default virtio boot: `[KLOG] no log partition…`, `/dev/root -> vdb`;
  `runtests.py <arch> --suite regress` 15/15 on aarch64 and x86_64.
- Stick runs were driven by `driver.py` with `LEANDROS_USB_STICK=` (same
  QEMU invocation `run-qemu.sh --usb-stick` builds); `run-qemu.sh` itself
  (interactive window) was not run.

## Open

- Log text keeps ANSI escapes from brush/getty; fine to read, noisy.
- A byte pushed by one CPU while another flushes may read as NUL for one
  flush (reader strips NULs; the byte is then lost). Rare, not seen.
- Slot full stops storing (FULL flag); 16 MiB is roughly hours of normal log.
- Hot-unplug of the boot stick: flushes fail, one `[KLOG] write … failed` line.
