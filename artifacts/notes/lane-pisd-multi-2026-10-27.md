# lane/pisd-multi — 2026-10-27 — PARTIAL (mitigation done; root cause needs one Pi boot)

## Problem
Pi 5 on main a678f983 (includes pisdperf 6df8608d): inbound SSH refused (RST from netd: no listener) although
sshd logs "listening on 0.0.0.0:22 ready"; ping and DHCP fine; no "multi-block failed" lines. 30ac675c works.
Without pcie only the sdhci changes differ.

## Change (b7e73b1f)
- Multi-block CMD18/CMD25 is OFF by default; `sdhci.multi=1` re-enables the pisdperf path (and its
  per-64 KiB cond_resched). With it off, span_io is exactly the old behaviour: one 4 KiB block (8 single
  sectors) per DEVICES hold, no reschedule point. do_io/do_io_sector/wait_normal_int/send_command/
  reset_cmd_dat/tick_window are byte-identical to 30ac675c.
- Boot line: `[sdhci] I/O mode: PIO single-sector CMD17/CMD24 (multi-block off; sdhci.multi=1 enables it)`
  or the old multi-block line with `(sdhci.multi=1)`.
- `sdhci.multitest=1` (read-only): at probe, reads the first 1024 blocks (4 MiB) of the partition single-sector
  and multi-block (spans of 16/1/7 blocks, the shapes span_io/read_block issue), one line:
  `[sdhci] multitest 1024 blocks (4 MiB, spans 16/1/7) crc single=X multi=Y multifails=N match|MISMATCH at block B span-sector S word W single=.. multi=.. badsectors=..`
  Runs regardless of sdhci.multi. Knobs are parsed in drivers::sdhci::configure (kernel init, before the probe);
  QEMU raspi4b has no DTB cmdline, so `LEANDROS_SDHCI_KNOBS="..."` at build time bakes them in.
- QEMU raspi4b: `multitest ... crc single=6ba81fc0 multi=6ba81fc0 multifails=0000 match`.

## Root cause: open, two hypotheses to split on the Pi
1. Silent multi-block read/write corruption on BCM2712 (multitest decides it for reads). Code review vs
   Linux sdhci.c/sdhci-brcmstb (bcm2712 = plain SDHCI, only USE_CARD_BUSY; no multiblock/ACMD12 quirks):
   register setup, auto-CMD12, block addressing and per-block BRR/BWR waits look spec-correct. The one PIO
   difference: Linux drains blocks while PRESENT_STATE Buffer Read/Write Enable is set after each BRR/BWR;
   we wait for a fresh BRR/BWR edge per block. That fails loudly (timeout -> fallback), not silently.
2. Timing: multi-block makes card I/O far faster (and adds cond_resched in span_io), which can reorder boot
   (netd vs sshd vs Wi-Fi bring-up). "sshd listening, netd has no listener" smells like a lost listen
   registration / netd restart race rather than corrupted bytes (corrupt binaries would crash, not refuse).
   If multitest matches but sdhci.multi=1 still breaks SSH, restart sshd on that boot: if SSH then works,
   it is a userspace startup race exposed by faster I/O, not an sdhci bug.
