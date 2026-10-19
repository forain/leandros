# lane/kexeclog (2026-10-19)

Two follow-ups from lane/kexeclaptop: the laptop lost the final kexec log,
and i915 display interrupts stayed enabled across the jump.

## 1. The final kexec log goes to the next kernel (`kernel/src/kexec/prevlog.rs`)
Problem: everything `kernel_kexec` prints after `klog::flush_final` (xHCI
halt, driver hooks, the PCI quiesce, the per-function `[KEXEC]` device-reset
lines, the CPU stop, the purgatory line) reached only the serial port. The
laptop has no serial port, so those lines were lost.

Design:
- **Buffer**: `kexec_file_load` allocates one more owned 16 KiB buddy block
  (order 2) with the image (`stage_all`). Like every staging page, it lies
  outside every destination range, so the copy and the new image never
  touch it. x86_64 keeps it below 4 GiB and aarch64 below 8 GiB. It is freed
  on unload like the other blocks. Layout: magic `LOSPRVLG` (u64), text
  length (u32), capacity (u32), then the text.
- **Capture**: `exec_prepare` calls `prevlog::start` as the first step of
  `kernel_kexec`, and a failed prepare cancels it. After that every kernel
  console line is copied as well (`prevlog::tee` in `console_print_bytes`,
  and in the arch `log()` of `exec_x86_64.rs` / `exec_aarch64.rs`, which
  `devreset::execute` also uses). The copy is lock-free: a `fetch_add` cursor,
  and bytes past the capacity are dropped. Userspace console output is not
  captured. `prevlog::seal` runs just before the jump (x86: before the CR3
  switch, aarch64: before `enter_idmap`). It writes the final length, and on
  aarch64 it does `dc cvac` over the buffer. aarch64 `hex()` now formats
  locally, so its output goes through `log()` too.
- **Hand-over** in the boot info built at load time, not on the cmdline, so
  dedup and `--reuse-cmdline` are not affected and nothing accumulates:
  - x86_64: a private multiboot2 tag `0x4C4F5301` (`multiboot2::TAG_LEANDROS_PREVLOG`,
    payload `phys u64, size u64`). Other parsers skip unknown tags.
  - aarch64: `/chosen/leandros,kexec-prevlog = <phys u64, size u64>`.
  - Both parsers set `boot::kexec_prevlog()` (a static, not a BootInfo field).
  - The range is still Available in the handed-over map.
- **New kernel**: `prevlog::reserve_early` runs before `mm::init_with_map`.
  It accepts only a page-aligned range of 4-64 KiB that lies entirely inside
  one Available region, and then calls `mm::buddy::reserve_range`. Right
  after mm init, `prevlog::replay` checks the magic, prints a header line,
  then every line as `[prevkexec] <line>` (console + klog, so the lines reach
  the persistent log on the laptop), then `end`. It then calls the new
  `mm::buddy::release_reserved`, which clears that exact reservation slot
  and frees the pages.

## 2. i915: display interrupts off in the kexec hook
`drivers::i915::kexec_quiesce` now returns a Vec and first calls
`vblank::kexec_irq_off`, which works even when exec never ran, as long as
KMS mapped the MMIO. It does what Linux `gen8_irq_reset` does for the
display banks:
- GEN8_MASTER_IRQ = 0, followed by a posting read.
- DE pipe A, and pipe B/C when their IER is nonzero: IMR = ~0, IER = 0,
  IIR = ~0 written twice.
- DE port and DE misc banks: the same.
- A `KEXEC_OFF` flag stops the ISR on another CPU from turning the master
  back on.

Only interrupt registers are written. Plane, pipe and the GOP surface stay
as they are, so scanout of the boot framebuffer is untouched. The hook logs
`i915: display interrupts off (MASTER_IRQ a -> b, pipe A IER c -> d, ...)`,
and that line now also reaches the next kernel through prevlog.
**Compile-checked only** (x86_64 and aarch64 release builds). QEMU has no
i915. On the next laptop kexec, check that gen N+1 prints `MASTER_IRQ=0x00000000`
and pipe A `IER=0x00000000` in its `kms: vblank: before:` line.

## Tests (QEMU TCG, 4 CPUs, uefi-tcg; scripts in scratchpad `klog/`)
Limine gen0 (this lane's kernel) → `kexec -l` of this lane's kernel-direct
from an NVMe disk, `--reuse-cmdline --append=kexec.gen=N`, with
`kexec -e -c 1` and then `-c 2`. The `kexec` userland was rebuilt from main
(dedup). The older binary in the acpu scratchpad accumulated
`kexec.gen=1 kexec.gen=2`.
- aarch64: gen1 and gen2 each printed `[early] kexec prevlog: <addr>, 0x4000 B
  reserved until read`, then `[prevkexec] 1934 bytes, 25 lines`. The lines
  cover the exec plan, f2fs clean unmount, REBOOT phase 1, disk flush, NVMe
  CC.EN=0, PCI quiesce, `stopping 3 other CPU(s) from cpuN`, one devreset line
  per function (NVMe FLR back after 110 ms), the reset summary and the
  `cpu0: other CPUs off; purgatory ...` line. Then `buffer ... freed (4 pages)`.
  nproc 4, login, `/proc/cmdline` = `kexec.gen=1`, then `kexec.gen=2`.
- x86_64: same, 2154/2150 bytes, 28 lines, including `stopping 3 other
  CPU(s) from cpu2 (AP): NMI to the BSP`, the virtio-vga framebuffer owner
  kept, the `BSP cpu0: INIT sent to 3 CPU(s); purgatory ...` line and
  `NMI stack high-water`. nproc 4, login, a single `kexec.gen=N`, no prevlog
  key anywhere in the cmdline.
- `boot/bootparse-tests` (`cargo test --target aarch64-apple-darwin`): 12/12.
  The MB2 and FDT round trips now carry and parse prevlog.

## Open
- Real laptop: confirm the `[prevkexec]` lines in the persistent log of gen1,
  and the i915 IRQ state, on the next laptop kexec.
- Lines printed after `seal` (none today) would be lost. Output of the
  purgatory itself is not captured.
