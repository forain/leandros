# Lane pibrcmf-core (2026-10-26): CYW43455 chip + protocol layer

Status: DELIVERED as host-tested library code. Nothing calls it at boot yet, so it is inert on
virt and on the Pi until the integration step wires it to pisdio's `impl SdioBus`.

## What landed
- `drivers/src/brcmf/` with the frozen `mod.rs`, `bus.rs` and `fwil.rs` exactly as the wave
  brief gives them, plus one-line `//!` stubs for `sdio_host.rs` (pisdio) and `cfg.rs`,
  `event.rs`, `netdev.rs` (pibrcmf-net). `drivers/src/lib.rs` gets `pub mod brcmf;`.
- `chip.rs`: `Backplane` (F1 SBADDR window with caching, 32-bit read/write with the 2_4B flag,
  RAM read/write split at 32 KiB window boundaries). `Chip::attach` does what brcmfmac's
  probe_attach does: F1 enable and 64-byte blocks, ALP request with CHIPCLKCSR readback, chip id
  (AI, 0x4345 only), EROM scan (`brcmf_chip_dmp_erom_scan`: master/slave wrappers, 64-bit
  address and size descriptors), required cores CC/CR4/SDIO/D11, CR4 TCM sizing from the bank
  registers, rambase 0x198000, SR-capable probe (PMU chipcontrol 3 bit 2), WLANRESET in
  CARDCTRL, PMU resource reload, F2 off, clock dropped. AI core disable/reset, CR4
  passive/active, `clk_request` (ALP/HT, 15 ms bound), `kso_control`, `sr_init`.
- `fw.rs`: `nvram_normalize` (brcmf_fw_nvram_strip: comments, CR, invalid lines, `boardrev=0xff`
  default, double NUL, 4-byte pad, `(~words << 16) | words` token). `download(bus, chip, code,
  nvram_text)`: KSO wake, HT, CR4 halted + D11 reset, code at rambase, nvram at the top of RAM,
  256-byte read-back of both ends, reset vector at backplane 0, CR4 release, then F2: FORCE_HT,
  tosbmailboxdata = 4 << 16, F2 512-byte blocks, IOEN and IORDY (3 s bound), hostintmask,
  watermark 8, IENx = 0x07, SR init. `clm_load(fwil, blob)`: "clmload" in 1400-byte chunks
  (DL_BEGIN/DL_END, handler version 1, type CLM), then "clmload_status".
- `sdpcm.rs`: header pack/parse, tx credit (`data_ok`), 4-byte / block padding, F2 tx and rx at
  the ChipCommon window with fixed addressing, a 64-byte first read and then the rest from the
  tag length, rx abort (CCCR abort + RF_TERM), intstatus W1C, FC change, host mailbox (FWHALT,
  DEVREADY/FWREADY version, FC bits, NAKHANDLED), sequence and window bookkeeping. Glom frames
  are dropped; glomming is never requested.
- `bcdc.rs`: `Brcmf<B: SdioBus>` owns bus + chip + SDPCM state and implements `Fwil`. dcmd
  header with the request id, a 1518-byte tx limit, a 2.5 s response wait, stale responses ignored,
  error flag mapped to `FwError::Status`. Data tx carries the BCDC v2 header. Event and data frames
  that arrive while an ioctl waits are queued (64 max) and handed out by `poll`. Frames with
  ethertype 0x886c go to `on_event`, the rest to `on_data`.

## Tests
`cargo test --manifest-path drivers/brcmf-tests/Cargo.toml --target aarch64-apple-darwin`:
22/22. The mock (`drivers/brcmf-tests/src/mock.rs`) is a scripted 43455: an EROM table, AI
wrappers, CR4 bank registers, SDIO core intstatus/mailboxes, an 800 KiB TCM array, and a fake
firmware that "boots" only when the reset vector at 0 matches the image and the nvram token is
valid. It then raises FWREADY and answers BCDC requests ("ver", "clmload", "clmload_status",
WLC_GET_VERSION; anything else gets BCME_UNSUPPORTED). `download_then_ver_iovar` runs the
whole path from attach through download to the "ver" iovar.
I checked that the guards catch real breakage by mutating the code: with no window cache, no NUL
after the default boardrev, or no RF_TERM on rx failure, `window_is_cached`,
`nvram_normalisation` and `bad_frame_is_dropped_and_rx_recovers` fail.
`scripts/check-rpi5.sh` (rpi5 and raspi4b) is clean, with no brcmf warnings. aarch64 virt regress
(runtests default suite, tag pibrcmfcore): overall PASS, 0 boots failed.

## Integration notes (for the step that wires pisdio + this + pibrcmf-net)
- Firmware bytes: `crate::firmware::request_firmware(path, max)` returns a page list
  (`Firmware::chunks()`), not a slice. The simplest way to get a slice is to collect into a `Vec<u8>`
  (about 600 KiB) or use `read_firmware_chunked`. Paths are `fw::FW_PATH`, `fw::NVRAM_PATH` (falling
  back to `NVRAM_FALLBACK_PATH`) and `fw::CLM_PATH`, all under /lib/firmware/brcm (linux-firmware names).
- Order: card init (pisdio: CMD5/3/7, 4-bit, clock) → `Chip::attach` → `fw::download` →
  `Brcmf::new(bus, chip)` → `fw::clm_load(&mut dev, clm)` → "ver" → cfg.
- `attach` itself enables F1, and `download` enables F2 and IENx. Both are idempotent, so it
  doesn't matter if the host already did this.
- Probe lines suggested for the boot path: `[brcmf] chip 4345 rev 6 cores N ram 0xc8000@0x198000 sr 1`,
  `[brcmf] fw <len> nvram <len> rstvec 0x... F2 ready`, `[brcmf] clm N chunks`, `[brcmf] ver <string>`;
  on failure print one line with `ChipError::as_str()` or `FwLoadError::as_str()`.

## Open
- Not hardware-verified. Real-chip unknowns: whether the 43455 needs the D11 core held
  disabled instead of reset (newer brcmfmac only disables it; we use the long-standing
  resetcore variant), and whether F2 IORDY takes longer than 3 s.
- Glom/superframes are not parsed. No idle KSO sleep: HT stays forced.
