# lane/hhdmwalk (+ hhdmwalk2) — 2026-10-24 — PARTIAL (walker merged report-only; fill unproven)

openbugs L8.3 tail: aarch64 counterpart of the x86_64 HHDM gap check/fill.

## What is merged
- `boot/src/hhdm_a64.rs`: pure aarch64 4 KiB-granule TTBR1 walker (`va_bits_of`, `walk`, `fill`, `holes`,
  `clear_at`); 11 host unit tests (`cd boot/memmap-tests && cargo test`).
- `kernel/src/main.rs` `early_hhdm_a64`: on the Limine path, walks every memmap range Limine maps (all types but
  RESERVED/BAD/FRAMEBUFFER), reports usable RAM the TTBR1 tables do not map (yellow) and re-sanitizes it out of mm as
  Reserved. **Default is report-only**: the in-place fill runs only with `mm.hhdmfill=1` (or `2m`, no 1 GiB blocks).
- Test knob `earlyfb.test=unmap4g` (clear L1 HHDM entries >= 4 GiB that hold only usable RAM) and `unmap4g1`
  (only the GiB at 4 GiB); in that path `exception::init()` is installed first.

## Evidence
- 6 GiB default boot (HVF): boot completes, memtest 1/1 PASS (tests-hhdmw-def); Limine's HHDM is complete on QEMU, so
  nothing is reported or patched.
- aarch64 regress on the merged branch: see the merge commit's lane run (tests-hhdmw-reg).

## Open: why the fill path is unproven
- `earlyfb.test=unmap4g1` (one GiB, 0x1_0000_0000, which holds no Limine allocation) wedges on HVF with **no kernel
  serial line at all**: the serial log ends with Limine's "Loading module boot():/initrd.gz". Note that even on a good
  boot the first kernel line in the serial log is `[early] dtb ram scan`; the earlier `[early]` prints (including
  the `hhdm: mapped N ranges` summary) do not reach the serial port at that point, so the wedge cannot be localized
  from the log. Installing the kernel vectors did not produce an ESR report either (the report itself prints).
- TCG (`--mode uefi-tcg`, `-d int`): the kernel takes a translation fault (ESR 0x96000004, FAR 0x9000018) in
  `arch_aarch64::uart::putc_within` right after Limine's eret to the entry point — i.e. an early print uses the
  identity UART address before `mm_get_hhdm_offset()` is set, and TTBR0 has no L0 entry under TCG. VBAR is 0 then,
  so the CPU loops at 0x200. This happens before the knob code and blocks TCG as a debugging route; it is a separate
  bug worth its own item (repro: `runtests.py aarch64 x --mode uefi-tcg --cmdline "earlyfb.test=unmap4g1" /bin/memtest`
  with `LEANDROS_QEMU_EXTRA="-d int -D log"`, first Data Abort after the eret to 0xffffffffc0080040).
- Next steps: fix the early identity-UART print (route through the HHDM from entry), then repeat the TCG `-d int`
  run with `unmap4g1` + `mm.hhdmfill=1` to get the faulting PC; suspects remain a write through a cleared GiB before
  the fill (console/earlyfb state) and missing break-before-make / TLBI ordering in the fill.

## x86_64 gap (why the aarch64 work exists, still unexplained)
From artifacts/notes/lane-fbmm-2026-10-16.md: on the HP with Fast Boot OFF, Limine 11.4.1 handed over PML4E present /
PDPTE 0 above 4 GiB although the memmap reported 13 GiB usable. Limine's source maps every non-RESERVED/BAD entry,
so the cause is not in the code path we can read; the suspect is the EFI memory map being retyped at
ExitBootServices after Limine built the HHDM. It stays unexplained because it reproduced only on that one machine
with that one firmware setting and Fast Boot ON (the shipped configuration) does not show it. The kernel fills the
gaps in place on x86_64 (1d011eb2) and checks the result (9031d16c).
