# lane/pisound-design 2026-10-26: sound (and the rest) on the Raspberry Pi 5 (BCM2712 D0)

Status: DESIGN (no driver code). Inputs: `$SP/hw/rpi5-d0.dts`, the tree at `a4ce2266`, Linux
`raspberrypi/linux rpi-6.12.y` (`drivers/gpu/drm/vc4/vc4_hdmi.c`, `vc4_hdmi_regs.h`,
`drivers/dma/bcm2835-dma.c`, `drivers/bluetooth/btbcm.c`, `hci_bcm.c`) read as documentation,
and `lane-pv3d-scanout-2026-10-26.md`.

## TL;DR, recommended order

| # | lane | calls | needs | proves without hardware | first hardware line |
|---|------|------:|-------|-------------------------|---------------------|
| 1 | `pihdmi-probe` | ~30 | nothing | host tests: timing decode, CEA snap, N/CTS table | `[hdmiaud] hdmi0 hpd=1 hdmi_mode=1 1920x1080@60.00 pixclk=148500 kHz` |
| 2 | `usbisoch` | ~50 | nothing | aarch64 QEMU `qemu-xhci` + `usb-audio`, WAV capture | (laptop) `[uac] ...` after lane 4 |
| 3 | `pidma40` | ~40 | nothing | host tests: SCB builder, cyclic ring math | `[dma40] m2m ch10 65536 B ok` |
| 4 | `uac1` | ~50 | 2 | QEMU `sndtest all` -> WAV -> `sndwav.py --expect` PASS | laptop + any USB dongle: tone audible |
| 5 | `pihdmi-audio` | ~55 | 1, 3 | host tests: IEC958 packer, MAI/CRP register images | `sndtest all` audible on the HDMI sink |
| 6 | `uac2` | ~50 | 4 | host tests on captured UAC2 descriptors | laptop + a UAC2 DAC |
| 7 | `pibt-uart` | ~45 | nothing | host tests: H4 framer vs scripted controller | `[bt] hci reset ok lmp_subver=0x6119` |
| 8 | `pibt-fw` | ~40 | 7 | host test: real `.hcd` parsed, patchram transcript | `[bt] patched fw ... bdaddr=d8:3a:dd:..` |
| 9 | `bt-hci-scan` | ~50 | 8 | host tests: event parsers | `btctl scan` lists nearby devices |
| 10 | `pihvs-ro` | ~40 | 1 | host tests: dlist decoder | `[hvs] fw dlist ch0 1 plane 1920x1080 XRGB` |
| 11 | `pihvs-plane` | ~55 | 10, pv3d C+F | host tests: dlist writer | zero-copy flips, `[hvs] eof` as vblank |
| 12+ | `pikms-*` (4+ lanes) | | 11 | | mode set, hotplug, EDID, 2nd head |

On the Pi itself USB audio also waits for lane `piusb` (RP1 xHCI over pcie2). The shortest path to
**audible sound on this Pi** is therefore HDMI (lanes 1, 3, 5: nothing else to wait for), if the
HDMI sink has speakers. USB audio (2, 4) runs in parallel because it is the only one QEMU can test
end to end, and it gives the laptop and PC a second sound card at the same time.
**Questions for the user** (they decide what gets built first): does the Pi's HDMI display have
speakers? Is there a USB audio dongle or DAC in the house (UAC1 or UAC2)? Which Bluetooth device
matters (keyboard or mouse, or headphones)? Is there an I2S HAT?

## What exists today (the seam every backend plugs into)

- `servers/pipewire/src/lib.rs`: in-kernel audio server, `enum Backend { None, Virtio, Hda }`, with
  seven calls: `reconfigure_stream(id, freq, ch)`, `send_pcm_data(&[u8]) -> usize`,
  `tx_used_idx`, `queued_bytes`, `top_up_silence`, `recover_stream`, `info`. The pump runs on a
  100 Hz tick, so a backend needs a ring of at least about 40 ms of audio and a cheap way to read
  its play position. Selection: `snd.backend=auto|virtio|hda`; `init()` returns ENODEV when
  neither probes.
- `drivers/src/hda.rs` is the model to copy: a 64 KiB cyclic ring, monotonic `played`/`written`
  counters, a zeroed region behind the DMA, linear resample to 48 kHz, S16 stereo only.
- Test kit: `sndtest info|tone|lr|all`, `LEANDROS_AUDIO_WAV=<file>` in `run-qemu.sh`
  (`-audiodev wav`), and `.claude/skills/run-leandros/sndwav.py <wav> --expect`, which checks the
  L/R RMS, the 440/660 Hz tones and a 44.1 kHz segment.
- USB: `drivers/usb` has xHCI with control, bulk (MSC) and interrupt-IN pipes (`xhci/intr.rs`,
  harvested on the tick with try-lock only), hubs, and hotplug. **No isochronous support**
  (`context.rs` has no `EP_TYPE_ISOCH_*`, `ring.rs` has no `TRB_ISOCH`).
- Bluetooth: nothing (no HCI, H4, L2CAP or BlueZ-like code anywhere in the tree).
- Display on the Pi: firmware framebuffer through the mailbox (`rpi_mailbox.rs`), with v3d BOs
  copied in (`kms_v3d.rs`). We never touch HDMI, HVS or pixelvalve registers. `config.txt` has
  no `dtoverlay=vc4-kms-v3d` but does have `hdmi_force_hotplug=1`.

## 1. HDMI audio on BCM2712

### What Linux `vc4_hdmi` does (gen 6, `bcm2712_hdmi0/1_variant`)
Startup (`vc4_hdmi_audio_startup`): it refuses unless the sink is HDMI (`display->is_hdmi`), not
DVI, then writes `MAI_CTL = RESET|FLUSH|DLATE|ERRORE|ERRORF`. There is no `phy_rng_enable` on 2712.

Prepare (`vc4_hdmi_audio_prepare`):
1. `MAI_SMP`: N/M from `rational_best_approximation(audio_clock, fs)`. `audio_clock` is the
   `dvp` gate (`clock@7c700000`, `brcm,brcm2711-dvp`) whose parent is the **fixed 108 MHz**
   clock (phandle 0x11), so 108 MHz / 48 kHz = 2250 exactly.
2. `MAI_CTL = CHNUM(ch) | WHOLSMP | CHALIGN | ENABLE`.
3. `MAI_FMT = SAMPLE_RATE(code(fs)) | AUDIO_FORMAT(PCM)`.
4. `MAI_THR`: **D0 = `VC4_GEN_6_D`, which uses the `VC6_D_HD_MAI_THR_*` field layout**. C1 boards
   (`rpi5-c1.dts`) use the gen-5/6C layout. The values (0x10/0x10/0x1c/0x1c) are the same but the
   bit positions differ, so key them off `bcm2712_id.rs`.
5. `MAI_CONFIG = BIT_REVERSE | FORMAT_REVERSE | CHANNEL_MASK(mask)`, then `MAI_CHANNEL_MAP` from
   `vc5_hdmi_channel_map`.
6. `AUDIO_PACKET_CONFIG = ZERO_DATA_ON_SAMPLE_FLAT | ZERO_DATA_ON_INACTIVE_CHANNELS |
   B_FRAME_IDENTIFIER(0x8) | CEA_MASK(mask)`.
7. ACR (`vc4_hdmi_set_n_cts`): `N = 128*fs/1000` (6144 at 48 kHz) and
   `CTS = pixclk*N/(128*fs)` (148500 for 1080p60). Then `CRP_CFG = EXTERNAL_CTS_EN | N` and
   `CTS_0 = CTS_1 = CTS`. **This needs the pixel clock of the current mode.**
8. An audio InfoFrame goes into packet RAM (`vc4_hdmi_write_infoframe`: packet id = type - 0x80 =
   4, `VC4_HDMI_PACKET_STRIDE`, the enable bit in `RAM_PACKET_CONFIG`). This is a
   read-modify-write: the firmware's AVI InfoFrame (id 2) must survive.

Data path: the DAI accepts only `SNDRV_PCM_FMTBIT_IEC958_SUBFRAME_LE`. Every sample is a 32-bit
IEC958 subframe: preamble nibble, 24-bit audio (S16 goes in bits 12..27), V, U, C (channel-status
bit `frame % 192`) and even parity P. On Linux, alsa-lib's iec958 plugin builds these. **We
must build them in the backend.** It is a pure function, easy to host-test against vectors from
alsa-lib's `pcm_iec958.c`. Channel status: consumer, PCM, the fs code in byte 3, word length in
byte 4.

DMA: `dmas = <&dma40 0x41fa000c>` (hdmi0) / `0x41fa000d` (hdmi1). In `bcm2835-dma.c` the whole
cell becomes `c->dreq`: the low 5 bits are `PER_MAP` (DREQ **12 / 13**), and the upper bits feed
the DMA40 CS QOS/panic-QOS and TI flags (`BCM2711_DMA40_CS_FLAGS`, wait-resp, burst). Port lines
~360-395 and ~760-770 verbatim. The destination is `MAI_DATA` in the shared `hd` block
(`reg-names "hd"` = `0x7c720000`, CPU 0x10_7c72_0000): hdmi0 at +0x1c, hdmi1 at +0x3c. The
access is 4 bytes wide, maxburst 2, fixed destination, incrementing source. `dma40@0x10_0001_0600`
channels the ARM may use: `brcm,dma-channel-mask = 0x7c0` = **6..10** (SPIs 0x56..0x5a, which is
GIC 86..90 + 32). Use channel 10 for audio and leave 6..9 alone. The ring is a cyclic list of
40-bit SCBs (`struct bcm2711_dma40_scb`). The play position comes from the channel's current
source address, which is our LPIB.

Other register facts (vc6 hdmi0; hdmi1 has the same `hdmi`-block offsets but MAI_CTL/DATA/SMP at
HD+0x30/0x3c/0x40):
`hd`: DVP_CTL 0x00, MAI_CTL 0x10, MAI_THR 0x14, MAI_FMT 0x18, MAI_DATA 0x1c, MAI_SMP 0x20,
FRAME_COUNT 0x60. `hdmi` (0x7c701400 / 0x7c706400): FIFO_CTL 0x7c, MAI_CHANNEL_MAP 0xa4,
MAI_CONFIG 0xa8, AUDIO_PACKET_CONFIG 0xc0, RAM_PACKET_CONFIG 0xc4, RAM_PACKET_STATUS 0xcc,
CRP_CFG 0xd0, CTS_0/1 0xd4/0xd8, SCHEDULER_CONTROL 0xe8, HORZA/B 0xec/0xf0, VERTA0/B0
0xf4/0xf8, HOTPLUG 0x1c8. Packet RAM is the `packet` range (0x7c703800 / 0x7c708800). HPD
interrupts go to `aon_intr` (l2 intc at 0x7d510600), lines 7/8 (hdmi0) and 14/15 (hdmi1).
Both `hdmi@` nodes are `status = "disabled"` in the DTB because Linux would claim them only with
the KMS overlay.

### Is it possible while the firmware owns the mode? Probably yes, with three conditions
Audio (MAI, ACR, audio packets, packet RAM) is independent of timing and PHY. Linux sets it up
after the mode set and touches no timing register. Nothing on the audio path needs a mode set by
us. The conditions, each answered read-only by `pihdmi-probe` on one boot:
1. **The link is in HDMI mode, not DVI.** The firmware chooses this from the EDID. Read it in
   `SCHEDULER_CONTROL` (manual-format/HDMI bit). If it is DVI, add `hdmi_drive=2` to
   `config.txt` and re-check.
2. **The pixel clock is known.** Nobody tells us the mode. Measure it: htotal x vtotal from
   HORZA/B and VERTA0/B0, times the frame rate from the `hd` FRAME_COUNT delta over a timed
   100 ms window (no IRQ needed). Snap the result to the CEA list (25.2/27/74.25/148.5/297/594 MHz,
   and the /1.001 variants) and take N/CTS from the HDMI-spec table. Cross-check with mailbox
   `GET_CLOCK_RATE` for clock ids 9 (PIXEL), 13 (M2MC = HDMI hsm) and 14 (PIXEL_BVB).
3. **The audio clock gate and reset are released.** Read the `dvp` block (`0x7c700000`, 0x10 bytes:
   SW_INIT 0x04, MISC_CONFIG 0x08 as in `clk-bcm2711-dvp.c`) before touching `hd`. If the gate is
   closed, opening it is lane 5's first write. Read the `hd` registers only under the survivable
   SError guard from `pv3dpwr2`.

Known risk: if the firmware re-trains HDMI on a hotplug, our audio setup is lost. Lane 5 re-checks
`CRP_CFG`/`MAI_CTL` on every `recover_stream`, and the hardware test includes one unplug/replug.
The Pi 5 has no firmware audio service (no `vchiq` node in the DTB), so nobody else drives
the MAI and nothing conflicts with us.

## 2. USB Audio Class (UAC1, then UAC2)

Split into three lanes so each fits in 60 calls:
- **`usbisoch`** (xHCI, shared by all machines): `EP_TYPE_ISOCH_OUT = 1`/`IN = 5`, `TRB_ISOCH = 5`
  with SIA (start ASAP), TBC/TLBPC = 0 for FS/HS, interval from `bInterval` (FS: frames; HS:
  2^(b-1) microframes). An `IsochPipe` modelled on `intr.rs`: N = 64 one-millisecond TDs primed,
  harvested on the 100 Hz tick (try-lock), and refilled from a byte ring. On Ring
  Underrun/Overrun or Missed Service, it re-primes with SIA and counts the event. A FS device
  behind a HS hub must have the TT hub slot/port set in its slot context; check `hub.rs`.
  QEMU test: a hidden `usbisochtest` that streams a generated sine to `usb-audio` with raw
  SET_INTERFACE/SET_CUR. This is testable now because QEMU (both our binaries) has
  `qemu-xhci` + `usb-audio` (UAC1, FS, 48 kHz S16 stereo, isoch OUT EP1).
- **`uac1`**: a class driver that parses the AC header, terminals and Feature Unit and the AS
  interface/format type I (rates, subframe size, channels), then sets the alt setting and
  `SET_CUR` sampling frequency on the endpoint. Add `Backend::Usb(UsbAudio)` with the same seven
  calls. Packets per ms: fs/1000 frames, so 44.1 kHz alternates 44/45. Bind late: the device
  enumerates after `pipewire::init`, so on hotplug the server re-probes when it is `Backend::None`,
  and also when `snd.backend=usb`. On unplug it falls back to `None`, or to HDA. Keep the policy
  minimal: the most recently plugged wins, and `snd.backend=` pins one.
  `run-qemu.sh --usb-audio` adds `-device qemu-xhci -device usb-audio,audiodev=snd0`. Test:
  `LEANDROS_AUDIO_WAV=... sndtest all` then `sndwav.py --expect`, the same pass criteria as the HDA
  lane (440/660 Hz, L/R separation, 44.1 kHz segment).
- **`uac2`**: HS, a clock-source entity (`SET_CUR` CS_SAM_FREQ on the clock, not the endpoint),
  24/32-bit subslots, the explicit feedback IN endpoint (async DACs: 16.16 samples per microframe,
  adjusting how many frames go in each packet), and implicit feedback as a quirk. QEMU's
  usb-audio is UAC1 only. Instead, test the descriptor parsing and the feedback math on the host
  with `lsusb -v` dumps of two or three real devices, and test the hardware on the laptop.
Benefit beyond the Pi: the laptop and PC get USB headsets and DACs through the same backend.
On the Pi it is gated on `piusb`.

## 3. Bluetooth (CYW43455 BT half): scope

- **Transport:** `uarta: serial@7d50c000`, on the **BCM2712 itself, not RP1**. It is
  `brcm,bcm7271-uart`, which means a 16550A-compatible MMIO UART (`reg-shift 2`,
  `reg-io-width 4`, 96 MHz clock, `skip-init`, RTS/CTS with auto-flow). It is on
  **SPI 0x114 (GIC 308)**. Pins are GPIO 24-27 on `gio` (`BT_RTS/CTS/TXD/RXD`), and the firmware
  has already muxed them (`skip-init`), so verify the mux with the `bcm2712_id.rs` row reader.
  `max-speed` is 3,000,000.
- **Power:** `shutdown-gpios = <&gio 29 0>` (`BT_ON`). `gio` is `0x10_7d50_8500`, the bank that
  `brcmf/sdio_host.rs` already drives for WL_ON (gpio 28). Reuse that helper: drive BT_ON low,
  wait 10 ms, drive it high, wait about 100 ms.
- **Firmware: not in firmware-nonfree.** The pinned `c91cd280` tree has no `.hcd`. It lives in
  **RPi-Distro/bluez-firmware**, `debian/firmware/broadcom/BCM4345C0.hcd`; pin commit
  `cdf61dc691a49ff01a124752bd04194907f0f9cd` with a sha256, as `ports/brcmfmac/fetch.sh` does.
  Linux tries `brcm/BCM4345C0.raspberrypi,5-model-b.hcd` first, then `brcm/BCM4345C0.hcd`. The
  chip is subver 0x6119.
- **Init (btbcm/hci_bcm):** start at 115200 8N1 with RTS/CTS. Send HCI_Reset and Read Local
  Version. Then `0xfc2e` (download minidriver), wait 50 ms, send each `.hcd` record as an HCI
  command (opcode, length, data), `0xfc4e` (launch RAM), and wait 250 ms. Then `0xfc18` for
  3 Mbaud (switch our UART too), Reset, and Read Version again. If the BD_ADDR is the BCM4345C0
  placeholder (`43:45:c0:00:1f:ac`), `0xfc01` sets it. The address comes from the **live** DTB's
  `local-bd-address`, which the firmware fills (the decompiled file shows zeros), with a fallback
  of mailbox `GET_BOARD_MAC` + 1.
- **Host stack: we have none.** BlueZ cannot be ported on its own: it needs the Linux kernel's
  `AF_BLUETOOTH`/mgmt core. BTstack has a non-free licence. NimBLE is LE-only (no A2DP). Build
  our own minimal HCI core in the kernel or a server. Milestones: (a) `bt-hci-scan`: inquiry + LE
  scan, which lists devices; (b) L2CAP + SMP/SSP pairing + HID (classic) / HOGP (LE) for
  keyboards and mice, about 2-3 lanes; (c) AVDTP/A2DP source + SBC encoder as a fourth audio
  backend, about 3 lanes. Bluetooth audio is the **most expensive** sound path, so it goes last.
  The HCI core also serves USB BT dongles (btusb = HCI over USB bulk/interrupt) on the laptop and
  PC later, but the laptop's Intel BT needs its own firmware loader.
- No QEMU path. Tests are host-side: an H4 framer, a `.hcd` record parser run on the real file,
  and the patchram state machine against a scripted fake controller (a transcript in btmon order).

## 4. Display: direct HVS/KMS on the Pi 5

- **Firmware KMS probe:** do not send `SET_PLANE` (0x48015) blind, because it could change the
  screen. Probe the read-only fkms tags instead: `GET_NUM_DISPLAYS` 0x00040013 and
  `GET_DISPLAY_SETTINGS` 0x00040014. If they answer, try one `SET_PLANE` of the current
  framebuffer at the same position in `pihvs-ro`. The Pi 5 firmware is documented as full-KMS
  only, so expect a refusal. This goes in `pihdmi-probe` because it costs the same boot.
- **Gen-6 HVS, staged:** `pihvs-ro` reads `hvs@107c580000`: the display list pointers, the
  firmware's dlist in dlist RAM, and the frame/EOF status. It decodes and logs them, and resolves
  the hvs `interrupts = <2 9 16>` parent, which is `disp_intr` @0x7c502000. `pihvs-plane` writes a
  one-plane dlist (XRGB8888, unity, 40-bit BO PA, pitch), swaps the pointer at EOF, restores the
  firmware dlist on release, and turns HVS EOF into the vblank for PAGE_FLIP events. It keeps the
  firmware's timing and touches no PHY or clocks.
- **Full KMS (`pikms-*`, 4 or more lanes):** EDID over DDC (`ddc0/1` brcmstb-i2c at
  0x7d508200/0x280, or mailbox `GET_EDID_BLOCK` 0x00030020 first), HPD IRQ through `aon_intr`,
  pixel clock via a firmware clock `SET_RATE`, `vc6_hdmi_phy_init`, pixelvalve + HDMI timing, and
  then the scrambler for >340 MHz.
- **What it unlocks:**
  - HDMI audio does **not** require it (see section 1), but KMS makes audio robust: we know the
    mode, so there is no measured pixel clock, and we redo ACR on every mode change and hotplug.
  - HPD-driven hotplug, mode changes, the second HDMI head, and real vblank (no copy, no tearing).
  - Sink audio capabilities from the EDID SADs, i.e. which rates the TV accepts.

## 5. I2S on RP1 (HATs): backlog

`rp1_i2s0..2` at RP1 0xa0000/0xa4000/0xa8000 (DesignWare I2S) use RP1's `dw-axi-dmac`
(`dma@188000`). This needs PCIe + RP1 (`pipcie`) and a HAT with a codec whose driver (an I2C
control path) depends on the HAT. `rp1_audio_out@94000` is RP1's PWM audio. Its pins on the
Pi 5 B are only GPIO 12/13 on the header, with no jack. Build none of this until the user names a
HAT.

## Lane details (each must fit in 60 tool calls; all are aarch64 QEMU regress + `check-rpi5.sh`)

1. **`pihdmi-probe`**: `drivers/src/hdmi_probe.rs`, rpi5 only, read-only and inert:
   - Boot lines: `[hdmiaud] dvp sw_init=.. misc=..`; per port
     `[hdmiaud] hdmiN hpd=.. sched=0x.. hdmi_mode=0|1 mai_ctl=.. crp=.. ram_pkt=..
     h=../.. v=../.. fps=..  pixclk=.. kHz (cea ..)`; `[hdmiaud] mbox clk9=.. clk13=.. clk14=..
     edid=<n blocks|refused> fkms=<n displays|refused>`; `[dma40] ch6..10 cs=..` (all idle?).
   - Host tests: timing decode, fps-to-CEA snapping, the N/CTS table (HDMI 1.4 §7.2 values for
     32/44.1/48 kHz).
   - Next boot shows: `hdmi_mode=1` and `pixclk=148500` for a 1080p60 TV. If `hdmi_mode=0`, add
     `hdmi_drive=2`.
2. **`usbisoch`**: as in section 2. QEMU milestone: `usbisochtest` streams 2 s to `usb-audio` and
   the WAV has a 1 kHz tone with RMS above threshold and fewer than 1 % zero-runs. Counters:
   `isoch_underrun`, `isoch_missed` in kiostat.
3. **`pidma40`**: `drivers/src/bcm2712_dma40.rs`: channel claim (mask 0x7c0), SCB builder, cyclic
   rings, a position read and abort. Boot self-test on rpi5: a 64 KiB mem-to-mem copy on ch10 with
   a pattern check (`[dma40] m2m ch10 65536 B ok`). This also settles whether dma40 takes CPU
   physical 40-bit addresses (expected: yes, with no bus alias). Host tests for the SCB/cyclic math.
4. **`uac1`**: as in section 2; `snd.backend=usb`; `info` prints `backend: usb-audio <vid:pid> <rate>`.
   Hardware: on the laptop with any USB headset or dongle, `sndtest all` is audible and
   `isoch_underrun` stays 0 over 60 s.
5. **`pihdmi-audio`**: `drivers/src/hdmi_audio.rs` + `Backend::Hdmi` (cfg rpi5),
   `snd.backend=hdmi`, and auto on rpi5 once proven. IEC958 packer, MAI/ACR/InfoFrame, 128 KiB
   ring (IEC958 at 48 kHz stereo is 384 KB/s, so about 340 ms), DMA ch10 cyclic, and position
   from the dma40 source address. Counters: MAI ERRORE/ERRORF (FIFO under/overflow) in kiostat.
   Hardware: `[hdmiaud] stream 48000 2ch n=6144 cts=148500 dma ch10`, `sndtest all` audible, the
   error counters stay 0 over 60 s, and audio survives one HDMI unplug/replug (or the failure is
   logged and recovered).
6. **`uac2`**: as in section 2.
7. **`pibt-uart`**: a 16550 MMIO driver (the IRQ-driven RX ring is the lesson of #101: lost RX
   chars on uart10), BT_ON, an H4 framer, Reset + Read Local Version. `[bt] hci reset ok
   lmp_subver=0x6119`, or `[bt] no answer` (inert).
8. **`pibt-fw`**: `ports/bluez-firmware/fetch.sh` (pinned as above), shipped to `/lib/firmware/brcm/`,
   patchram, 3 Mbaud, BD_ADDR. `[bt] patched fw ... bdaddr=..`.
9. **`bt-hci-scan`**: HCI core + `btctl scan`. Backlog after this: `bt-l2cap`, `bt-hid`, `bt-a2dp-sbc`.
10. **`pihvs-ro`**, 11. **`pihvs-plane`**, 12+. **`pikms-*`**: as in section 4.

Pi boots are the scarce resource, so batch lanes 1, 3 and 7 into one boot: all three are
read-only or self-contained probes. Lane 5 then needs one or two more boots (`pi-remote` kexec,
once Wi-Fi works).
