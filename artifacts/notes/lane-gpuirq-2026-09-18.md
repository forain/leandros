# lane/gpuirq — aarch64 virtio-gpu completion interrupt (INTx over GIC SPI), parked sync waits, fence-delivered flip events

Machine: Mac (aarch64 **HVF**, x86_64 **TCG**). Worktree `leandros-gpuirq`, branch `lane/gpuirq` off `7a9ed31`.

## Root cause / what was missing

The x86_64 side got the first device interrupt (virtio-gpu MSI-X, `63fc1c2`); aarch64 stayed
poller-only, so every control-queue completion there was observed at the next 100 Hz tick
(up to 10 ms), every flip-complete event was tick-paced (one per tick), and reply-needing
commands spun the vCPU under `VIRTIO_GPU`.

**Transport:** the aarch64 kernel drives `virtio-gpu-pci` (modern, PCIe ECAM on QEMU `virt`),
not virtio-mmio. QEMU `virt` wires the gpex host bridge's INTA..D to SPIs 3..6 = INTIDs 35..38,
swizzled `line = (slot + pin) % 4`. The GPU is slot 4 pin A → INTID 35 (`0x23`), confirmed by
the boot line `[GPU] INTx armed: slot 4 pin 1 -> INTID 0x23` and the first-completion line.
An ITS/MSI path on GICv3 was not attempted (needs an ITS driver; INTx is one SPI).

## What changed

- `arch/aarch64/src/lib.rs`: `arch_request_irq(id, handler)` / `arch_disable_irq(id)` — the
  driver-side seam into `gic::request_irq` (register-then-enable), same shape as `arch_monotonic_ns`.
- `drivers/src/virtio_gpu.rs`:
  - keeps the ISR-status pointer (`isr_cfg`); `enable_intx()` (aarch64, only when MSI-X is not
    armed) computes the INTID, clears PCI command INTx-disable, reads the ISR once to drop a stale
    condition, registers `virtio_gpu_intx_isr` and arms `CTRLQ_IRQ_ARMED`. Runs before DRIVER_OK.
  - `virtio_gpu_intx_isr`: reads the ISR byte first (that is what deasserts the level line, EOI
    follows in the dispatcher), then the MSI-X handler's body (`ctrlq_tick`: try_lock, reap,
    fence hook). **Storm guard**: 1024 consecutive deliveries with `isr == 0` → mask the SPI,
    log once, poller only.
  - the cursor queue sets `VIRTQ_AVAIL_F_NO_INTERRUPT` (nobody waits on it).
  - **Parked sync waits** (`CtrlqWait`): when an interrupt is armed, `submit` and
    `ensure_ctrlq_room` park in `wfi` (aarch64, IRQs masked — wakes on pending regardless of
    PSTATE.I) / `sti; hlt; cli` (x86_64) and reap on return, instead of spinning; the lock is
    still held (the wait did not move to the call sites), so this is "vCPU idle for the round
    trip", not "other tasks run". Bounded 5 s in ticks. **The interrupt is routed to the BSP only**,
    so the handler kicks a waiter parked on another CPU with the reschedule IPI
    (`CTRLQ_PARKED_CPU`, no preempt flag set) — without this the waiter slept until its own tick
    (measured: mean sync wait 2.5 ms, max 11.7 ms; with the kick: mean 0.1–0.4 ms).
  - presents are now fenced: `send_present_async` (final RESOURCE_FLUSH / SET_SCANOUT_BLOB)
    publishes `LAST_PRESENT_FENCE`.
- `drivers/src/drm_device_interface.rs`: `PENDING_FLIPS` entries carry the present's fence and
  the tick they were queued at. `flip_fence_service` (from the fence hook, i.e. the ISR, and from
  every tick) promotes in-order entries whose fence retired; `drm_tick` delivers only unfenced
  entries (cursor-only commits) or fenced ones unanswered for 2 ticks (fallback for a lost IRQ or
  a slow host). New read-only ioctl `0x1008` returns the interrupt census (armed, irqs, flips on
  fence, parked, spurious, delivered, sync, timeouts). `[DRMSTAT]` gained `flips_irq
  ctrlq_parked intx_spurious park_kicks` at the END of the line.
- `drivers/src/{virtio_blk,virtio_net,snd,virtio_keyboard}.rs` + `pci.rs::PCI_CMD_INTX_DISABLE`:
  every polled virtio-pci function sets PCI command bit 10. They never read their ISR, so an
  asserted INTx would stay asserted forever; the GPU shares line 0 with virtio-net (slot 8) and
  would have livelocked on it.
- `kernel/src/syscall.rs`: routes `0x1008` to the DRM server.
- `userland/drmsmoke`: 6 new cases (`GPU_IRQ_ARMED`, `GPU_IRQ_COMPLETIONS_COUNTED`,
  `FLIP_EVENT_DELIVERED_ON_FENCE`, `SYNC_CMD_PARKED`, `GPU_IRQ_NO_TIMEOUTS`, `INTX_NOT_STORMING`)
  driven by the kernel counters (deterministic), plus printed flip-event latency / flip-rate
  diagnostics over a 32-flip burst.
- `scripts/gpuirqbench.py`: boots the default greeter on one arch, drives pointer motion
  (`--keys` adds keystrokes), reports the `[DRMSTAT]` ctrlq census (needs `DRM_STATS = true`).

## Evidence

**aarch64/HVF (Mac)** — drmsmoke **72/72** (66 + 6 new), greeter killed first
(`touch /etc/leandros/text-login; kill 3`; note `/run/greetd-init.pid` is not written, use pid 3).
No `[WDOG]`, `[TIMER]`, unhandled IRQ, panic, storm or TIMEOUT lines across ~12 boots.

| aarch64/HVF, 32 fenced PAGE_FLIPs, submit→event | main `7a9ed31` (tick) | lane/gpuirq (INTx + fence) |
|---|---|---|
| flip-event latency mean / min / max | 9.8 / 4.7 / 12.2 ms | **1.3–1.75 / 1.15 / 6–9 ms** |
| flips a waiting client can present per second | 101 | **570–740** |
| async completion observation latency (greeter window, n=4) | 4951 µs mean | 311 µs mean |
| `ctrlq_irqs` over the burst | 0 | 34–35 (one per completion) |
| flips delivered on the fence path | — | 32/32 |
| sync commands parked / kicked | — | 2/2 in the test; 13 parked, 6–9 kicked per greeter start |

Sync-wait wall time, 16 commands of a greeter session start (both `DRM_STATS` builds): main
1.3 ms total (mean 83 µs, max 219 µs, vCPU spinning) vs lane 1.9–6.9 ms total (mean 0.12–0.43 ms,
max 0.28–2.3 ms, vCPU parked). Parking costs ~0.1–0.3 ms of wall latency per reply-needing command
(HVF wake + IPI) and buys an idle vCPU; these commands are ~3 % of traffic at session start and
0 during steady state, so it is a wash on this workload — flagged below.

**Greeter frame rate (aarch64/HVF, pointer motion 10/s, 60 s):** main 9.63 commits/s, lane
9.85 commits/s — **not a discriminating benchmark**: pointer motion is a cursor-plane-only atomic
commit (`curs_mv` = `atomic`, `flips_sub` = 2), never touches the control queue, and its
completion event is unfenced (tick path by design). Keystrokes (`--keys`) did not make the greeter
re-render the primary plane either (`flips_sub` stayed at 2). The 101 → 570–740 flips/s ceiling
above is the number that changed.

**x86_64/TCG (Mac)** — drmsmoke **72/72**, `[GPU] MSI-X armed`, 32/32 flips on fence, 2 parked;
no regression. Flip-event latency there is ~172 ms — that is the software-scaled 1280x800 present
under TCG, not the delivery path (`d_irqs` = 34 for 32 flips).

## Refuted / findings

- "Greeter desktop frame rate on aarch64" is input-bound and cursor-plane-only; the ctrlq is idle.
- The spin bound `CTRLQ_WAIT_ITERS` is an iteration count; the parked path needed a tick bound.
- BSP-only interrupt routing + parking a waiter on another CPU = sleeping until that CPU's tick.
  This also applied to x86_64 MSI-X (destination APIC 0) — the kick covers both arches.

## What remains / unverified

- **x86_64/KVM and the Zink/Venus desktop were NOT measured on the box** (this lane ran on the
  Mac). The present-fence change fences SET_SCANOUT_BLOB there; the desktop lane's 20.1 fps
  should be re-run with `scripts/zinkbench.py` before merge is called safe on KVM.
- The parked wait still holds `VIRTIO_GPU`; moving the wait to the call sites (so other tasks can
  use the CPU) is the remaining design step. If the ~0.1–0.3 ms extra wall latency per sync
  command matters somewhere, `CtrlqWait::new` is the one place to gate it off.
- 1-of-N SPI routing (`GICD_IROUTER` bit 31) would remove the IPI hop; not tried.
- `INTX_STORM_LIMIT` fallback path is not exercised by a test (would need a device holding the line).
- The aarch64 greeter's session start issues 16 sync commands vs 3 with no greeter; nothing else
  in the census differed.
