# lane/lappower 2026-10-27: laptop ACPI power, backlight, lid/buttons, hotkeys

Status: **PARTIAL**. Backlight and battery identity work on the real laptop. Live charge level and AC state read as 0.

## What landed
- `drivers/src/acpi_power/aml.rs` is a new AML interpreter in pure core+alloc, with a `Host` trait for hardware access.
  - It loads the DSDT and SSDTs into a namespace.
  - It evaluates methods: Field, IndexField and BankField over SystemMemory, SystemIO, PCI_Config and EmbeddedControl; buffer fields; packages; Index/DerefOf; If and While; `_OSI` (Windows strings); Notify.
  - A While loop is capped at 100k iterations or 2 s.
  - Host-tested against a laptop-like ASL compiled with iasl: `$SP/amltest` (`cargo run --release -- t.aml`). Every value matched.
- `ec.rs` is a polled embedded-controller driver. It finds the EC from the ECDT or from `_CRS`. After 32 timeouts it disables itself.
- `power.rs` handles device discovery and polling.
  - Discovery covers PNP0C0A, ACPI0003, PNP0C0D, PNP0C0C, PNP0C0E, PNP0C09 and `_BCL`/`_BCM`.
  - At init it runs `\_SB._INI`, the EC's `_REG(3,1)` and `\_SB.PCI0._INI`. It also sets SCI_EN through SMI_CMD when the firmware left it clear.
  - The 200 ms poll in thread `kacpid` handles PM1 fixed buttons, EC `_Qxx` queries and EC GPE clearing. Other GPE methods run only with `acpi_power.gpe=1`. Every second it also polls `_LID` and `_PSR`.
  - Notify handling: 0x86/0x87 become KEY_BRIGHTNESSUP/DOWN; lid changes become SW_LID; battery and AC Notifies bump `change_seq`.
- Plumbing:
  - The `/sys/class/{power_supply,backlight}` overlay is `servers/vfs/src/sysclass.rs` plus `drivers/src/power_sysfs.rs`. `brightness` is writable as root.
  - `drivers/src/i915/backlight.rs` drives the PCH BLC PWM.
  - evdev has a switch device, "ACPI Power Buttons".
  - PS/2 E0 20/2E/30 map to mute and volume down/up.
  - upower now reads power_supply from sysfs.
- Knobs: `acpi_power=0` turns all of this off; `acpi_power.fake=1` serves fake QEMU data.

## Evidence
- QEMU q35 x86_64: namespace of 282 nodes, 0 load errors, 12 ms; vfstest green. aarch64 vfstest green, where the module is inert.
- Laptop trial a6e0ad3f3374 (branch at 15eeb634, which includes main b28bb581), then booted back to good:
  - Namespace of **5494 nodes, 0 load errors, 0 init errors, 56 ms**. Found the EC (GPE 0x6E), BAT0, BAT1, AC, LID_ (open), SLPB, and the GFX0.DD1F video output with 100 levels. The power button is a fixed-feature button.
  - `ls /sys/class/power_supply` gives `AC BAT0 BAT1`; `ls /sys/class/backlight` gives `intel_backlight`.
  - BAT0 from `_BIX` reads correctly: charge_full 3942000, voltage_min_design 11550000, cycle_count 464, Li-ion, model "Primary", Hewlett-Packard, serial "36821 2020/05/14".
  - **Backlight:** max 1500. Writing 375 gave actual_brightness 375; restoring 1500 gave 1500.
  - **WRONG:** `_BST` returns remaining 0 and rate 0, so capacity is 0% and status Discharging. Voltage is 9.14 V. `_PSR` returns 0 on both reads, 3 s apart.
  - The firmware booted with **SCI_EN clear**; init switched it to ACPI mode.

## Open (next lane)
1. `_BST` and `_PSR` read 0. We need the real DSDT: expose `/sys/firmware/acpi/tables/DSDT` (or dump it), run `iasl -d`, then run the `$SP/amltest` harness against it. Likely causes: the HP BTST/SMI path through SystemIO 0xB2 with results in a GNVS SystemMemory region; data refreshed only by an EC `_Qxx` event; or an Acquire/Wait semantic.
2. The ACPI-mode switch (SCI_EN) is new behaviour on every x86 machine that boots in legacy mode. Watch the PC.
3. Lid, button and hotkey events were not proven on hardware. upower was not run on the laptop, because deploy-kernel ships no userland.
4. `ls /sys/class` does not list the two new classes.
