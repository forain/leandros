# lane lapdsdt (2026-10-27): PARTIAL, laptop unreachable, NOT merged

Branch lane/lapdsdt (merged with main of 2026-10-27). Commits: raw ACPI tables at /sys/firmware/acpi/tables + power_supply/backlight class dirs in the root image (06b01cc8); EC hardening in drivers/src/acpi_power/ec.rs + power.rs.

Done:
- EC: one lock around all transactions, stale OBF drained before each command, 50 ms waits, 1 s back-off after 4 consecutive timeouts instead of the permanent disable after 32, first failed EC field read logged (no more silent 0), timeouts logged (first 16).
- Knobs: acpi_power.trace=1 (every EC rd/wr/query), acpi_power.ecdump=1 (256-byte EC dump at init).
- Interpreter: Sleep clamp 20 -> 100 ms (Stall/Sleep were already real delays, not no-ops). _REG(3,1) now also called on the PNP0C09 device itself.
- Builds clean INTEL=1 x86_64 and aarch64 (no ^error). vfstest green on both under QEMU.

Not done (laptop 172.16.149.179 unreachable for >30 min, ping 100% loss): trial deploy, DSDT/SSDT pull + iasl, BAT0/AC verification. DSDT findings: none yet.
Next: boot trial on laptop with acpi_power.trace=1 acpi_power.ecdump=1; cat /sys/firmware/acpi/tables/{DSDT,SSDT*} as root; iasl; classify the _BST/_PSR path per design section 3.
Not merged: EC behavior change is unverified on hardware.

## Hardware pass (2026-10-28): FIXED (EC hardening verified on the real EliteBook)
- Trial 305b6797 (acpi_power.trace=1 ecdump=1): BAT0 capacity 22 -> 24 %, status Not charging at up=17 s then Charging, current_now 1.1-1.5 A, AC online=1 over 4 samples in 140 s; BAT1 present=0. The EC dump and trace are consistent with sysfs.
- DSDT: BAT0 has _STA/_BIX/_BST (BTIX/BTST wrappers), AC _PSR -> EC0.GACS -> UPAD, which refreshes ACST from EC field ADP (offset 0x84 bit 0) only when the GACP bit 0 is set (event path), otherwise returns the cached ACST. EC reads are guarded by Acquire(BTMX/ECMX) and ECRG, hence the _REG(3,1) call mattered. ADP read 0x11 in the dump = AC present, BATP bits set.
- The first sample right after boot (AC 0 / Not charging / current 0) was an EC settling transient; it reads correctly from ~40 s on.
- Added guard (power_sysfs): a battery with now==0 && full==0 (failed or all-zero _BST/_BIF) is reported present=0/status Unknown instead of capacity 0, so no policy daemon can see a fake empty battery.
- Not promoted. vfstest green on x86_64 and aarch64 QEMU; INTEL=1 x86_64 release build clean.
