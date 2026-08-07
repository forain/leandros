# x86-64 IA32_PAT bring-up — make `PageFlags::WRITECOMBINE` mean WC on both arches

Lane J, 2026-08-06. Worktree based on `main` at **`18a7a9f`** ("drm: honour the host's
requested blob cacheability"), which is the commit this builds on. `c5abb8d`
(FB_DAMAGE_CLIPS) is its parent and is included.

Patch: `pat_bringup.patch` (331 lines, 3 files, all under `arch/x86_64/src/`).
Verified to apply cleanly to a pristine `18a7a9f`.

---

## Recommendation up front

**Land it** — but not for the reason the item was filed under, and the reasoning that
got there overturns a premise in `18a7a9f`'s own commit message.

The item says "there is no `IA32_PAT` bring-up anywhere in `arch/x86_64`, therefore the
reset PAT applies, therefore WC is unreachable". The first clause is true. The second is
**false on the boot path we actually use**: Limine 11.4.1 programs `IA32_PAT` itself,
and it already puts **Write Combining at PA5**. So on the Limine path WC has been
one PTE bit away the whole time, and the reset PAT never applied.

That changes the shape of the fix from "bring up a facility we lack" to "stop depending
on what the loader happened to leave, and make every CPU agree about it". The second
half is the part that turned out to matter most: it closes a **latent cross-CPU
memory-type divergence that exists on `main` today** (see §5), which is a correctness
issue, not a throughput one.

The throughput win is real but currently theoretical: today exactly one small,
read-polled buffer takes the `WRITECOMBINE` path. It becomes 1-2 orders of magnitude
the moment any real Vulkan content streams uploads through host-visible memory, which
is the direction M3/M4 are already going.

---

## 1. The finding that reframes the item: Limine already programs IA32_PAT

Static scan of the shipped bootloader binaries in `.limine-cache/limine-11.4.1-binary/`
for `mov ecx, 0x277` (`b9 77 02 00 00`) followed by `wrmsr` (`0f 30`):

| binary | sites |
|---|---|
| `limine-bios.sys` | 2 |
| `BOOTX64.EFI` | 2 |
| `BOOTIA32.EFI` | 2 |
| `limine-uefi-cd.bin` | 4 (two copies of the above) |
| `BOOTAA64.EFI` | 0 |

Both x86 sites write the same value. Decoding the second one, at `BOOTX64.EFI+0x42f34`:

```
0f a2                    cpuid
f7 c2 00 00 01 00        test edx, 0x10000     ; CPUID.01H:EDX.PAT[bit 16]
74 11                    jz   +0x11            ; skip if the CPU has no PAT
b9 77 02 00 00           mov  ecx, 0x277       ; IA32_PAT
b8 06 04 07 00           mov  eax, 0x00070406
ba 05 01 00 00           mov  edx, 0x00000105
0f 30                    wrmsr
```

`IA32_PAT = 0x0000_0105_0007_0406`, i.e.

| entry | PA0 | PA1 | PA2 | PA3 | PA4 | PA5 | PA6 | PA7 |
|---|---|---|---|---|---|---|---|---|
| Limine | WB | WT | UC- | UC | **WP** | **WC** | UC | UC |
| reset | WB | WT | UC- | UC | WB | WT | UC- | UC |

So under Limine: **PA5 is already WC**, PA4 is WP, PA6 is UC. Only the direct-boot path
(`kernel/src/entry_x86_64.s`, which writes EFER and nothing else) leaves the reset PAT.

Two consequences:

* The `18a7a9f` comment in `translate_flags` ("with the reset PAT ... PCD alone selects
  PA2 = UC-") reaches the **right conclusion for the wrong reason**. PA2 is UC- in both
  the reset PAT *and* Limine's, so the landed fix behaves exactly as documented and
  there is no correctness regression to chase — only a factual claim to correct.
* Limine sets the PAT bit in its own PTEs (that is what a WC entry at a PAT-bit-only
  slot is *for* — the framebuffer). This makes "just pick any unused slot" unsafe in a
  way it was not on aarch64, where the runtime `MAIR_EL1` read proved indices 2..7 were
  all zero.

---

## 2. Slot choice: PA5, and why the write is provably harmless on both boot paths

`PAT_IDX_WC = 5`. A leaf PTE selects it with `PAT=1, PCD=0, PWT=1` (index `0b101`).
The MSR write is a **read-modify-write of byte 5 only**, mirroring the aarch64 MAIR RMW.

The safety argument does not rest on "no one uses this slot". It rests on a case split
that covers both loaders:

* **Limine path** — PA5 is already `0x01`. The RMW computes
  `(before & ~(0xFF<<40)) | (0x01<<40)`, which for `before = 0x0000010500070406` is
  `before` **unchanged**. A value-identical `wrmsr` cannot reinterpret a single live
  translation, including Limine's own PAT-bit framebuffer mapping. Nothing to prove
  about who is using PA5, because nothing about PA5 changes.
* **Direct-boot path** — PA5 goes WT → WC. Here the slot provably has no users: a PTE
  only reaches PA4..PA7 by setting the PAT bit, and setting that bit without having
  programmed `IA32_PAT` is precisely what Limine's own `test edx, 0x10000` guard exists
  to avoid; our kernel has never set it (this patch is its first user); and the
  direct-boot page tables built at `entry_x86_64.s:133` are 2 MiB PDEs written as
  `0x00000083` — PS set, bit 12 (the PDE's PAT bit) clear.

Both branches leave every live translation meaning exactly what it meant.

Rejected alternatives:

* **PA1 (Linux's slot).** Linux reprograms PA1 from WT to WC so WC is reachable without
  the PAT bit — convenient, because then 4 KiB and 2 MiB mappings use the same bits.
  Rejected here: PA1 is selected by `PWT` alone, and we inherit Limine's page tables
  wholesale. We cannot grep a binary's PTEs, so we cannot rule out an inherited WT
  mapping that would silently become WC.
* **PA4 / PA6 / PA7.** All are reachable only via the PAT bit, so all are equally
  "unused by us" — but under Limine all three hold values Limine chose (WP/UC/UC), and
  clobbering one would be a real change with no way to prove it inert. PA5 is the only
  slot where the write is a no-op on the primary path.

---

## 3. Large pages: the PAT bit is bit 7 in a PTE and bit 12 in a PDE

This is handled **structurally**, not by care:

* `map_4k` is the only function in the tree that writes a mapping, and it always writes
  a leaf at the **PT** level. There is no 2 MiB mapping path in the kernel at all.
* `ensure_table` masks intermediate entries down to `PRESENT|WRITABLE|USER` before
  storing them, so the PAT bit cannot leak from `translate_flags` into a PDPTE/PDE where
  the same bit 7 means PS.
* The only 2 MiB leaves that exist are the ones `entry_x86_64.s` builds (`0x83`) and
  whatever Limine builds for the HHDM. We never re-flag either.
* Grep confirms nothing outside `arch/*/paging.rs` inspects a page-table entry, and the
  only reader of bit 7 anywhere (`ensure_table`, via `PageTableFlags::HUGE`) runs at
  non-leaf levels only.

The patch adds `PageTableFlags::PAT_4K = 1 << 7` as a deliberate alias of `HUGE`, with a
comment stating the aliasing invariant and that a future 2 MiB path needs its own
bit-12 constant rather than this one. bitflags 2.11.1 permits duplicate-valued flags.

---

## 4. Cache-flush protocol

SDM Vol 3A §11.12.4 defers PAT changes to §11.11.8, the MTRR change protocol. The patch
implements it in `write_pat` **minus** two steps, each omitted with a stated reason:

Implemented: save/clear IF → clear `CR4.PGE` if set → `wbinvd` → `wrmsr 0x277` →
`wbinvd` → `mov cr3, cr3` → restore `CR4.PGE` → restore IF.

Omitted, with justification:

* **No-fill cache mode (`CR0.CD=1, NW=0`).** Its job is to stop *this* CPU filling lines
  under a type that is mid-change. That requires holding cached data under a changing
  slot — which never happens here: on the BSP under Limine no slot changes at all, and
  on the direct-boot path and on every AP the changing slots have no PTE selecting them
  at that instant (§2, §5). The `wbinvd` pair covers the residual. `CR0.CD=1` is also
  not free under virtualisation — KVM treats it as an MMU-wide memory-type event
  (`KVM_X86_QUIRK_CD_NW_CLEARED`) — so paying for it here adds risk rather than removing
  it.
* **The MP rendezvous.** Replaced by structural ordering; see §5.

`CR4.PGE` is cleared across the CR3 reload because `mov cr3, cr3` does not invalidate
global pages and the loader may well have marked its own mappings global. This is the
one step that is *cheaper* to do than to argue away, so it is done.

---

## 5. SMP — and the pre-existing divergence this closes

`IA32_PAT` is per-logical-processor, and an AP leaves INIT/SIPI with the **reset** PAT.

**This is a live defect on `main` today, independent of this patch.** On the Limine
path the BSP inherits `PA4=WP, PA5=WC, PA6=UC` while every AP runs with `PA4=WB,
PA5=WT, PA6=UC-`. Limine's framebuffer mapping selects PA5. `arch::init` tries to
re-map the framebuffer with `NO_CACHE`, but does so with `map_4k`, which returns `false`
the moment it meets one of Limine's huge pages — the framebuffer loop in `arch::init`
says so in a comment and then ignores the result. The kernel console writes the
framebuffer through `mm::phys_to_virt(info.base)` (`drivers/src/framebuffer.rs:653`),
i.e. Limine's HHDM mapping. If that re-map fails, the console is **WC on the BSP and WT
(cached) on every AP** — the same physical lines under two memory types on two
processors, which the SDM leaves undefined. It has not bitten us because WT is coherent
and the console is idempotent, but it is exactly the shape of bug that shows up as rare
corruption rather than a fault.

The patch therefore has each AP adopt the BSP's **entire 64-bit PAT**, not just patch
PA5 — `PAT_AFTER` is published by the BSP and written verbatim by each AP. All CPUs
become bit-identical.

The ordering constraint, and how it is satisfied:

* **BSP**: `paging::init_pat_bsp()` is the first statement of `arch::init()`, ahead of
  the GDT. It needs nothing but `rdmsr`/`wrmsr` and the I/O-port UART, so unlike
  aarch64 (where the MAIR value had to be stashed until the UART was mapped) it can
  print inline. It runs before `arch::init` maps the LAPIC or the framebuffer, and
  before `smp::smp_init`, which is the last statement of `arch::init`.
* **AP**: `paging::init_pat_ap()` is the first statement of `smp::sched_ap_entry`,
  ahead of `apic::init`. At that point the AP has executed only the trampoline and has
  touched nothing but its stack and the parameter block — both write-back through PA0,
  a slot that is WB in the reset PAT and in Limine's, and which this never changes. So
  the AP holds no cached data under any slot that is about to change.
* **First consumer**: the first mapping that can select a reprogrammed slot is created
  by `sys_mmap` for a DRM blob, which cannot happen until user space runs — long after
  every AP is online with the same PAT. There is no CPU hotplug.

---

## 6. MTRRs

They do still combine with PAT, and the guest programs none, so whatever the firmware
left applies. For the range that matters — a virtio-gpu host-visible blob, which lives
in the device's 64-bit prefetchable BAR above top-of-RAM — SeaBIOS/OVMF leave
`MTRRdefType = UC` with variable ranges marking RAM WB. **So the MTRR type for our BAR
is UC, and the row we depend on is (MTRR=UC, PAT=WC).**

SDM Vol 3A Table 11-7 gives WC for that row. Two independent corroborations, because a
table row recalled from memory is not evidence:

* Linux's `arch_phys_wc_add()` returns success without adding any MTRR when
  `pat_enabled()`. Every DRM WC framebuffer and every `pci_ioremap_wc_bar()` on Linux
  therefore gets its WC purely from PAT over an MTRR-UC MMIO range. If MTRR UC
  dominated, that would be broken everywhere, on both vendors.
* Linux's `pat_x_mtrr_type()` consults MTRRs **only** when the request is WB; a WC
  request is passed through untouched.

Virtualisation is the sharper question, and it cuts our way on the box that matters:

* The verification host is a **Ryzen 9 7950X**, so SVM/NPT. AMD nested paging carries no
  memory-type field; the guest's PAT is used directly (KVM's `svm_get_mt_mask` returns
  0), combined with the host's own type for that HVA, taking the more restrictive.
  WC ∧ WB → WC.
* On Intel/VMX this depends on `vmx_get_mt_mask`. Older KVM forces EPT WB with
  `IPAT=1` — "ignore guest PAT" — for non-MMIO memslots, which would make *any* guest
  PTE cacheability choice a no-op. Linux 6.10+ honours guest PAT on self-snoop CPUs.
  Note this is not a new exposure: the same condition governs whether `18a7a9f`'s UC
  mapping takes effect, and that fix demonstrably worked on this host, which is direct
  evidence that guest PAT is honoured here.

**Honest bottom line:** MTRRs cannot defeat this on the hardware we verify on, and the
one configuration that could (an old-KVM Intel host with EPT `IPAT=1`) would equally
defeat the already-landed UC fix, so it is not a regression introduced here. The
`wc=` field in the boot print (§7) reports whether the CPU accepted the PAT write; a
host that ignores guest PAT at the EPT level will not be caught by that read-back, and
the micro-benchmark in §7 is the check that would catch it.

---

## 7. Runtime checks another lane can run (I have no QEMU)

**(a) The one-line boot print — the direct analogue of the `MAIR_EL1` print.**
`init_pat_bsp` emits, before anything else on x86-64:

```
[ARCH] IA32_PAT before=0x0000010500070406 after=0x0000010500070406 wc=1
```

Falsifiable predictions, in order of what they would tell you:

| observation | meaning |
|---|---|
| `before=0x0000010500070406`, `after` identical, `wc=1` | expected on the Limine path. Proves §1's binary decode against a live register, and proves the MSR write is inert there. |
| `before=0x0007040600070406`, `after=0x0007010600070406`, `wc=1` | expected on the direct-boot path (`target/final-x86_64/kernel-direct`). |
| any other `before` | §2's case split needs re-checking against the actual value before landing. |
| `wc=0` | the CPU or hypervisor refused the write; the kernel has fallen back to PCD/UC-, i.e. exactly `18a7a9f`'s behaviour. Not a failure, but worth reporting. |

Grep for it with `grep 'IA32_PAT' <serial log>`. One line, both boot paths, no flag.

**(b) Confirm the framebuffer divergence in §5 — cheap, uses code that already exists.**
`paging::debug_walk_pte(cr3, mm::phys_to_virt(framebuffer_base))` prints the whole
chain. If it reports `(2MB page)` with bit 12 set, the framebuffer is on Limine's WC
mapping and §5's divergence was live before this patch. If it reaches a `PT[...]` entry
with `0x10` set, the `arch::init` re-map succeeded and the framebuffer is UC- on all
CPUs and was never divergent. Either answer is worth recording.

**(c) The check that actually measures the win.** Neither (a) nor (b) proves WC is
*faster*, and on an EPT-`IPAT` host both would pass while WC did nothing. Time a
`memcpy` of ~1 MiB into a mapped host-visible blob from `vkrender`, once on this build
and once with `PAT_WC_READY` forced false (a one-line local edit). UC should be
roughly 20-50× slower. If the two are within noise, guest PAT is not reaching the
hardware and this patch is inert — which is a finding, not a bug.

---

## 8. Source-level verification done here (no QEMU)

Disassembly of the built x86-64 release kernel confirms the intent survived codegen.

`init_pat_bsp`, inlined into `kernel_main`:

```
cpuid                              ; eax=1
btl  $0x10, %edx                   ; CPUID.01H:EDX.PAT
jb   .have_pat                     ; else -> pat_print(0,0,false)
movl $0x277, %ecx ; rdmsr          ; before
movabsq $0xFFFF00FFFFFFFFFF, %rax  ; ~(0xFF << 40)  -> byte 5 only
andq %rbx, %rax
leaq 0x10000(%r14), %rdi           ; r14 = 0xFFFFFF0000 -> 0x10000000000 = 1 << 40
orq  %rax, %rdi                    ; want = (before & mask) | (WC << 40)
callq write_pat
movl $0x277, %ecx ; rdmsr          ; read back
andl $0xff00, %edx ; cmpl $0x100   ; byte 5 == 0x01 ?
sete %al ; movb %al, PAT_WC_READY
```

`write_pat` is a real out-of-line function containing both `CR4.PGE` arms, the
`wbinvd` / `wrmsr` / `wbinvd` / `mov cr3,cr3` sequence, and the IF save/restore.

`arch_map_page`'s translated flags:

```
testb $0x30, %cl        ; NOCACHE(1<<4) | MMIO(1<<5)
jne   -> orq $0x10      ; PCD, unchanged
testb $0x40, %cl        ; WRITECOMBINE(1<<6)
je    -> done
movb  PAT_WC_READY, %al
testb %al, %al
je    -> orq $0x10      ; fallback: PCD/UC-
orq   $0x88             ; PAT(1<<7) | PWT(1<<3)  = index 0b101 = PA5
```

`0x88` is the whole point: index 5, and the fallback arm is UC- and never WB.

---

## 9. Build results

Release only, both architectures, all four kernel variants the build script produces:

| target | linker | result |
|---|---|---|
| x86_64 standard (Limine) | `linkers/x86_64.ld` | **Finished `release`** |
| x86_64 direct boot | `linkers/x86_64-direct.ld` | **Finished `release`** |
| aarch64 standard (Limine) | `linkers/aarch64.ld` | **Finished `release`** |
| aarch64 direct boot | `linkers/aarch64-direct.ld` | **Finished `release`** |

No new warnings. The two `unnecessary unsafe block` warnings in `smp.rs:100` and
`smp.rs:102` are pre-existing — they are `smt_shift`'s `__cpuid` calls, which sit above
every line this patch touches, so the reported line numbers are unchanged by it.

aarch64 is untouched by construction: `kernel/Cargo.toml` gates `arch-x86_64` behind
`[target.'cfg(target_arch = "x86_64")'.dependencies]`, so the crate is not even compiled
for aarch64. Every new asm/CPUID site is additionally `#[cfg(target_arch = "x86_64")]`,
matching the file's existing style, and `pat_wc_ready()` has a `#[cfg(not(...))]`
arm returning `false`.

`rustfmt --check` on all three touched files exits **1** (formatting diff), not 2/3
(parse error) — they parse. `cargo fmt --check` and `clippy` are not gates here per the
lane brief.

---

## 10. File ownership

Touched, in full:

* `arch/x86_64/src/paging.rs`
* `arch/x86_64/src/lib.rs`
* `arch/x86_64/src/smp.rs`

Nothing under `drivers/`, `servers/net/` or `servers/vfs/` — **no overlap with any
other lane**. The patch does not touch `mm/src/paging.rs` either, so it stacks with
anything that does.

---

## 11. Residual risks and follow-ups

1. **The `before` value is a static-analysis claim until (a) runs.** The decode in §1 is
   unambiguous, but it is a decode of a binary, and Limine could in principle restore
   PAT before handing off. The patch is safe either way — that is the point of the case
   split in §2 — but the print should be read before this is called settled.
2. **WC is weakly ordered where UC was not.** Stores to WC memory are only flushed at
   serializing events. This moves us *toward* the reference behaviour rather than away
   from it: the host explicitly asked for WC (`VIRTIO_GPU_MAP_CACHE_WC`), so Mesa's
   Venus path is already written against WC semantics on native Linux, and its ring
   submission goes through a locked atomic, which drains the WC buffers. Worth knowing
   if a blob ever gets a new consumer.
3. **No WB alias exists to conflict with.** Limine base revision 1+ does not map MMIO in
   the HHDM (noted at `arch/x86_64/src/lib.rs:45`), and `18a7a9f` scoped the flag to
   host-visible blobs precisely because guest-backed blobs are the ones the kernel
   memcpys through `phys_to_virt`. So there is no second mapping of these pages at a
   different type.
4. **§5's framebuffer divergence deserves its own TODO entry** whether or not this
   patch lands, because it is pre-existing and this patch fixes it only incidentally.
5. **`translate_flags` precedence** now reads MMIO/NOCACHE first, then WRITECOMBINE,
   matching aarch64 exactly. Today nothing sets both (`map_device` passes `mmap`'s flags
   through unchanged and `sys_mmap` ORs in only `WRITECOMBINE`), so the ordering is not
   exercised — it is there so a device BAR stays strongly ordered if a future caller
   passes both.
