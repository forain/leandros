# lane/kexecacpi 2026-11-13: ACPI tables in Reserved RAM above 2 GiB after kexec

FIXED (x86_64 direct boot). `direct_hhdm::map_acpi` maps only the ACPI table pages (walker `boot/src/acpi_pages.rs`).
Guard `kexecacpitest.py` (own boot, 2800M): green maps 11 ACPI pages, 4 CPUs; red (`KEXECACPI_NOFIX=1`) see run log.
Test-design lesson: the retype alone did nothing, because OVMF keeps tables mid-RAM and `extend` 2M pages cover them;
the knob must also make usable RAM above the lowest table Reserved (info::build), as real firmware has tables at the top.
Open: none known; aarch64 unaffected (DT path).
