# lane pibrcmf-fw2 (2026-10-26): CYW43455 post-download bring-up, diagnostics, runtime retry

Status: FIXED in host tests against a mock that reproduces the Pi failure; hardware unverified.

## Root cause (Pi boot3: `[brcmf] fail fw HT clock timeout`)
`fw::download` asked for the HT clock BEFORE writing the firmware, with a 15 ms bound.
brcmfmac downloads with `bus->alp_only = true` (ALP only) and asks for HT only in
`brcmf_sdio_firmware_callback`, after the CR4 is released, with `PMU_MAX_TRANSITION_DLY` = 1 s.
On a fresh 43455 the HT PLL resource is not available before the firmware runs, so the chain
died at the first clock request; nothing was ever written to TCM.

## What changed
- `fw::download_with(bus, chip, code, nvram, &Knobs)` in brcmfmac order: KSO (if SR) → ALP
  (`dl_ht=0`) → CR4 halt + D11 reset → code at 0x198000 + nvram (token last word) at the top of
  RAM, both read back → int clear, rstvec at backplane 0, CR4 released → clock request dropped
  (CLK_SDONLY) → HT request, 1 s → SR re-probed (PMU chipcontrol 3 bit 2; brcmfmac evaluates
  `brcmf_chip_sr_capable` after the download, so `sr 0` at attach is expected) → FORCE_HT,
  tosbmailboxdata = 4<<16, F2 512 B blocks, IORDY 3 s, hostintmask, watermark 8 (the Pi's
  func1 device id is 0x4345, not 0xa9bf, so brcmfmac uses DEFAULT_F2_WATERMARK, no
  DEVICE_CTL/MESBUSYCTRL writes) → `sr_init` (WAKEUPCTRL HTWAIT, CMD14 cardcap, FORCE_HT) or
  restore the saved clock → IENx = 7. Errors carry the stage (`DlFail { err, stage }`).
- `chip::PMU_MAX_TRANSITION_US` 15 ms → 1 s; `clk_request_t` takes an explicit bound.
- `fw::diag`: one line with CHIPCLKCSR, SLEEPCSR, IORDY, PMU control/status/res_state/pending/
  min/max masks, CR4 wrapper ioctl/resetctl/iost and halt|run, SDIO intstatus + tohost mailbox,
  the end-of-RAM word; if it is an sdpcm_shared pointer: version, flags, TRAP/ASSERT, console
  address, and the last N console lines (`[brcmf] con: ...`), read in ring order.
- RAM: 0xc8000 @ 0x198000 matches brcmfmac for 4345 (CR4 bank sizing + tcm_rambase).
  The `[brcmf] files ...` line prints the firmware's trailer string (feature list incl. `-sr-`,
  version, CRC) so the variant on the card is visible.
- Runtime retry: after a failure the `brcmf` kthread stays alive and polls `/etc/brcmf.ctl`
  once a second (content present at boot is ignored; a command runs when the content changes).
  `retry [k=v ...]` re-runs `sdio_host::probe` (host quiesce, WL_REG_ON low `wloff_ms`, high,
  `wlon_ms`, SDIO init, chip id) and the whole chain; on success the thread goes on to wifi0 as
  at boot. `diag [lines=N]` re-dumps the parked chip; `status`. `/bin/brcmfctl` (shell script,
  installed by mkfs-f2fs-populated.py) writes `seq=<n> <args>` to the file. Knobs: clk_ms dl_ht
  post_ms ht_ms f2_ms sr=auto|0|1 kso wm f2wm mesbusy force_ht noclm wloff_ms wlon_ms lines
  fw= nvram= clm=. Each retry starts from the defaults.
- `sdio_host::probe(off_us, on_us)`: boot_probe made re-runnable (mappings cached).

## Tests
- brcmf-tests 51/51 (was 44). Mock: HT only after the firmware runs + 50 ms, SR bit after the
  firmware runs, firmware leaves sdpcm_shared + a wrapped console ring, optional trap.
  New guards: `download_uses_alp_only_like_brcmfmac` (dl_ht=1, the old order, fails at
  `dl-clk-ht` exactly like the Pi), `ht_wait_bound_is_a_knob` (15 ms bound fails),
  `sr_reprobed_after_firmware_start`, `f2_watermark_knobs`,
  `diag_reads_the_console_of_a_trapped_firmware`, knob parsing, console tail.
- check-rpi5.sh rpi5 + raspi4b clean. aarch64 regress: see the merge report.

## Next Pi boot should show
    [brcmf] files fw /lib/firmware/brcm/cyw43455-sdio.bin 609309B nvram 2074B clm 2676B rstvec 0x... <trailer>
    [brcmf] chip 4345 rev 6 cores 7 ram 0xc8000@0x198000 sr 0
    [brcmf] fw 609309 nvram ...@0x... rstvec 0x... sr 1 F2 ready t=...ms
    [brcmf] clm 1 chunks / mac ... / ver wl0: ... 7.45.x
or `[brcmf] fail fw <err> stage=<kso|dl-clk-alp|passive|code|nvram|active|ht|sr-probe|f2>` +
`[brcmf] diag ...` + `[brcmf] con: ...` lines + `[brcmf] ctl: waiting for commands in /etc/brcmf.ctl`.

## Open
- Not hardware-verified. The SD root on the card must be re-prepared to get /bin/brcmfctl;
  with only a new kernel, write the file by hand: `echo "seq=1 retry ht_ms=3000" > /etc/brcmf.ctl`.
