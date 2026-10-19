# lane/kexectool (2026-10-19)

Lane 4 of `artifacts/notes/kexec/PLAN.md`: `userland/kexec/kexec.c`, a
kexec-tools style front end over `kexec_file_load` + `reboot(KEXEC)`.
Installed as `/bin/kexec` (all tools in this tree live flat in /bin).

    kexec -l KERNEL [--initrd=F] [--append=S|--command-line=S] [--reuse-cmdline]
    kexec -u            unload
    kexec -e            sync, then reboot(KEXEC); "no kernel loaded" on EINVAL
    kexec KERNEL ...    load + exec

`--reuse-cmdline` seeds the command line from /proc/cmdline; `--append` adds
to it (no de-duplication: chained `kexec.gen=1 kexec.gen=2`). Errors name
the failing step and use strerror. Without `--initrd` the load passes
KEXEC_FILE_NO_INITRAMFS.

Wired: `scripts/build-userland.sh` (zig cc), `scripts/mkfs-f2fs-populated.py`
bins list, `runtests.py` regress suite (non-destructive
`kexec -l /boot/kexec --initrd=... && kexec -u`).

## Test (TCG, 4 CPUs)
- aarch64 (uefi-tcg): -u, -e with nothing loaded (error), missing kernel
  (error) ok; `-l ... --append=kexec.gen=1` + `-e`: `[early] cmdline:
  kexec.gen=1`, 4 CPUs online, login. Chain with `--reuse-cmdline
  --append=kexec.gen=2`: cmdline `kexec.gen=1 kexec.gen=2`, 4 CPUs, login.
- x86_64: same sequence, gen 1 and gen 2 identical results, 4 CPUs (xAPIC), login.
- regress entry command run by hand on aarch64: RC=0.
