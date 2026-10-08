# Lane poweroff (2026-10-02): FIXED

Branch `lane/poweroff` @ `67b66f2` (base origin/main `dce4140`, the vtarb merge), pushed. No COSMIC source was patched.

COSMIC's Shut Down / Restart (cosmic-osd -> login1.PowerOff/Reboot) now powers off or reboots the VM cleanly on both arches, the Linux way: login1 asks init, init does the orderly shutdown, and the kernel's reboot(2) ends with ACPI S5 / RESET_REG (x86_64) or PSCI SYSTEM_OFF / SYSTEM_RESET (aarch64).

## What changed
- **Kernel reboot(2)** (`kernel/src/power.rs`, nr 169 on x86_64 / 142 on aarch64): Linux magics (MAGIC1 plus the 4 MAGIC2 values), CAP_SYS_BOOT = euid 0 checked first (EPERM), then RESTART/RESTART2, HALT, POWER_OFF, CAD_ON/CAD_OFF (state only, since nothing generates Ctrl-Alt-Del yet), and EINVAL for KEXEC/SW_SUSPEND/unknown. The order follows `kernel_power_off`: system_state, then notifiers (none), then device shutdown, then machine. Device shutdown runs `sync` and then `f2fs_server::shutdown_all()`: every volume still writable gets a clean-unmount checkpoint, a virtio-blk FLUSH, and goes read-only. Then the console drains, IRQs are masked and the stall watchdog is told to stand down (`sched::mark_system_down`).
- **x86_64** (`arch/x86_64/src/power.rs`, `boot/src/acpi.rs`): the FADT is parsed at boot (PM1a/PM1b_CNT, X_ blocks, SMI_CMD/ACPI_ENABLE, RESET_REG when RESET_REG_SUP is set, ARM_BOOT_ARCH), along with `\_S5_` from DSDT AML. QEMU q35 gives `PM1a_CNT=0x604, \_S5_ 0/0, RESET_REG=0xCF9 value 0x0F`. Power-off writes SLP_TYP and then SLP_TYP|SLP_EN, and enables ACPI first if SCI_EN is clear. Reset tries RESET_REG, then the KBC 0xFE, then 0xCF9, then a triple fault.
- **aarch64** (`arch/aarch64/src/power.rs`, `smp.rs`): a generic `psci_call` on the HVC/SMC conduit (the same choice CPU_ON makes), plus SYSTEM_OFF 0x84000008 and SYSTEM_RESET 0x84000009. raspi4b has no PSCI, so it halts.
- **f2fs**: `CP_UMOUNT_FLAG` (0x1, at ckpt_flags offset 132) is written on clean-unmount checkpoints and cleared on ordinary ones. Mount now logs `[F2FS] /dev/vdX: checkpoint ver N is a clean unmount` or `... is NOT a clean unmount: the previous shutdown was unclean`. A read-only mode answers EROFS for every mutating op, skips atime and writes no checkpoint. `remount_by_port`, and umount writes a clean checkpoint. mkfs used 0x8 for this flag, which is CP_ERROR_FLAG in Linux, so it was corrected to 0x1.
- **mount(2) MS_REMOUNT[|MS_RDONLY]**: root only. It is decided before source and fstype are read (both may be NULL).
- **kill(-1, sig)** (`sched::kill_all`): every process the caller may signal, except `init_pid()` and the caller. It used to return EPERM unconditionally.
- **init** (`userland/init`): listens on `/run/user/initctl`. Sockets can only be bound on tmpfs here, and `/run/user` is the root-only tmpfs, so the name cannot be squatted; the socket itself is mode 0666. Protocol: one line `poweroff|reboot|halt`, answered `ok|denied|invalid`. A request is authorised from SO_PEERCRED: uid 0, or a peer whose ancestry or session reaches a login init supervises (greetd chain, serial getty, tty2..6). That is logind's default `allow_active` policy. Shutdown sequence: SIGTERM+SIGHUP+SIGCONT to -1, a grace period of up to 5 s, SIGKILL to -1 with up to 3 s, sync, umount the other f2fs volumes, remount `/` read-only, reboot(2). init also sets CAD_OFF at boot.
- **`/bin/poweroff`, `/bin/reboot`, `/bin/halt`** (`userland/poweroff`): one binary dispatching on argv[0], hardlinked by mkfs. By default it asks init. `-f` runs sync + reboot(2) directly (root only). If init is unreachable, it falls back to `-f`.
- **login1** (`ports/sysbus`): PowerOff/Reboot/Halt emit PrepareForShutdown(true) and forward to init, which answers AccessDenied or Failed on refusal. CanPowerOff/CanReboot/CanHalt return "yes" while init listens. Suspend and the other sleep states stay NotSupported. The probe now checks Can* and Suspend instead of calling PowerOff.
- **QEMU launchers**: the UEFI paths of `run-qemu.sh` and `driver.py` no longer pass `-no-reboot`, so a guest reboot restarts through the firmware and the driver's sockets survive it. `LEANDROS_NO_REBOOT=1` restores the old behaviour.

## Evidence
Harness: scratchpad `po/pw.py`. It keeps one persistent serial connection and records the shutdown and the reboot. Each run writes a marker to `/root` and `/home/leandro` immediately before the shutdown, with no sync. Both columns are final builds.

| | x86_64 KVM virgl (desktop) | aarch64 HVF virgl (Mac) |
|---|---|---|
| COSMIC power key -> Confirm | init: `power-off requested by pid 269 (uid 1000): authorised`; all processes gone in 58 ms; **QEMU exits by itself 0.5 s after the click** | pid 223 (uid 1000) authorised; 54 ms; QEMU exits after 0.5 s |
| next boot | `checkpoint ver 814 is a clean unmount`, both markers present | ver 812 clean, markers present |
| COSMIC power key -> Restart | `reboot: Restarting system`, same QEMU back at greeter (screenshot), ver 879 clean, markers present | same, ver 882 clean, markers present, greeter screenshot |
| root `reboot` / `poweroff` (serial) | reboot -> back at login, clean, markers; poweroff -> QEMU exit 0.5 s, clean, markers | identical |
| leandro `poweroff -f` / `reboot -f` | `reboot(2): Operation not permitted`, rc=1 | same |
| root `halt` | `reboot: System halted`, VM stays up, WDOG 0 (was 1 before `mark_system_down`), next boot clean | not run |
| leandros-sysbus probe | 14/14 (CanPowerOff/CanReboot yes/yes) | 14/14 |
| vfstest / f2fstest / polltest | 59/0, 6/0, 18/0 (PASS/FAIL), rc 0 | 59/0, 6/0, 18/0 |
| exc/PF/SEGV/panic/WDOG in the runs | 0 | 0 |

Unclean detection works: a boot after the driver's `stop` (QEMU `quit`) logs `checkpoint ver 29 is NOT a clean unmount`.

## Notes / open
- Running all three tests on the Mac at load 150-230 (other lanes' builds) made the guest so slow that a greeter login never reached the desktop. That was an invalid run, and it was redone at low load.
- Shutdown takes about 50-90 ms here because COSMIC exits on the first SIGTERM/SIGHUP. Interactive shells ignore SIGTERM, which is why SIGHUP is sent too.
- Not done: logind's "block" inhibitors for `shutdown` are not enforced (cosmic only takes handle-power-key ones); the Ctrl-Alt-Del key -> SIGINT -> init path; the greeter's own power buttons (cosmic-greeter has no login1 in its phase); RESTART2's argument string is ignored.
- `kill(2)` still has no permission check for single-pid targets (pre-existing; kill_all has one). `mount(2)` without MS_REMOUNT is still unprivileged (pre-existing).
- Shared files touched: `kernel/src/syscall.rs`, `kernel/src/main.rs`, `sched/src/lib.rs`, `servers/f2fs/src/lib.rs`, `userland/init/src/main.rs` (on top of vtarb's VT logins; those are stopped by the same kill(-1)), `userland/libc/src/syscall.rs`, `ports/sysbus/*`, `scripts/mkfs-f2fs-populated.py`, `scripts/run-qemu.sh`, `.claude/skills/run-leandros/driver.py`.
