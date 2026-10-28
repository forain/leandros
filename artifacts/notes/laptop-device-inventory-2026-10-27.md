# HP EliteBook 840 G5 — device inventory and driver gap table (2026-10-27)

Source: the real laptop booted on LeandrOS trials `lapinv` / `lapinv2` (lane lapinv, built on main b28bb581),
boot logs `artifacts/remote-logs/20261027-223715-172.16.149.179/` and `.../20261027-224342-.../klog-current.txt`
(`grep LAPINV`). The kernel now logs this inventory at every x86_64 boot (`drivers/src/lapinv.rs`, knob `lapinv=0` off):
ACPI table list, every `_HID`/`_CID` in the DSDT + 30 SSDTs (byte-level AML walk; `_STA` is NOT evaluated, so
"declared" ≠ "fitted"), every PCI function with BARs and subsystem id, TPM / Thunderbolt / SMBus probes.
USB from `lsusb` on the same boot. Subsystem id of every PCH function: 103c:83b2.

ACPI tables: FACP DSDT SSDT×30 RTMA UEFI OEML TPM2 MSDM SLIC WSMT HPET APIC MCFG DBGP DBG2 DMAR NHLT ASF! FPDT BGRT.

## PCI (25 functions)

| BDF | id | what | driver status | owner |
|---|---|---|---|---|
| 00:00.0 | 8086:5914 | KBL-R host bridge | n/a | — |
| 00:02.0 | 8086:5917 | UHD 620 (bar0 0x1ff2000000, bar2 0xc0000000) | works: KMS + iris/ANV; internal panel only | lapdisp (ext. outputs) |
| 00:04.0 | 8086:1903 | processor thermal (DPTF participant) | none (thermal via MSRs works) | unowned (low) |
| 00:14.0 | 8086:9d2f | PCH xHCI | works | — |
| 00:14.2 | 8086:9d31 | PCH thermal subsystem | none | unowned (low) |
| 00:15.0 / 15.1 | 8086:9d60 / 9d61 | Serial IO I2C0 / I2C1 | **none** — hosts the I2C-HID touchpad, USB-PD controllers, NFC | unowned (see I2C rows) |
| 00:16.0 | 8086:9d3a | ME HECI | none (not needed) | — |
| 00:16.3 | 8086:9d3d | AMT KT/SOL serial (io 0x3060) | none | unowned (low) |
| 00:1c.0/1c.4/1d.0 | 8086:9d13/9d14/9d18 | PCIe root ports | works | — |
| 00:1f.0 | 8086:9d4e | LPC/eSPI | works (EC, PS/2) | — |
| 00:1f.2 | 8086:9d21 | PMC | none (S0ix/S3 control) | lapsusp |
| 00:1f.3 | 8086:9d71 | HDA (Conexant + i915 HDMI codec), NHLT present | partial: speakers | lapaudio2 |
| 00:1f.4 | 8086:9d23 | SMBus (io 0xefa0; firmware leaves I/O decode OFF) | probe only (lapinv): no ST accelerometer at 0x18/19/1c/1d/29 | — |
| 00:1f.6 | 8086:15d7 | I219-LM | works | — |
| 01:00.0 | 8086:24fd | Wireless 8265 | partial: WPA2, legacy 54 Mb/s | lapwifiht |
| 02:00.0, 03:00-02.0 | 8086:15da | Alpine Ridge 2C (JHL6340) TB3 bridges | works (enumerated) | — |
| 04:00.0 | 8086:15d9 | Alpine Ridge 2C NHI | report only (lapinv): D0, FW_STS 0x800001a1 (ICM_EN, NVM auth done), OUTMAIL 0x800003f0 = **mode 3, firmware connection manager** | unowned (TB tunnelling) |
| 3a:00.0 | 8086:15db | Alpine Ridge USB 3.1 xHCI (USB-C ports) | partial: early handoff/quiesce only, no root hub in `lsusb` | unowned → suggest lane `lapusbc` |
| 3b:00.0 | 144d:a808 | Samsung PM981 NVMe | works | — |

## USB (bus 1 = PCH xHCI)

| port | vid:pid | device | status | owner |
|---|---|---|---|---|
| 7 | 8087:0a2b | Intel 8265 Bluetooth | none | lapbt |
| 8 | 138a:00ab | Synaptics/Validity fingerprint (vendor class) | none — feasibility note below | unowned (not feasible) |
| 9 | 0408:5371 | Quanta HP HD webcam (composite) | none | lapuvc |
| 13 | 0781:55ab | SanDisk boot stick | works | — |

**No USB CCID smartcard reader is present** (not fitted on this SKU or disabled in BIOS setup); nothing to drive.

## ACPI devices of interest (`_HID`/`_CID`; `_STA` not evaluated)

| path | id | what | status | owner |
|---|---|---|---|---|
| AC, BAT0, BAT1 | ACPI0003, PNP0C0A×2 | AC adapter, batteries | none | lappower |
| LID, PWRB, SLPB | PNP0C0D/0C0C/0C0E | lid, power, sleep buttons | none | lappower |
| PCI0.LPCB.EC0 (+SEN1/SEN2 INT3403) | PNP0C09 | embedded controller, DPTF sensors | none (needs AML) | lappower |
| WMIB, WMIV, WTBT | PNP0C14 | HP WMI (hotkeys, BIOS), TB WMI | none | lappower (hotkeys) |
| WLBU | HPQ6001 | HP wireless (rfkill) button | none | lappower |
| PS2K | HPQ8002 / PNP0303 | keyboard | works | — |
| PCI0.I2C0.TPD0 | runtime `_HID` / PNP0C50 | I2C-HID touchpad (we use the PS/2 path) | works via PS/2; I2C-HID none | unowned (low) |
| PCI0.I2C0.TPL0 | ELAN25D0 / PNP0C50 | Elan touchscreen option (non-touch SKU likely `_STA`=0) | none | unowned (not fitted?) |
| PCI0.I2C1.TPL1, CPD0 | PNP0C50 | further I2C-HID options | none | unowned |
| PCI0.I2C0.UCM1/UCM2 | INT3515 | TI TPS6598x USB-PD controllers | none (PD firmware autonomous) | lapdisp (alt-mode status) |
| UBTC | USBC000 / PNP0CA0 | UCSI | none | lapdisp |
| PCI0.I2C1.NFC1 | NXP1002 | NXP NFC option | none | unowned (optional) |
| PCI0.I2C1.IMP3 | IMPJ0003 | unknown HP I2C device | none | unowned |
| TPM_ | IFX0763 / MSFT0101 | **TPM 2.0**, Infineon, TIS at 0xfed40000 | probe only (lapinv): did_vid 0x001b15d1, access 0xa1 (locality 0 active), TPM2 table start method 6 (TIS) | — |
| FDAD | HPIC000C | HP fall-detect device; no HPQ6000/HPQ6007 lis3lv02d node, nothing on the PCH SMBus → EC-hosted, AML-only | none | lappower (needs AML/EC) |
| BTKL | INT3420 | Bluetooth power/kill | none | lapbt |
| IETM, PTID, SKC0, DGFC, DGHM, MCPP | INT3400/340E/3470/340D/3530 | DPTF framework, PAC power meters | none | unowned (low) |
| PCI0.LPCB.CWDT | INT3F0D | watchdog | none | unowned (low) |
| SPI1.FPNT, SPI0/1.SPL0/1, GNSS, UA00.GPS0, CIO2, CHUB, SHAD, EIAD | (various) | SPI fingerprint/HID, WWAN GNSS, IPU, sensor hub options — not present as hardware on this unit (no ISH PCI function, no WWAN) | n/a | — |

**No ambient light sensor** (no ACPI0008, no ISH) and **no ST accelerometer** on this laptop.

## Fingerprint reader 138a:00ab — feasibility

Synaptics (ex-Validity) match-in-sensor device with a TLS-wrapped protocol whose pairing keys are provisioned
per laptop (HP flashes them at the factory; Windows pairs through the vendor driver). libfprint has no driver
for 00ab; python-validity covers only 138a:0090/0097/009d and 06cb:009a, and needs a vendor firmware blob plus
re-pairing that wipes Windows enrollments. Porting would be: USB bulk + TLS 1.2 (ECDH P-256, AES-CBC/HMAC) +
reverse-engineered command set — weeks, with no reference for this pid. **Not feasible; leave unowned.**

## Unowned work worth a lane

1. `lapusbc`: drive the Alpine Ridge xHCI 3a:00.0 (USB on the USB-C ports; ICM already tunnels in mode 3).
2. I2C DesignWare host (9d60/9d61) + I2C-HID: unlocks touchpad precision mode, UCSI/PD status, NFC.
3. DPTF/thermal participants (INT3400 family) and the INT3F0D watchdog: low priority.
