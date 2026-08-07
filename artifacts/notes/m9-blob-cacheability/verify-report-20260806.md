# Blob-mapping cacheability — verification report (2026-08-06)

Lane B. Linux box `forain@172.16.158.150`, `/home/forain/Projects/leandros`, EndeavourOS,
virglrenderer 1.3.0, QEMU 11.0.1. Base commit `a0f2c46`, clean tree; both pre-existing
stashes left untouched. `blob_cacheability.patch` applied with `git apply` (7 files,
+211/−21), plus two instrumentation additions described below.

**Verdict: PASSED.** `s0_submit` passes 3/3 under x86_64/KVM without
`VN_PERF=no_fence_feedback`, where it previously timed out 2/2. The scoping is proven on
the wire, not inferred. The pre-registered host-side stop condition (KVM EPT/IPAT
forcing WB) did **not** trigger — the guest-side change was sufficient.

---

## 1. MAIR_EL1 at runtime — the claim the aarch64 half rests on

The "attributes 2..7 are zero" claim had only ever been static disassembly of Limine's
`BOOTAA64.EFI`. It is now a runtime register read, aarch64 under TCG on the box, printed
from `arch::init` immediately after the UART is mapped (the value is captured in
`mmu::enable_identity` either side of the read-modify-write and stashed in
`mmu::MAIR_BEFORE` / `MAIR_AFTER`, because the UART is not mapped yet when that runs):

```
[ARCH] MAIR_EL1 before=0x00000000000000ff after=0x00000000004400ff
```

**Claim confirmed, and then some.**

| attr | before | meaning | after |
|---|---|---|---|
| 0 | `0xFF` | Normal WB/WA, inner+outer | `0xFF` (preserved) |
| 1 | `0x00` | Device-nGnRnE | `0x00` (preserved) |
| 2 | `0x00` | Device-nGnRnE | **`0x44`** — Normal Inner/Outer NC |
| 3..7 | `0x00` | Device-nGnRnE | `0x00` |

- Attributes 2..7 are indeed zero as delivered, so MAIR index 3 — what
  `arch/aarch64/src/paging.rs:21` called `ATTR_NOCACHE // index 3 (normal NC)` — was
  **Device-nGnRnE**, not Normal-NC. The comment was false, exactly as TODO item 2 states;
  the patch corrects it. Had `PageFlags::NOCACHE` been reused for blob mappings, Mesa's
  first unaligned access through the fence-feedback buffer would have been an alignment
  fault, which is why the separate `WRITECOMBINE` flag is the right shape.
- Index 2 was zero before the write, confirming that claiming it reinterprets no live
  translation (its former flag `ATTR_STRICT` had no users).
- Attributes 0 and 1 survive the read-modify-write byte-for-byte.

**One correction to the analysis, in our favour.** TODO item 1 predicted Limine would
leave `MAIR = 0xFF | (dev_attr << 8)`. The observed value is a flat `0x00FF` — Limine
took its *other* path, so **attribute 1 is zero too**. `PageDescFlags::ATTR_DEV` is
therefore Device-nGnRnE, not the Device-nGnRE its comment claims. Harmless today (nGnRnE
is strictly stronger and MMIO works), but the `ATTR_DEV` comment is wrong for the same
reason `ATTR_NOCACHE`'s was, and is worth a follow-up correction.

---

## 2. The decisive test — x86_64 / KVM / `--venus` / `vkrender`, no `VN_PERF`

`scripts/run-qemu.sh x86_64 --kvm --venus`, guest command literally `vkrender`, no
environment override. Three consecutive runs.

| run | `s0_submit` | `s2_checksum` | totals |
|---|---|---|---|
| 1 | **PASS** | `0x02C0FDC5` | `failures = 0, skipped = 0` |
| 2 | **PASS** | `0x02C0FDC5` | `failures = 0, skipped = 0` |
| 3 | **PASS** | `0x02C0FDC5` | `failures = 0, skipped = 0` |

Literal lines, identical in all three runs:

```
s0_submit: PASS
[INFO] s2_checksum: FNV-1a over 262144 bytes = 0x02C0FDC5
--- vkrender done, failures = 0, skipped = 0 ---
```

Baseline for comparison: 2/2 timeouts in `s0_submit` on this configuration before the
patch.

---

## 3. The scoping proof

Two instrumentation lines were added and kept (both unconditional — `servers/drm`'s local
`serial_debug` is gated on `drivers::pci::RENDER_DEBUG`, which is `false`, so the trace
uses `drivers::pci::serial_debug` directly):

- `servers/drm/src/lib.rs`, at the `DRM_IOCTL_MMAP` (0x1007) reply, one line per resolved
  mmap token giving the host's cache type and the mapping actually chosen.

Literal serial, byte-identical across all three KVM runs:

```
[DRM] host-visible blob mapped: res=0x00000010 win_off=0x00000000 phys_hi=0x00003800 phys_lo=0x00000000 map_info=0x00000001
[DRM-SRV] mmap token=0x0000380000000000 map_info=0x01 -> writeback
[DRM] host-visible blob mapped: res=0x00000011 win_off=0x00030000 phys_hi=0x00003800 phys_lo=0x00030000 map_info=0x00000001
[DRM-SRV] mmap token=0x0000380000030000 map_info=0x01 -> writeback
[DRM] host-visible blob mapped: res=0x00000012 win_off=0x00130000 phys_hi=0x00003800 phys_lo=0x00130000 map_info=0x00000003
[DRM] non-cached blob mapping honoured (uncached)
[DRM-SRV] mmap token=0x0000380000130000 map_info=0x03 -> uncached
[DRM] host-visible blob mapped: res=0x00000013 win_off=0x00140000 phys_hi=0x00003800 phys_lo=0x00140000 map_info=0x00000001
[DRM-SRV] mmap token=0x0000380000140000 map_info=0x01 -> writeback
[DRM] host-visible blob mapped: res=0x00000014 win_off=0x00940000 phys_hi=0x00003800 phys_lo=0x00940000 map_info=0x00000001
[DRM-SRV] mmap token=0x0000380000940000 map_info=0x01 -> writeback
```

Five host-visible blobs are mapped in a `vkrender` session. **Exactly one** — `res=0x12`,
the only one the host answered `map_info=0x03` (WC) for, i.e. Mesa's fence-feedback
buffer — takes the uncached path. The Venus command ring (`res=0x10`, the first and
largest mapping, `map_info=0x01`) and the three other `CACHED` blobs stay write-back. The
scoping is by cache type, not blanket, and the change is not "the hang went away".

This also confirms the mechanism end to end: the one buffer `vn_GetFenceStatus` polls
with a plain load is precisely the one that was aliasing a host WC mapping write-back.

---

## 4. Non-regression

### `vkrender` / `s2_checksum` on all three configurations

| configuration | `s0_submit` | `s2_checksum` | totals |
|---|---|---|---|
| x86_64 / KVM (×3) | PASS | `0x02C0FDC5` | `failures = 0, skipped = 0` |
| x86_64 / TCG | PASS | `0x02C0FDC5` | `failures = 0, skipped = 0` |
| aarch64 / TCG | PASS | `0x02C0FDC5` | `failures = 0, skipped = 0` |

`s2_checksum` is unchanged from the pinned `0x02C0FDC5` on every configuration, as
predicted: subtest 2's readback buffer is `map_info=0x01` and stays write-back.

The scoping line is identical in shape on all three, with the token base differing only
because the host-visible window lands at a different guest-physical address per
machine/accelerator:

```
x86_64/TCG :  [DRM-SRV] mmap token=0x000000C000130000 map_info=0x03 -> uncached
aarch64/TCG:  [DRM-SRV] mmap token=0x0000008000130000 map_info=0x03 -> uncached
x86_64/KVM :  [DRM-SRV] mmap token=0x0000380000130000 map_info=0x03 -> uncached
```

and in every case the other four blobs in the session are `map_info=0x01 -> writeback`.
aarch64/TCG exercising the new MAIR index 2 path with an unchanged checksum is the
aarch64 half's only reachable positive signal.

### `venustest` / `vktest`

| configuration | `venustest` | `vktest` |
|---|---|---|
| x86_64 / KVM | 68 PASS, `--- venustest done, failures = 0 ---` | `=== SUMMARY: 0 failure(s) ===` |
| x86_64 / TCG | 68 PASS, `failures = 0` | `=== SUMMARY: 0 failure(s) ===` |
| aarch64 / TCG | 68 PASS, `failures = 0` | `=== SUMMARY: 0 failure(s) ===` |

`vktest` still opens the real device: `vkEnumeratePhysicalDevices … count=1`,
`"Virtio-GPU Venus (AMD Ryzen 9 7950X 16-Core Processor (RADV RAPHAEL_MENDOCINO))"`,
`vkCreateDevice -> VK_SUCCESS`.

`venustest`'s own host-visible blobs are all `map_info=0x01 -> writeback`, which is the
negative half of the scoping: nothing outside Mesa's feedback buffer changed behaviour.

### Full suite, FRESH images, `vfstest` run exactly once per image

Images rebuilt with `./scripts/build-all.sh` immediately before; `vfstest` is the first
command in each boot. x86_64 on KVM, aarch64 on TCG. Release builds throughout.

| test | x86_64 | aarch64 |
|---|---|---|
| vfstest | 36 PASS / 0 FAIL | 36 PASS / 0 FAIL |
| scmtest | 30 PASS / 0 FAIL | 30 PASS / 0 FAIL |
| wakepolltest | 10 PASS / 0 FAIL (`SUMMARY pass=40 fail=0`) | 10 PASS / 0 FAIL (`pass=40 fail=0`) |
| forktest | 3 / 0 | 3 / 0 |
| epolltest | 9 / 0 (`SUMMARY pass=9 fail=0`) | 9 / 0 |
| polltest | 6 / 0 | 6 / 0 |
| waittest | 3 PASS / **1 FAIL** — `wait_on_process_group: FAIL` | 5 PASS / 0 FAIL |
| sigtest | 6 / 0 | 6 / 0 |
| timertest | 6 / 0 | 6 / 0 |
| memtest | 4 / 0 | 4 / 0 |
| drmsmoke | 22 / 0 | 22 / 0 |

The single red is `waittest wait_on_process_group` on x86_64 — the flake already recorded
in the open-issues list, and aarch64 passes the same subtest in the same wave on the same
build, which is what a flake looks like and not what a memory-attribute change looks
like. Nothing in this patch touches wait, process groups or scheduling.

`vfstest` is 36/36 on aarch64 as well: the fresh image confirms the `xattr_list_f2fs`
"known aarch64 red" really is the dirty-image artifact it was root-caused to be.

### Boot / Limine

Limine base revision 6 on both arches (`limine: Base revision: 6` in every boot log). Not
touched.

---

## 5. The pre-registered stop condition did not fire

The residual risk was that KVM sets IPAT+WB in the EPT entry for the `ram_device` memslot
QEMU creates for a mapped blob, in which case the guest PTE is ignored and no guest-side
change could help. The discriminator was: hang persists *and* serial confirms the
feedback blob took the uncached path.

Observed: the serial confirms the uncached path **and** the hang is gone, 3/3. So KVM is
not forcing WB for this memslot, the guest PTE is honoured, and the guest-side fix is
sufficient. No QEMU/KVM work is needed.

## 6. Item 2 (`ATTR_NOCACHE` is Device memory) — closed in the same commit

`arch/aarch64/src/paging.rs:21`'s `// index 3 (normal NC)` is corrected in place; index 3
is documented as what it is, Device-nGnRnE, with the boot-path evidence inline. The
framebuffer (`arch/aarch64/src/lib.rs:116`) is **deliberately left as Device memory** —
it works today only because `pitch = width*4` keeps every access aligned, and changing it
is a separate decision that deserves its own test, not a side effect of this patch. That
caveat is now written next to the flag rather than living only in TODO.md.

Follow-up found while verifying, not fixed here: since the observed MAIR is a flat
`0x00FF`, **attribute 1 is zero too**, so `PageDescFlags::ATTR_DEV`'s `// index 1 (device
nGnRE)` is also wrong — it is nGnRnE. Behaviourally harmless (nGnRnE is strictly
stronger, and MMIO works), but the comment should be corrected for the same reason.

## 7. What was added beyond the prepared patch

1. `arch/aarch64/src/mmu.rs` + `arch/aarch64/src/lib.rs`: `MAIR_BEFORE` / `MAIR_AFTER`
   captured in `enable_identity` and printed from `arch::init` once the UART is mapped
   (it is not mapped yet where the RMW happens). One boot line; kept, because it turns
   the assumption this whole flag rests on into a runtime-visible fact.
2. `servers/drm/src/lib.rs`: the `[DRM-SRV] mmap token=… map_info=… -> uncached/writeback`
   line, emitted for **every** resolved 0x1007 token, using `drivers::pci::serial_debug`
   rather than the file-local `serial_debug` (which is gated on
   `drivers::pci::RENDER_DEBUG`, currently `false`, so it would have printed nothing).
   Kept: without it "the hang went away" is the only evidence, and the scoping is the
   substance of the change.

## 8. Landed

Committed **locally** on the box only (never pushed): `0df1810`, `main`, on top of
`a0f2c46`. `git format-patch` of it saved to
`~/code/leandros-artifacts/notes/m9-blob-cacheability/0df1810-blob-cacheability-landed.patch`
so the Mac can apply the same thing. Both pre-existing stashes verified still present and
untouched (`stash@{0}` was never popped).

## 9. Reproduction

Harness `/tmp/m9b_run.py` on the box — one process owns the QEMU run (pty for
`-serial mon:stdio`, logs in as root, sends commands, no watcher/poller/waiter). Serial
logs in `~/m9b/` on the box: `x86-kvm-vkrender-{1,2,3}.log` (the decisive runs),
`c-{x86,aa64}-suite.log`, `c-{x86-kvm,x86-tcg,aa64-tcg}-venus.log`, `aa64-mair-boot.log`.

**Harness trap worth recording.** The first sweep's `expect()` searched backwards with a
4096-byte lookback and therefore re-matched the *previous* command's end sentinel, so
every command after the first in a boot reported `rc=0` without ever running. It was
caught only because `venustest` output was missing from a log that claimed it passed. The
fix marks the buffer position *before* sending and numbers the sentinel per command; all
results in this report come from the fixed harness (except the three x86_64/KVM
`vkrender` runs, which were one command each and so were never affected).
