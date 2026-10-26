# lane/piv3d 2026-10-26 (openbugs #98) - DESIGN: a real V3D 7.1 (BCM2712) kernel driver

Status: DESIGN ONLY, no driver code. Goal: Mesa 25.3.6 `v3d` (GL ES 3.1) and `v3dv` (Vulkan 1.3) run on
the Pi 5 GPU through our existing single-device DRM node. Reference: upstream Linux `drivers/gpu/drm/v3d/`
(master, fetched 2026-10-26: v3d_regs.h, v3d_mmu.c, v3d_gem.c, v3d_sched.c, v3d_irq.c, v3d_drv.c,
v3d_power.c, v3d_submit.c), `drivers/pmdomain/bcm/bcm2835-power.c`, `drivers/clk/bcm/clk-raspberrypi.c`,
Mesa 25.3.6 (`leandros-artifacts/llvmpipe-lane/src/mesa`, `include/drm-uapi/v3d_drm.h`), DTB `$SP/piirq/rpi5.dts`.

## 1. What exists today (inventory)

| Piece | Where | State |
|---|---|---|
| `v3d` identity (VERSION `v3d`/`20180419`, no trailing NUL) | `servers/drm/src/lib.rs:145-180` + `std_handle_version` | done, single device: card0 AND renderD128 both say `v3d` (settled 2026-08-20) |
| Backend switch | `drm_device_interface.rs:4508` `V3D_ENABLED`, armed by SET_CLIENT_CAP `0x1000_0003`; `v3d_set_enabled(true)` is the hook a real probe calls | done |
| ioctl numbers 0x40-0x4D | `drm_device_interface.rs:4368-4381`, dispatch `:9988` | all 14 decoded |
| GET_PARAM | `:10098` synthetic IDENT0 `0x07443356`, IDENT1 `0x20000421` (8 QPU, VPM 16 KiB), HUB_IDENT3 `0x00040600` (rev 6, compat 4); TFU=1 CSD=0 CACHE_FLUSH=1 PERFMON=0 MULTISYNC=0 CPU_QUEUE=0 SUPER_PAGES=0 MAX_PERF=0 | stub |
| CREATE_BO | `:10169` buddy, physically contiguous, power-of-two, zeroed; VA from `V3D_VA_SPANS` (0x1000..4 GiB, 4 KiB grain) | VA allocator reusable; no page table behind it |
| MMAP_BO / GET_BO_OFFSET / WAIT_BO | `:10250/:10275/:10304` (WAIT_BO returns ETIME, Mesa retries only on ETIME) | reusable as-is |
| SUBMIT_CL | `:10387` validates, rejects extensions, **ignores in_sync_bcl/in_sync_rcl**, fake fence after `V3D_STUB_LATENCY_TICKS`=2 | stub; frames are not pixels |
| TFU, CSD, CPU, PERFMON_* | `v3d_enosys` | ENOSYS |
| sync_file export, syncobj | `sync_file.rs`, `syncobj_core.rs` (on while v3d armed, kmscube2) | reusable |
| ADDFB2 of a v3d BO | `v3d_framebuffer` `:2602` (linear, pitch == w*4, offset 0) -> virtio 2D resource | QEMU only; Pi needs its own scanout path |
| FB pitch check | `rpi_mailbox.rs:674` logs `v3d_linear_match=` (firmware pitch vs tight `w*4`) | log only |
| GIC | `arch/aarch64/src/gic.rs:515 request_irq`, `:562 enable_spi` | exists; SPI delivery on BCM2712 is in doubt (#101) |

What Mesa calls on the stub (kmscube2, 98 fps on QEMU HVF with `MESA_LOADER_DRIVER_OVERRIDE=v3d`): VERSION,
GET_PARAM (IDENT0/1, HUB_IDENT3, SUPPORTS_*), CREATE_BO, MMAP_BO, GET_BO_OFFSET, SUBMIT_CL, WAIT_BO,
SYNCOBJ_CREATE/WAIT, SYNCOBJ_HANDLE_TO_FD(EXPORT_SYNC_FILE), PRIME, ADDFB2, GEM_CLOSE. No TFU/CSD calls
because CSD=0 and nothing generated a mipmap. Mesa build rule: only ship the megadriver
(`build-gpu-stack-alpine.sh DRIVERS=zink,virgl,softpipe,v3d`) which has an NT_GNU_BUILD_ID note.

Found while reading (fix in phase A, cosmetic today): stub `V3D_HUB_IDENT1 = 0x0700_0101` is mis-encoded.
Linux layout is TVER[3:0], REV[7:4], NCORES[11:8], WITH_TFU bit 17, so it decodes as "V3D 1.0". A 7.1
single-core TFU part is `0x0002_0117`-ish. Mesa never reads it; the real driver forwards the hardware value.

## 2. Hardware (BCM2712, DT `v3d@2000000`, compatible `brcm,2712-v3d`, status "disabled" in the stock DT)

The `axi` bus has identity `ranges` and `dma-ranges` (0..64 GiB and 0x10_xxxx_xxxx), so **V3D sees CPU
physical addresses directly**. There is no 0xC000_0000 bus alias like the mailbox has. No `dma-coherent`,
so V3D is **not** cache-coherent with the CPUs.

| Region | Phys | Size | Contents |
|---|---|---|---|
| hub | `0x10_0200_0000` | 0x4000 | AXICFG 0x0, UIFCFG 0x4, IDENT0-3 0x8-0x14, HUB_INT 0x50-0x64, **GMP 0x600 (7.1 moved it to the hub)**, **TFU 0x700-0x734**, MMUC 0x1000, MMU 0x1200-0x1238 |
| core0 | `0x10_0200_8000` | 0x6000 | CTL_IDENT 0x0-0x8, L2CACTL 0x20, SLCACTL 0x24, L2TCACTL 0x30, L2TFLSTA/END 0x34/0x38, CTL_INT 0x50-0x64, CLE 0x100-0x178, PTB 0x300-0x310, **CSD 0x900, QUEUED_CFG0-7 0x930-0x94C (7.1 offsets)** |
| sms | `0x10_0203_0800` | 0x700 | SMS_REE_CS 0x0, SMS_TEE_CS 0x400 (power state machine, 7.1 only) |

Interrupts (`interrupts = <0 0xfa 4  0 0xf9 4>`, level-high). Linux `v3d_irq_init` takes index 1 as the core
IRQ and index 0 as the hub IRQ:
- **hub = SPI 250 = INTID 282**: TFUC bit1, MMU_CAP bit3, MMU_PTI bit4, MMU_WRV bit5, GMPV bit6 (7.1).
- **core0 = SPI 249 = INTID 281**: FRDONE bit0 (render), FLDONE bit1 (bin), OUTOMEM bit2, CSDDONE **bit6** on
  7.1 (bit7 on 4.2), PCTR bit5 on 7.1.
Enable sequence: `INT_CLR = mask; INT_MSK_SET = ~mask; INT_MSK_CLR = mask` for both blocks.

### Power-on (what Linux does on a Pi 5)
1. Power domain `pm 1` (= BCM2835_POWER_DOMAIN_GRAFX_V3D) on `brcm,bcm2712-pm` @ `0x10_7D20_0000`. The
   2712 has no ASB, so `bcm2835_asb_power_on(PM_GRAFX_2712 = 0x304, 0, 0, PM_V3DRSTN = bit6)`: clock off,
   wait 1 us, `PM[0x304] = 0x5a000000 | (PM[0x304] | bit6)` (deassert reset, password in [31:24]), clock on.
   The reset line `resets = <pm 0>` is the same domain cycled off and on.
2. Clock `clocks = <firmware_clocks 5>` = RPI_FIRMWARE_V3D_CLK_ID 5. `clk_prepare_enable` = mailbox tag
   SET_CLOCK_STATE `0x00038001` (id 5, state 1), then (`maximize` variant) SET_CLOCK_RATE `0x00038002` to
   GET_MAX_CLOCK_RATE `0x00030004`. Our `rpi_mailbox.rs` already sends property tags; add these three.
3. SMS (7.1): `SMS_TEE_CS = CLEAR_POWER_OFF (bit29)`; poll `STATE[4:0] == IDLE (0)` for 100 ms; then reset:
   `SMS_REE_CS = STATE 4`, poll until state is neither ISOLATING_FOR_RESET nor RESETTING.
4. `L2TFLSTA = 0, L2TFLEND = ~0` (core); then read `MMU_DEBUG_INFO`: PA width = 30 + [11:8], VA width = 30 + [7:4].
5. Read `HUB_IDENT1`: ver = TVER*10 + REV must be 71, NCORES must be 1; `HUB_IDENT3[15:8]` = rev.
Inert-on-failure rule: every poll is bounded at 100 ms. Any timeout logs `[v3d] ...`, leaves the backend
disarmed (softpipe keeps working), and never touches the root card.

### MMU (flat, single level)
- 32-bit GPU VA, 4 KiB pages, **one flat table of 2^20 u32 PTEs = 4 MiB physically contiguous**
  (buddy order 10). `MMU_PT_PA_BASE = pt_pa >> 12`.
- PTE = `pfn(24 bits, so PA < 64 GiB) | VALID bit28 | WRITEABLE bit29 | BIGPAGE bit30 (64 KiB) | SUPERPAGE
  bit31 (1 MiB)`. Big/super pages need VA and PA alignment, and every one of the 16/256 PTEs is still written
  (`pfn+i`). Our BOs are already contiguous power-of-two buddy blocks, so super pages come for free later
  (SUPPORTS_SUPER_PAGES may stay 0).
- `MMU_CTL = ENABLE | PT_INVALID_{ENABLE,ABORT,INT} | WRITE_VIOLATION_{ABORT,INT} | CAP_EXCEEDED_{ABORT,INT}`;
  `MMU_ILLEGAL_ADDR = scratch_pa>>12 | bit31` (one zeroed 4 KiB page, so faults land somewhere harmless);
  `MMUC_CONTROL = ENABLE`.
- **Flush** after every PTE change: `MMUC_CONTROL = FLUSH|ENABLE`, poll FLUSHING(bit2) clear; then
  `MMU_CTL |= TLB_CLEAR(bit2)`, poll TLB_CLEARING(bit7) clear. Each poll is bounded at 100 ms.
- Our side: the PT is written by the CPU, so map it Normal-NC (or `dc cvac` every PTE line before the flush).
  BO CPU mmaps must be write-combine/NC like Linux `map_wc = true`. Check the `dev_uncached` branch in
  `kernel/src/syscall.rs:~3000` covers v3d BOs. A cacheable BO mapping gives silently stale pixels.
- MMU faults (hub PTI/WRV/CAP): log `MMU_VIO_ID`, `MMU_VIO_ADDR` (shifted left by va_width-32), write MMU_CTL back to ack.

### Command submission
- **Bin (CT0)**: `PTB_BPOS = 0` (drop the previous overflow); invalidate caches; `CT0QMA = qma, CT0QMS = qms`
  if qma; `CT0QTS = qts | ENABLE(bit1)` if qts; `CT0QBA = bcl_start`; **`CT0QEA = bcl_end` starts it**.
  Done = FLDONE. OUTOMEM -> allocate a 256 KiB BO, map it, `PTB_BPOA = va`, `PTB_BPOS = 256 KiB` (the IRQ
  handler does this; BOs are attached to the job and freed with it).
- **Render (CT1)**: invalidate caches; `CT1QBA = rcl_start`; **`CT1QEA = rcl_end` starts it**. Done = FRDONE.
  Render waits for bin; skip bin when `bcl_start == bcl_end`.
- **Cache invalidate before every job** (`v3d_invalidate_caches`; L3/L2C are no-ops on 7.1):
  `L2TCACTL = L2TFLS | FLM_FLUSH(0)<<1`; `SLCACTL = 0x0F0F0F0F` (TVCCS|TDCCS|UCC|ICC).
- **Cache clean job** (SUBMIT_CL with FLAG `FLUSH_CACHE`, and after every CSD): `L2TCACTL = TMUWCF(bit8)`, poll
  clear; `L2TCACTL = L2TFLS | FLM_CLEAN(2)<<1`, poll L2TFLS clear. Runs on the CPU, after the job's fence.
- **TFU** (hub): write IIA 0x70C, IIS 0x710, ICA 0x714, IUA 0x718, IOA 0x720, **IOC 0x71C (7.1 only, from
  `v71.ioc`)**, IOS 0x724, COEF0-3 (only if `coef[0] & USECOEF`), then **`ICFG 0x708 = icfg | IOC(bit0)`
  starts it**. Done = hub TFUC.
- **CSD** (core): invalidate caches; `QUEUED_CFG1..6 = cfg[1..6]`, **`CFG7 0x94C = 0` (7.1)**, then
  **`QUEUED_CFG0 0x930 = cfg[0]` starts it**. Done = CSDDONE bit6. Followed by a clean job.
- Hang detection: a 500 ms timer per queue; progress = CTnCA/CTnRA changed (CL) or CSD batch count
  changed. No progress -> reset: `GMP_CFG(hub 0x604) = STOP_REQ`, poll GMP_STATUS counts==0; SMS reset; PM
  domain cycle; re-init (step 4 + MMU + IRQ); fail the job's fence with an error; bump `GLOBAL_RESET_COUNTER`.

### 7.1 vs 4.2 deltas the code must key on
TFU block at 0x700 (4.2: 0x400) and new IOC register; GMP moved to the hub at 0x600; CSD queued at 0x930 with
a CFG7; CSDDONE bit6, PCTR bit5; MMUC CLEAR bit11; SMS block (power and reset) instead of the bridge or GCA;
hub GMPV interrupt; PTE `pfn` is 24 bits. Single core, one hub. 4.2 (Pi 4) support is out of scope;
use `ver >= 71` helpers anyway so a Pi 4 port is cheap.

## 3. UAPI needed by Mesa 25.3.6

Probe gates (verified in source):
- `v3dv` `device_has_expected_features` (`v3dv_device.c:747`) **requires TFU && CSD && CACHE_FLUSH &&
  MULTISYNC_EXT**. Without them there is no Vulkan device at all.
- `v3d` gallium: `compute = has_csd` (`v3d_screen.c:296`), so **GL ES 3.1 needs CSD**; SSBO and image
  limits need CACHE_FLUSH (`:175-180`).
- CPU_QUEUE is optional: `v3dv_queue.c:309/566/726/835` run timestamp, query and indirect-CSD work on the CPU
  when it is 0. PERFMON is optional (no VK_KHR_performance_query).

| ioctl (nr) | code | struct (bytes) | Need |
|---|---|---|---|
| SUBMIT_CL 0x00 | 0xC0486440 | bcl_start,bcl_end,rcl_start,rcl_end,in_sync_bcl,in_sync_rcl,out_sync,qma,qms,qts (u32 x10) @0-39; bo_handles u64 @40; bo_handle_count @48; flags @52 (FLUSH_CACHE=1, EXTENSION=2); perfmon_id @56; pad @60; extensions u64 @64 (72) | GL+VK |
| WAIT_BO 0x01 | 0xC0106441 | handle, pad, timeout_ns u64 (16) | GL+VK (have) |
| CREATE_BO 0x02 | 0xC0106442 | size, flags, out handle, out offset (16) | have; back it with PTEs |
| MMAP_BO 0x03 | 0xC0106443 | handle, flags, out offset u64 (16) | have |
| GET_PARAM 0x04 | 0xC0106444 | param, pad, value u64 (16) | have; answer from HW registers |
| GET_BO_OFFSET 0x05 | 0xC0086445 | handle, out offset (8) | have |
| SUBMIT_TFU 0x06 | 0x40586446 | icfg,iia,iis,ica,iua,ioa,ios @0-27; coef[4] @28; bo_handles[4] @44 (first = output); in_sync @60; out_sync @64; flags @68; extensions u64 @72; v71.ioc @80; pad @84 (88) | GL (mipmaps, blits) + VK |
| SUBMIT_CSD 0x07 | 0x40586447 | cfg[7] @0; coef[4] @28; bo_handles u64 @48; bo_handle_count @56; in_sync @60; out_sync @64; perfmon_id @68; extensions u64 @72; flags @80; pad @84 (88) | ES 3.1 + VK |
| PERFMON_* 0x08-0x0A, 0x0C, 0x0D | | | stub: PERFMON=0, MAX_PERF=0, ENOSYS stays |
| SUBMIT_CPU 0x0B | 0x4018644B | | stub: CPU_QUEUE=0, ENOSYS stays |

Extensions, a chain of `drm_v3d_extension {u64 next; u32 id; u32 flags}`:
- **MULTI_SYNC id 1 (required by v3dv)**: `{ext(16); u64 in_syncs; u64 out_syncs; u32 in_count; u32 out_count;
  u32 wait_stage; u32 pad}` = 48 bytes. Each element is `drm_v3d_sem {u32 handle; u32 flags; u64 point;
  u64 mbz[2]}` = 32 bytes. Semantics (v3d_submit.c:210-230): in-syncs attach only to the job whose queue
  == `wait_stage` (BIN=0, RENDER=1, TFU=2, CSD=3, CACHE_CLEAN=4, CPU=5); out-syncs get the LAST job's
  fence; the legacy `in_sync_*`/`out_sync` fields are ignored when MULTI_SYNC is present (v3d_submit.c:1161).
  Binary syncobjs only: reject `point`/`flags` != 0 with EINVAL (no timelines on this path).
- INDIRECT_CSD and the TIMESTAMP/PERF query extensions (ids 2-7) appear only with SUBMIT_CPU, so they
  stay out because CPU_QUEUE=0.

GET_PARAM after phase C: TFU=1, CSD=1, CACHE_FLUSH=1, MULTISYNC_EXT=1, CPU_QUEUE=0, PERFMON=0,
MAX_PERF_COUNTERS=0, SUPER_PAGES=0 (1 later), the RESET_COUNTERs live. IDENT values come from the
registers, never synthetic, once the hardware is probed.

## 4. Design shape

New module `drivers/src/v3d/` behind feature `rpi5`, with a hardware seam so all logic is host-testable:
```rust
pub trait V3dRegs { fn hub_r(&self, off: u32) -> u32; fn hub_w(&self, off: u32, v: u32);
                    fn core_r(&self, off: u32) -> u32; fn core_w(&self, off: u32, v: u32);
                    fn sms_r(&self, off: u32) -> u32;  fn sms_w(&self, off: u32, v: u32); }
```
- `regs.rs` (offsets and bits above), `power.rs` (PM, mailbox clock, SMS), `mmu.rs` (4 MiB PT, insert/remove,
  flush), `queue.rs` (5 queues BIN/RENDER/TFU/CSD/CLEAN, one job in flight per HW queue, in-order
  dependencies, fence ids from `syncobj_core`), `irq.rs` (core and hub handlers -> complete job -> signal
  fence -> wake waiters), `submit.rs` (ioctl parsing incl. MULTI_SYNC, BO refs held until the fence).
- `drm_device_interface.rs` keeps the ioctl parsing it already has. `v3d_dispatch` calls into `v3d::` when
  the hardware probed, and the stub path stays for QEMU (`--arm-v3d` regress guard unchanged).
- `MockRegs` (host tests): register file plus a scripted "GPU" that, on a write to CT0QEA/CT1QEA/ICFG/CFG0,
  raises the matching INT_STS bit after N polls. It checks write ORDER (QBA before QEA, cache invalidation
  before the start, MMU flush after a PTE write).

## 5. Phased plan (lanes of 60 or fewer tool calls)

| Lane | Scope | Pi-visible milestone (one boot) | Testable without HW |
|---|---|---|---|
| **pv3d-power** (A) | DT/constant region mapping (device-nGnRE), PM 0x304 + mailbox clock tags (STATE, MAX_RATE, RATE), SMS power-up/reset, ident read, the `[v3d]` probe line, fix HUB_IDENT1 encoding, kiostat counter | `[v3d] ident ver=71 rev=6 cores=1 qpus=8 va=32 pa=NN clk=NNNMHz sms=idle` (or one inert `[v3d] fail <step>`) | host tests: power sequence order against MockRegs (PM password, bounded polls, timeout -> inert); `check-rpi5.sh`; QEMU regress (rpi5 code compiled out) |
| **pv3d-mmu** (B) | 4 MiB PT (order 10, NC), scratch page, MMU enable/flush, CREATE_BO/free -> PTEs; BO mmaps NC; IRQ wiring both lines + `INT_SET` self-test | `[v3d] mmu pt=0x... flush ok; irq hub=282 ok core=281 ok` (software-raised via HUB_INT_SET/CTL_INT_SET, so it **also proves SPI delivery on BCM2712**, see #101) | host: PTE encoding (4K/64K/1M), flush order, VA reuse after free; QEMU: stub path unchanged |
| **pv3d-cl** (C) | SUBMIT_CL real: bin/render queues, OUTOMEM overflow, invalidate + clean jobs, honoring in_sync_bcl/rcl, fence signal from IRQ, WAIT_BO real, 500 ms hang timer + reset, MULTI_SYNC ext; GET_PARAM from HW, MULTISYNC=1 | in-kernel self-test: a hand-built render-only RCL (`bcl_start==bcl_end`) that clears a 64x64 RGBA8 tile and stores it linear; `[v3d] selftest clear crc=... ok frdone=1 t=NNus`. Then kmscube: `Rendered N frames` and a pixel CRC of the BO | host: job state machine against MockRegs (FLDONE->render start, OUTOMEM path, timeout->reset, multisync wait_stage routing); QEMU: existing kmscube guard stays green |
| **pv3d-tfu-csd** (D) | SUBMIT_TFU (7.1 offsets + IOC), SUBMIT_CSD (CFG7=0, clean job), CSD=1 | `[v3d] selftest tfu copy ok` (raster->raster 64x64), then `es31test` compute dispatch writes an SSBO pattern | host: TFU/CSD register sequences, clean-after-CSD ordering |
| **pv3d-scanout** (E) | Pi scanout of v3d BOs: ADDFB2 on rpi5 = linear XRGB8888 with pitch w*4 -> page flip copies into the firmware FB honoring its pitch (`v3d_linear_match`), dcache clean; later HVS/firmware-KMS direct scanout | kmscube visible on the HDMI panel; `[kms] v3d flip N` counter | host: stride-convert copy unit test; QEMU unaffected |
| **pv3d-mesa** (F) | Auto-arm on probe (`v3d_set_enabled(true)` from the real probe, no override env), `vulkaninfo`/`vkcube`/`glmark2-es2` smoke tests in `/bin`, Pi card-prep includes the megadriver + `libvulkan_broadcom.so` + ICD json, build-id packer check (#98 tail), COSMIC session on v3d | `vulkaninfo` shows "V3D 7.1.6"; `vkcube` and COSMIC on panel; fps numbers | packer check on host; QEMU `--arm-v3d` guard; Vulkan probe against the stub with CSD/MULTISYNC=1 behind a stub-only knob |

Seams: A gives B `V3d { regs, ver, va_width }`. B gives C `mmu.map(bo)/unmap` + `irq::on_core/on_hub`
callbacks. C gives D the queue/fence/clean-job machinery (TFU and CSD are just more queue kinds). E depends
only on B (BOs + cache maintenance). F needs C+D. A and B can share one Pi boot, and so can C and D (a boot
is a card swap).

Order of risk: (1) A: whether the firmware leaves GRAFX_V3D powered (Linux skips power-up when
PM_POWUP is already set) and whether SET_CLOCK_STATE is honored; (2) B: SPI delivery (#101 says uart10's SPI
never fires; the INT_SET self-test isolates that for V3D within one boot, and a polled fallback of INT_STS
on the 1 kHz tick keeps C/D working without IRQs); (3) C: CL encoding of the self-test (take it from Mesa
`v3dx_rcl.c` / `cle/v3d_packet_v71.xml`, not from memory).

## 6. Open / not decided here
- SUPER_PAGES=1 (Mesa then aligns BOs >= 1 MiB). Cheap with buddy BOs; decide after C.
- PERFMON and CPU_QUEUE stay 0 (no perf queries, timestamps on the CPU).
- Runtime PM (SMS power-off, clock to min on idle) is deferred; always-on for now.
- Pi 4 (V3D 4.2, `brcm,2711-v3d`, bridge/ASB reset) is not covered, but the `ver` helpers keep it possible.
