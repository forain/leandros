# lane lapinv — 2026-10-27

DELIVERED (inventory) + probes. Inventory and gap table: `artifacts/notes/laptop-device-inventory-2026-10-27.md`.

## What was built
- `drivers/src/lapinv.rs` (x86_64 only, runs at boot before the NVIDIA probe; `lapinv=0` disables, `lapinv.smbus=0`
  skips SMBus, `lapinv.acpi=1` also prints the AML device lines on the UART):
  - ACPI: list of tables; byte-level AML walk of DSDT + SSDTs (Device/Scope PkgLength + NameString, `_HID`/`_CID`
    as EISAID or string) → `[LAPINV] acpi <table> <oem> <path> _HID=<id>` in the kernel log (84 `_HID` + 17 `_CID` on the laptop).
  - PCI: every function with BARs and subsystem id (kernel log).
  - TPM: TPM2 table start method + locality-0 registers, read-only.
  - Thunderbolt 3 NHI: PM state, FW_STS, ICM operating mode (REG_OUTMAIL_CMD 0x3990c), read-only.
  - Intel SMBus host: enables I/O decode if firmware left it off (as Linux i801 does), takes the INUSE semaphore,
    reads WHO_AM_I (0x0F) at the ST accelerometer addresses; a recognised chip is powered and one sample logged.
- `boot/src/acpi.rs`: `for_each_table()` (XSDT/RSDT + the FADT's DSDT).

## Evidence (real laptop, trials lapinv / lapinv2, both booted to sshd, good entry restored after each)
- `tpm: vendor 0x15d1 (Infineon) device 0x001b`, access 0xa1, TPM2 start method 6 (TIS); ACPI `IFX0763`/`MSFT0101` → TPM 2.0.
- `thunderbolt NHI 04:00.0 15d9 ... D0 FW_STS 0x800001a1 (ICM_EN=1 nvm_auth_done=1) OUTMAIL 0x800003f0 mode 3 (connection manager)`.
- `smbus 9d23: enabled I/O decode (cmd 0x0 -> 0x1)`; addrs 0x1d/0x1c/0x19/0x18/0x29 no ack.
- QEMU x86_64 (q35): runs, TPM absent, ICH9 SMBus left alone (HOSTC disabled); timertest green. aarch64: module compiled out.

## Requested leftovers — outcome
1. HP accelerometer: **not present** — no HPQ6000/HPQ6007 node, nothing on the PCH SMBus; the only candidate is
   `FDAD` (HPIC000C), EC-hosted and AML-only → needs lappower's EC/AML work. Probe stays for other HP models.
2. Ambient light sensor: **not present** (no ACPI0008, no ISH).
3. Smartcard reader: **not present** on USB (no CCID device enumerated).
4. TPM 2.0: probe done (above). No command transport yet.
5. Fingerprint 138a:00ab: feasibility note in the inventory — not feasible (TLS + per-device pairing, no reference driver).
6. Thunderbolt 3: report done (ICM firmware CM, mode 3, NVM authenticated).

## Open / suggested lanes
- `lapusbc`: Alpine Ridge xHCI 3a:00.0 (USB on the USB-C ports) is only handed off/quiesced, no bus in `lsusb`.
- I2C DesignWare (9d60/9d61) + I2C-HID: touchpad precision mode, UCSI/PD status, NFC.
- AML interpreter is the gate for battery, EC hotkeys, HP wireless button, fall-detect (lappower).
