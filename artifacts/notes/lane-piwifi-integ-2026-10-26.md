# lane piwifi-integ — 2026-10-26

Status: DELIVERED up to the firmware "ver" (not hardware-verified). pibrcmf-net had not merged,
so wifi0 registration and wifi.conf autoconnect are NOT wired here.

## What
- `drivers/src/brcmf_boot.rs` (rpi5 only): `start(cmdline)` runs right after the V3D probe in
  `kernel/src/init.rs`. It takes pisdio's parked host (`sdio_host::take_host()`), reads the fmac
  node's `local-mac-address` from the boot DTB, and spawns kthread `brcmf`, which waits for
  `firmware::root_ready()` (≤ 120 s) and loads all three files before touching the chip:
  firmware, nvram (with fallback), CLM. Then it runs attach → download → `Brcmf::new` →
  `clm_load` → `cur_etheraddr` (DTB MAC; it keeps the nvram placeholder if the DTB has none) → "ver".
- The ready device is parked: `brcmf_boot::take_device() -> Option<Brcmf<SdioHost<Mmio>>>`,
  with `state()` (0 none, 1 skipped, 2 running, 3 ready, 4 failed) and `version()`.
- `fw::FW_PATH`/`CLM_PATH` now point at pifw's names: `cyw43455-sdio.{bin,clm_blob}`.
- `brcmf=0` on the cmdline skips the bring-up and leaves the host parked.

## Expected Pi lines (after the `[sdio]` line)
    [brcmf] chip 4345 rev 6 cores N ram 0xc8000@0x198000 sr 1
    [brcmf] fw 609309 nvram <len> rstvec 0x... F2 ready
    [brcmf] clm 2 chunks
    [brcmf] mac xx:xx:xx:xx:xx:xx dtb
    [brcmf] ver wl0: ... version 7.45.x ...
Or a single fail line: `fail firmware-missing <path>`, `fail no-sdio-host`, `fail chip <ChipError>`,
`fail fw <FwLoadError>`, `fail clm <FwError>`, `fail mac ...`, `fail ver ...`, `fail root-not-mounted 120s`.

## Evidence
- `scripts/check-rpi5.sh` passes for both rpi5 and raspi4b.
- aarch64 virt: runtests /bin/nettest PASS, 0 `[brcmf]` lines (inert).
- The rpi5 kernel-direct.bin contains the new strings.

## Open
- wifi0 netdev and autoconnect: these wait for pibrcmf-net. It can call `take_device()` once `state()==3`.
- MAC from OTP is not implemented; only the DTB MAC is used. If the DTB has none, the nvram placeholder is kept and logged.
- On a mid-chain failure the chip stays powered but idle; WL_REG_ON is not dropped, because sdio_host has no pub power-off.
