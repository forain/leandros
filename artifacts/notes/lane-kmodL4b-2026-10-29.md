# lane kmodL4b (2026-10-29): kmodcfg - tristate MODULES, to-cargo, alias autoload, kmodctl, gate 6

DELIVERED.

- `Kconfig`: `config MODULES` is now `bool "Loadable module support"` + `option modules`, default n (kconfiglib 14.1.0 parses it without warning).
  `drivers/Kconfig`: `KMOD_HELLO` tristate, no `depends on MODULES` (so MODULES=n offers y/n, MODULES=y offers y/m/n). =y is ignored (no built-in form).
- `scripts/kconfig/to-cargo.py`: `=m` needs `modules/<dir>/Cargo.toml`, else hard error naming the symbol (`KMOD_HELLO` -> `hello` via `KMOD_NAME_MAP`). kmod-list is sorted.
- `kernel/src/kmod/autoload.rs`: after `modules.load`, reads `lib/modules/<id>/modules.alias` (`pci`/`usb` lines, parsed into `KmodDevId`), and for each `drivers::pci::scan()` function loads the first matching name once through `load_by_name` (blacklist, kmod=0 apply; `Exists` is silent). USB lines are parsed and ignored. `modules.load` is no longer required for the alias path.
- `userland/kmodctl/kmodctl.c`: `list`, `load <path>`, `unload <name>`, `info <path>`; errors print strerror + errno. Plain C built with zig like kexec/kexectest (plan said Rust; C avoids a new userland crate).
- `userland/kmodctltest/kmodctltest.c` (closes L3d gate 6): SKIP without /lib/modules or a loaded kmodtest. As root: second load of the running kernel's kmodtest -> EEXIST (exactly one of the two build-id dirs), unload -> EBUSY, kmodbad -> ENOEXEC (8); fork + setgid/setuid(1000): load and unload -> EPERM. Added to the `regress` list and a `kmodctl` suite in runtests.py.
- `mkfs-f2fs-populated.py`: stages `target/kmod/<arch>/rootfs/lib/modules/**` (else the `initrd/` copy, i.e. the KMOD_TEST blobs) into `/lib/modules`; kmodctl and kmodctltest go to /bin.

## Evidence (aarch64, KMOD_TEST=1)
- features output md5 identical to main for x86_64, x86_64_laptop, aarch64, rpi5 defconfigs.
- `CONFIG_KMOD_HELLO=m` without modules/hello: `to-cargo.py: CONFIG_KMOD_HELLO=m but there is no module crate modules/hello/Cargo.toml`.
- Alias (temp edit of testblob: alias file instead of modules.load): `kmodtest loaded id 1` by alias; `modules.blacklist=kmodtest` -> `Blacklisted`, boot OK; `kmod=0` -> `disabled by kmod=0`.
- kmodctltest ALL PASS (EEXIST, EBUSY, ENOEXEC, EPERM).

## Notes
- The other build-id dir's kmodbad carries THIS kernel's build id but is linked against the other variant's kapi addresses: loading it RUNS it and hung the CPU (watchdog). kmodctltest therefore loads only the kmodbad of its own dir. Do not load both dirs' kmodbad.
- testblob.sh still writes `modules.load` (unchanged; it is L4a's file). The alias path was tested with a local, uncommitted edit.
- Not done: menuconfig TUI was not driven interactively; assignability checked through kconfiglib (MODULES=n -> (0,2), MODULES=y -> (0,1,2)) and write_min_config round-trips `KMOD_HELLO=m`.
