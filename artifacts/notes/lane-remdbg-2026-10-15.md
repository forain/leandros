# lane/remdbg: remote debugging on the real laptop over SSH (2026-10-15)

Goal: iterate on the HP EliteBook 840 G5 (booted from the USB stick) without
re-flashing the stick every round trip. You can push a kernel, files and knobs,
reboot, and collect logs over SSH, and a bad kernel cannot brick the stick.

The lane is based on main 8e2c1dfb, which already contains lane/gitssh (sshd)
and lane/homevol (the new stick layout). It is five commits.

## What was added

- **`userland/espctl`** (`/usr/sbin/espctl`, std/musl like sshd)
  - **Why it exists.** The VFS has no FAT driver, so the ESP is only reachable as
    a raw partition node (`/dev/sda1`). espctl reads and writes it through the
    `fatfs` crate.
    - The crate is vendored at 0.3.6 with two `create_dir` fixes. Upstream wrote
      an LFN entry before `.`/`..`, and a wrong `..` cluster for directories in
      the root; fsck.fat flagged both.
    - It sits behind a write-back 4 KiB block cache that flushes in block order,
      with a sync after each step.
  - **Kernel slots in limine.conf**:
    - `LeandrOS trial <id>`: one-shot.
    - `LeandrOS good <id>`: promoted.
    - `LeandrOS factory`: the flashed `/kernel.elf`, never modified.
  - **Where slots live.** Slot files go in `/leandros/k/<id>/{kernel.elf,initrd.gz}`,
    where `id` is a hash of the kernel and the initrd. They are never overwritten.
  - **Kernel command line.** Each entry's cmdline gets
    `leandros.slot=<slot> leandros.kid=<id>`, plus optional per-slot extra
    arguments (knob A/B tests without a new kernel).
  - **Commands:**
    - `status`
    - `install` (with read-back sha256 verification from the medium)
    - `arm` / `disarm` / `promote` / `discard` / `default` / `timeout` / `cmdline` / `reset`
    - `boot` (the init hook)
    - `import-keys`
    - `parts`
    - `klog [--all|--list|--boot N]`: the console log partition read on-device.
      It is the dmesg equivalent, and the twin of `read-usb-log.sh`.
    - `ls` / `cat` / `get` / `put` / `rm`
- **init** runs `espctl boot` after the fstab mounts and before sshd. It waits up
  to 60 s, and `espctl=0` on the cmdline skips it. The hook does three things:
  - If the armed trial kernel was booted, it makes the fallback the default
    again (one-shot).
  - If the trial was armed but not booted (picked away in the menu), it
    disarms it.
  - It merges ESP `/leandros/authorized_keys` into `/root/.ssh/authorized_keys`.
- **Kernel build tag.**
  - build-all.sh sets `LEANDROS_KERNEL_TAG` to `<rev>[-dirty]-<UTC time>`.
    Override it to name a test kernel.
  - The tag is printed in `[LEANDROS] Kernel starting... build <tag>` and is
    `uname -v` = `#1 <tag>`.
- **write-usb.sh `--authorized-keys F`.**
  - Also `LEANDROS_USB_AUTHORIZED_KEYS`. The default is
    `~/.ssh/leandros_remote.pub` when it exists; `--no-keys` puts none on the stick.
  - The keys go to ESP `/leandros/authorized_keys`.
  - `/limine` and `/leandros/k` are created at flash time.
- **`scripts/remote.py`** (host side, OpenSSH client):
  - Basics: `status`, `run`, `shell`.
  - Transfers:
    - `push` / `pull`: sha256-verified. A push goes to a temp name and is then
      renamed, and identical files are skipped.
    - `deploy-files`: `LOCAL:REMOTE[:MODE]` pairs or `--manifest`.
  - `logs` writes to `artifacts/remote-logs/<time>-<host>/` (gitignored):
    - `klog-current.txt`, and `klog-all.txt` covering the last 4 boots;
    - `status.txt`, `partitions.txt`, `limine.conf`;
    - `var-log/`, plus `/tmp/*.log`.
  - `reboot`: syncs, waits until the `/tmp` marker is gone and sshd answers,
    reports the boot time, and fetches logs. If the machine does not come back,
    it prints the exact fallback steps.
  - `deploy-kernel [--cmdline ..] [--reboot [--promote]]`
  - `promote`, `discard`, `espctl <args>`
  - `discover`: scans the Mac's /24 for the russh banner.
  - `keygen`, `setup`, `wait`, `forget-host`
  - **Target.** Set with `LEANDROS_REMOTE=host[:port]`, or `qemu[:run-id]` (that
    reads the driver's hostfwd file; under vmnet it reads the DHCP address from
    the serial log).
- **Agent skills:**
  - `.claude/skills/laptop-remote-debug`: reach the machine, files, logs,
    reboot, recovery.
  - `.claude/skills/laptop-deploy-kernel`: the round trip and the entry model.

## Safety model

- **What is touched.**
  - The flashed kernel, initrd and `/EFI` are never touched.
  - The flashed config is saved once as `/leandros/factory.conf`.
  - `espctl reset` restores it.
- **Order of an install.**
  1. The slot files are written and synced.
  2. The cache is dropped and the files are re-read from the medium and compared
     with the sha256 that the Mac computed.
  3. Only then does limine.conf change.
- **How limine.conf changes.**
  1. Write `/limine/limine.tmp`.
  2. Rename it to `/limine/limine.conf`. This is the shadow copy, which Limine's
     search order prefers to `/limine.conf`.
  3. Rewrite `/limine.conf`.
  4. Remove the shadow.

  Each step is synced. An interrupted commit is folded back on the next run.
  Fault injection after every synced step (`ESPCTL_FAULT_STEP=n`, on a FAT
  image, on the host) gave the old or the new config each time. Every kernel it
  referenced was present, and fsck.fat was clean.
- **One-shot trial.**
  - A trial that reaches init is never booted automatically again. A later
    hang plus a power cycle boots good (or factory).
  - A trial that dies before init is booted again by a power cycle. The user
    then picks `LeandrOS good` (or `factory`) in the Limine menu, which shows
    3 s whenever there is more than one entry. Limine also panics on a corrupt
    ELF ("Press a key to return to menu"). That boot then disarms the trial.
- **Reboot path.**
  - `/bin/reboot` sends a request to init.
  - init does the orderly shutdown: SIGTERM, remount read-only, and an f2fs
    checkpoint in the kernel.
  - `reboot(2)` then resets the machine: ACPI RESET_REG, then the keyboard
    controller, then 0xCF9, then a triple fault (x86), or PSCI SYSTEM_RESET
    (aarch64).

  This path already existed; nothing changed here.
- **Access.**
  - Root logs in by key only (`PermitRootLogin prohibit-password`).
  - The key travels on the ESP, so anyone with the stick can add a key. That is
    the same as anyone who can rewrite its root.

## QEMU results (stand-in for the laptop; sticks from write-usb.sh --image, slirp hostfwd)

| check | aarch64 (HVF, MBR stick, run id remdbg) | x86_64 (TCG, GPT stick, run id remdbgx) |
|---|---|---|
| key login as root via the ESP key | yes | yes |
| push/pull 150 MB random, byte-exact (cmp) + sha256 | yes (1.6 s up, 5.5 s down) | yes (8.9 s up, 29.6 s down) |
| directory push/pull, deploy-files with mode, exit status | yes | - |
| deploy-kernel (tag remdbg-test2) --reboot -> `NEW KERNEL RUNNING`, `uname -v` = `#1 remdbg-test2` | yes, 9 s round trip | yes, 12 s |
| hard reset (QMP system_reset) after the trial booted -> factory | yes | yes |
| arm, reboot, promote, reboot -> good | yes | yes |
| deliberately broken trial (truncated ELF): Limine PANIC, reboot reports failure + fallback steps; menu -> good boots; trial disarmed | yes | yes |
| default factory -> boots the flashed kernel | yes | - |
| logs: klog of 4 boots, var-log, status | yes | yes |
| fsck.fat of the ESP after all of the above | clean | clean |

Re-checked after the rebase onto 8e2c1dfb on both arches, with fresh sticks and kernel tag
`remdbg-rebased`. Each arch passed four checks: key login, a 150 MB push, and
`deploy-kernel --reboot --promote`, which booted `#1 remdbg*-test2` as trial. A
reboot afterwards booted the good entry. The full regress suite was not run in
this lane.

## What the user must do once, physically

1. On the Mac: `python3 scripts/remote.py keygen`. This creates
   `~/.ssh/leandros_remote{,.pub}`; this session already created it.
2. Build and flash as usual:
   - `./scripts/build-all.sh --arch x86_64` (with `LEANDROS_WIFI_CONF` if Wi-Fi
     is needed);
   - then `./scripts/write-usb.sh x86_64 /dev/diskN`.

   write-usb now prints `ssh: root key(s) from ~/.ssh/leandros_remote.pub`. A
   stick from an older flash only needs the .pub copied to its ESP as
   `/leandros/authorized_keys`. But the old image has no espctl, so a flash of
   this lane's build is needed anyway.
3. Boot the laptop from the stick with Ethernet or Wi-Fi up. Then find it:
   - `python3 scripts/remote.py discover` (it may get its Arch DHCP address,
     172.16.149.179);
   - or read `DHCP configured, address:` on the console.
4. Connect:
   - `export LEANDROS_REMOTE=<addr>`
   - `python3 scripts/remote.py status`

   The first connection records the host key in `~/.ssh/leandros_known_hosts`.
   After a re-flash, run `remote.py forget-host`.
5. From then on, the orchestrator does everything over SSH. The physical
   fallbacks remain:
   - the power button;
   - picking `LeandrOS good` in the menu after a pre-init failure;
   - re-flashing.

## Open items

- **x86_64 TCG: sshd unreachable after one boot.** This happened once, in the boot
  that followed a Limine panic and a menu pick. Every new connection through
  the slirp hostfwd was reset during kex. The guest listener was present, and
  ping/DHCP worked. Restarting sshd did not help; a power cycle did. It was not
  seen on aarch64 or in any other reboot. It may be slirp state left over from
  polling during the dead period, or a guest TCP accept problem. Watch for it
  on the laptop.
- **The sshd host key changes with each flash.** It is generated on first boot on the
  root fs. It could be persisted on /home or on the ESP.
- **Keys go to root only.** Accounts other than root do not get them.
- **No kernel-only build script.** For the tests, the kernel was built with the
  cargo line from build_kernel into its own target dir. A
  `scripts/build-kernel.sh` would make the laptop loop faster than a full
  build-all.
- **Writes are slow on a cheap stick.** espctl writes one USB command per 4 KiB
  block, because that is how the VFS block path works. Expect a few seconds per
  8 MB kernel on the laptop's stick (QEMU: 0.2 to 1.2 s).
- **Trial failures before init need a person.** Limine has no boot counter, so
  one-shot depends on init, and a trial that fails earlier needs someone at the
  menu. A watchdog-driven reset would not help either, because the default
  is still the trial.
- **No /dev/kmsg.** `espctl klog` is the on-device dmesg; `sync` first for
  the newest lines.
