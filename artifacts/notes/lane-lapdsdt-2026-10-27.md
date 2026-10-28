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
