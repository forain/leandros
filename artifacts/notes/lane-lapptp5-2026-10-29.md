# lane/lapptp5 - touchpad defaults to I2C when ACPI has the pad

Real HP EliteBook 840 G5: SYNA3071 (CPD0, PNP0C50, I2C 0x2c). A pad latched in I2C mode still answers PS/2
identification (AUX2: Synaptics absolute) but sends no motion, so the old `!ps2_syn` auto-switch never fired.

New default (drivers/src/i2c_hid.rs init): no `touchpad=` given (or `auto`) -> passive ACPI pre-scan
(scan(false): _INI/_STA/_HID/_CID/_CRS/_DSM only, no _PS0, no pad reads). A live (sta&1) slave with a HID descriptor
reg and hid SYNA* or cid PNP0C50 -> i2c, log `touchpad: CPD0 SYNA3071 present -> i2c (default)`.
No such pad / no LPSS controller -> ps2, log says why. `touchpad=ps2`, `ps2!` keep PS/2; `touchpad=i2c` forces.
PS/2 AUX: unchanged path of touchpad=i2c (PS/2 side still enumerates, silent; I2C node is the live source).
Why ps2 was default (lapptp3): default mode ran a 120 s plain I2C-HID listen (idle wakes). Mitigation kept: the listen
only runs with i2c_hid.listen=1/probe+reset; the I2C touchpad path is the same as the proven touchpad=i2c one.
