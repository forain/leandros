# lane nvprobe — read-only NVIDIA RTX 3070 (GA104) probe, G1

Branch `lane/nvprobe`. Module: `drivers/src/nvidia_probe.rs`, called from
`kernel/src/init.rs` just before `blkdev::init()`. Silent no-op unless the PCI
bus has a class-0x03 function with vendor 0x10de.

## What it does (all lines are `[NVPROBE] ...` in the normal kernel log)

1. Config space: BDF, vendor/device/subsystem, revision, class, COMMAND/STATUS.
2. BAR sizing with memory+IO decode off and local IRQs masked, every register and
   COMMAND restored (log line `COMMAND restored after BAR sizing`). Type 32/64,
   prefetchable, base, size, `UNASSIGNED` when base is 0. Expansion-ROM BAR too.
3. Capabilities: power management, MSI, MSI-X (table size), PCIe (current/max link
   speed+width), and the extended list (ECAM): every extended cap id, with the
   Resizable BAR entries decoded (current size, supported sizes).
   - x86_64: legacy 0xCF8 only reaches 256 bytes, so the probe maps the function's
     ECAM page via ACPI MCFG (`boot::acpi::find_ecam_base`), validated against
     legacy vendor/device. No MCFG / unmappable -> `extended config unavailable`.
   - aarch64: uses the existing ECAM base.
4. BAR0 (16 MiB, MMIO/uncached via `map_kernel_device`, like virtio): PMC_BOOT_0
   (0x0), PMC_BOOT_42 (0xA00) decoded as nouveau does (chipset = bits 28:20,
   GA104 = arch 0x17 impl 0x4 = 0x174, revision = low byte), PMC_ENABLE (0x200),
   PTIMER 0x9400/0x9410 read twice 100 us apart, and VRAM size from
   0x100ce0 (NV_PFB_PRI_MMU_LOCAL_MEMORY_RANGE: `mag << (scale+20)`, mag=bits 5:0,
   scale=bits 9:6, ecc=bit 30) — best effort, raw value is logged too.
5. VBIOS: PROM window BAR0+0x300000 (1 MiB) with the ROM-shadow bit cleared exactly
   as nouveau's shadowrom.c does (`nvkm_pci_rom_shadow(pci,false)`: config dword
   0x50 bit 0). The original dword is written back and logged. Fallback: expansion
   ROM BAR (enable bit toggled, restored; skipped if firmware left it unassigned).
   Validates 0x55AA, walks PCIR images (type: x86 / EFI / NVIDIA 0xE0, `last`
   flag), finds the BIT table and logs every token, logs declared size, bytes
   read, CRC32, and a 256-byte hex dump.
6. Framebuffer: `GOP framebuffer at BAR<n>+0x<off>` comparing Limine's fb phys with
   every GPU BAR (this is the VRAM offset a future display path will blit into).
7. Safety: bounded by a 600 ms read budget (5 s with dumpvbios); a truncated read
   is reported `TRUNCATED by time budget` and the CRC covers only what was read.
   Unassigned/unmappable BAR0 -> logged and skipped; 0xFFFFFFFF from BOOT_0 ->
   logged and skipped. The only GPU-visible writes: BAR sizing (restored), the
   shadow bit (restored), memory-decode enable only if firmware left it off.

## Kernel command line (Limine `cmdline:` in limine.conf; Limine revision untouched)

New `ExecutableCmdlineRequest` in `kernel/src/main.rs`. Options:

| option | effect |
| --- | --- |
| `nvprobe.off=1` | skip |
| `nvprobe.dumpvbios=1` | also print raw VBIOS base64 between `-----BEGIN VBIOS-----` / `-----END VBIOS-----` (76-col lines), 5 s budget. Off by default: ~1.4 MB through a 115200 UART/console is slow, and it floods the log. Decode with `base64 -d` after stripping `[NVPROBE]` lines (the base64 lines carry no prefix). |
| `nvprobe.vendor=0x1af4` | test mode: probe that vendor instead; no BAR0/PROM access, expansion ROM only |
| `nvprobe.romaddr=0x..` | test mode: temporarily place an unassigned ROM BAR there |

## QEMU validation (no NVIDIA available)

Ran with `cmdline: nvprobe.vendor=0x1af4 nvprobe.romaddr=0x80850000 nvprobe.dumpvbios=1`
(temporary, not committed) on virtio-vga (x86_64 q35) and virtio-gpu-pci (aarch64):
BAR sizing restored COMMAND (0x0007), expansion ROM BAR restored to its original
0xffff0000, boot continued to login on both. ECAM path worked on both arches.
x86_64 virtio-vga ROM: one image, 0x55AA, PCIR type 0, 39424 B, CRC32 0x5d5c49a1;
the base64 dump decoded back to the identical CRC. Default boots (no cmdline):
nvprobe prints nothing.

Not covered by QEMU: the PROM window / ROM-shadow path, BAR0 register decode, BIT
table walking (virtio ROM has no BIT), multi-image PCIR chains. Those are
exercised for the first time on the real PC.

## What the user should look for in the log (real PC)

Grep `NVPROBE`. In order:
- `device 01:00.0 vendor=0x10de device=0x2484|0x2488 ... class=030000` — the GPU was
  found. Also check the `0x228b` audio function is NOT picked (it is class 04).
- `BARn: ... size=` — expect BAR0 mem32 16 MiB, BAR1 mem64 prefetchable 256 MiB
  (ReBAR off) or 8 GiB, BAR3 mem64 32 MiB, BAR5 io 128 B. Any `UNASSIGNED` is a
  finding (firmware above-4G decoding off?).
- `ext cap 0x0015` / `ReBAR entry` lines — shows supported sizes; if supported
  includes 8 GiB the BIOS can enable Resizable BAR.
- `PCIe link: current ... max ...` — a current speed below max can be idle
  downtraining; width x16 expected on a x16 slot.
- `GOP framebuffer at BAR1+0x...` — expect offset 0 (or small). This is the target.
- `PMC_BOOT_0 = 0x174000a1`-like: `chipset=0x174 GA104 (Ampere)`. `PTIMER ... ticks=yes`
  proves BAR0 is live. `NO` or `0xffffffff` means decode/power problem.
- `VRAM (best effort ...)` — should read 8 GiB; treat as unverified if odd.
- `PROM: config 0x50 was ...` then `VBIOS image 0..N` lines: expect x86 (type 0),
  EFI (type 3) and NVIDIA (0xE0) images, `last=1` on the final one, a `BIT table`
  with tokens, and `CRC32`. `TRUNCATED` means the 600 ms budget was hit (re-run
  with `nvprobe.dumpvbios=1`, 5 s budget).
- `=== nvprobe done in N ms ===` — should be well under 1000.

If the stick log partition slot is >= 16 MiB, set `cmdline: nvprobe.dumpvbios=1`
once to capture the VBIOS for offline analysis.
