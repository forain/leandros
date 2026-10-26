# lane/pv3d-mmu 2026-10-26 (openbugs #98 phase B, #101 probe): V3D MMU + IRQ lines

Status: DELIVERED (code + host tests). Hardware result pending the next Pi boot.
Design: `artifacts/notes/lane-piv3d-design-2026-10-26.md` section 5, row B.

## What landed
- `drivers/src/v3d/mmu.rs`
  - Pure logic, host-tested: PTE encoding (24-bit PFN | VALID b28 | WRITEABLE b29 | BIGPAGE b30 |
    SUPERPAGE b31); `fill_ptes` picks 1 MiB/64 KiB/4 KiB pages by VA+PA alignment, writes `pfn+i`
    into every one of the 16/256 PTEs, and validates the whole range before it writes anything;
    `clear_ptes`; `enable` (PT_PA_BASE, MMU_CTL, ILLEGAL_ADDR=scratch|bit31, MMUC_CONTROL=ENABLE,
    flush) and `flush_all` (MMUC FLUSH, poll FLUSHING, then MMU_CTL |= TLB_CLEAR, poll TLB_CLEARING;
    each poll bounded at 100 ms with a named timeout stage). `Mmu::map` undoes its PTEs if the flush
    fails, because the caller frees the BO on error.
  - Hardware glue (`aarch64 + rpi5`): 4 MiB table from buddy order 10 plus a zeroed scratch page.
    Both are zeroed through the HHDM and then `dc civac`'d, and the PTEs are written only through a
    Normal-NC `kva::ioremap(WriteCombining)` alias. A table or scratch page above the MMU's PA width
    (`MMU_DEBUG_INFO[11:8]+30`) is refused. Super pages stay off (`SUPPORTS_SUPER_PAGES` is still 0);
    64 KiB big pages are on, as in Linux.
  - `active()`, `map_bo(va, pa, len)` (cleans and invalidates the BO's cacheable alias first) and
    `unmap_bo(va, len)`. `after_probe(regs, ver, va_width)` is called from pv3d-power's `probe_boot`
    success branch only (a failed/skipped probe never reaches it) and prints the one combined `[v3d] mmu ...;
    irq ...` line.
- `drivers/src/v3d/irq.rs`: hub (INTID 282) and core (INTID 281) masks (7.1 and 4.2), and
  Linux's enable order (CLR, MSK_SET=~m, MSK_CLR=m; core first, then hub). Also the core and hub
  handler bodies: ack exactly what was read; on an MMU fault, read VIO_ID and
  `VIO_ADDR<<(va_width-32)`, then write MMU_CTL back to ack its sticky bits. Registration goes
  through `drivers::irq::request_irq(Line)`. Phase C hooks are `set_core_hook`/`set_hub_hook`, and
  `poll()` is the polled fallback, with `polled()` saying whether it is needed.
- The INT_SET self-test per line sets HUB TFUC or CTL FRDONE through INT_SET, waits 50 ms for the
  handler counter, and always clears the bit at the end. Results:
  `ok` = delivered end to end. `gic-pending` = the device asserted it and the GIC has it, but the CPU
  never took it (the #101 uart10 shape). `cpu-masked` = the same, but DAIF.I was set during the test.
  `silent` = INT_STS latched but the GIC never saw the line. `nolatch` = INT_SET is ignored, so the
  test proves nothing. `busy` = request_irq failed. `mode=poll` unless both lines are `ok`.
- `arch_irq_line_state(id)` (new arch export, `gic::spi_state`): read-only GICD
  pending/active/enabled bits.
- `drm_device_interface.rs`, both behind `aarch64 + rpi5` and `mmu::active()`: CREATE_BO maps the
  BO after VA allocation (on failure it frees the VA and the pages and returns EIO). The last-ref free
  path unmaps before `v3d_va_free` and before the pages go back to the buddy allocator. BO user mmaps
  were already write-combine (Normal-NC) through `blob_map_cache_type`'s `v3d_va` branch, so nothing
  changed there. QEMU and the stub never activate the MMU, so their path is byte-identical.

## Evidence
- Host: `cd drivers/v3d-tests && cargo test --target aarch64-apple-darwin`: 26/26. Covers 4K/64K/1M
  encoding, PA-too-high, misalignment and out-of-range with nothing written, alignment-driven page
  choice, the enable sequence vs Linux, the flush order and both bounded timeouts, PTEs visible
  before the flush write, VA reuse after free with no stale PTEs, the failed-flush rollback, IRQ
  masks and enable order, handler acks, the MMU fault decode, and all self-test outcomes.
- Guards shown failing on mutated code: BIGPAGE->SUPERPAGE flag (4 fail), dropping the rollback
  (1 fail), not clearing the self-test bit (2 fail).
- `scripts/check-rpi5.sh`: rpi5 and raspi4b both clean.
- aarch64 QEMU regress (virt, under lock): overall PASS, 0/1 boots failed; the `--arm-v3d` kmscube guard rendered 29 frames (96 fps), so the stub path is unchanged.

## Next Pi boot (after pv3d-power's `[v3d] ident ...` line)
Success:
`[v3d] mmu pt=0x<pa> flush ok; irq hub=282 ok core=281 ok mode=irq`
Failures (each is inert; the line is the whole effect):
- `[v3d] mmu fail nomem; ...`: no 4 MiB buddy block, or the ioremap failed.
- `[v3d] mmu fail pa-too-high; ...`: the table landed above the MMU PA width.
- `[v3d] mmu fail flush-mmuc-timeout; ...` / `flush-tlb-timeout`: the MMU did not respond (block
  not really powered/clocked).
- `irq hub=282 gic-pending ...`: SPI delivery on BCM2712 is broken at the CPU interface, the same as
  #101. `silent` means the INTID is wrong or the line is not routed. `cpu-masked` means the probe ran
  with IRQs off, so the result is inconclusive. `nolatch` means INT_SET is ignored, so it is
  inconclusive. Any of these gives `mode=poll`.
- No `[v3d] mmu` line after `[v3d] ident`: the probe failed or was skipped, and phase B is correctly
  skipped.
Later: `[v3d] mmu fault bits=0x.. axi=0x.. va=0x..` (at most 8), `[v3d] mmu unmap ... fail <tag>`.

## Open
- Phase C drives `irq::poll()` from a tick when `polled()`, and sets the hooks.
- MMU PA width vs buddy placement: a BO above `1<<pa_width` fails CREATE_BO with EIO (logged by
  nothing); Linux avoids it with a DMA mask. Revisit if the Pi reports a small `pa=`.
