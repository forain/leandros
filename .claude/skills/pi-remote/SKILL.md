---
name: pi-remote
description: Put a new LeandrOS kernel on the Raspberry Pi 5 over the network and run it with no SD-card swap - upload the rpi5 build into the root, kexec into it, verify, promote. A bad kernel is recovered by a power cycle (the firmware always boots the card's kernel.img, which this flow never writes). Use for every rpi5 kernel change once the Pi has network.
---

# Pi 5: deploy -> kexec -> verify -> promote

Prerequisites: the Pi is up on the network with sshd and the remote key
installed (same as the laptop: see `laptop-remote-debug`, `remote.py setup`).

```bash
export LEANDROS_REMOTE=pi LEANDROS_PI_HOST=<pi-ip>   # or LEANDROS_REMOTE=pi:<ip>
./scripts/build-all.sh --arch aarch64 --rpi5          # release only
python3 scripts/remote.py pi-deploy                   # upload to /boot/pi/trial, kexec, wait, check uname -v
python3 scripts/remote.py pi-status                   # slots, running kernel, cmdline
python3 scripts/remote.py pi-promote                  # trial -> good, old good -> prev
python3 scripts/remote.py pi-boot good                # after a power cycle: back to the promoted kernel
```

How it works
- The Pi's boot FAT (partition 1) is NOT reachable from LeandrOS: the SD driver
  exposes only the f2fs root (partition 2) as `/dev/vdb`. Kernels therefore live
  in the root at `/boot/pi/{trial,good,prev}/{kernel,initrd,info}` and are
  entered with `kexec -l ... --reuse-cmdline; kexec -e` (firmware cmdline kept).
- `kernel` is the rpi5 `target/final-aarch64/kernel-direct` ELF (not
  kernel.img); `pi-deploy` refuses an ELF whose lowest PT_LOAD is not 0x200000.
- Slots: `trial` = last deployed; `good` = promoted; `prev` = the good before.
  `pi-promote` only works while the running kernel was kexec'd from `trial`.
- Recovery: power-cycle. The firmware boots the card's kernel.img (the
  "factory" kernel, last written at card-prep time); then `pi-boot good`.
  Updating kernel.img itself still needs a card swap
  (`scripts/prepare-rpi5-sdcard.sh`) until the FAT partition is exposed.
- `--append ARGS` adds kernel arguments for that kexec only; `--no-exec` stages
  without booting.

Expected serial (uart10) on a kexec
```
[KEXEC] carrying DTB reserved 0x0-0x80000 (left out of /memory)   # BL31; at kexec -l
[KEXEC] exec plan: purgatory=... park=... idmap L0=... (N GiB) ...
kexec_core: Starting new kernel
[KEXEC] stopping 3 other CPU(s) from cpuN
[KEXEC] cpu0: other CPUs off; purgatory ... -> entry 0x200000 fdt ...
... then the new kernel's normal early boot ("[early] dtb reserved: none" is
expected in gen2: the reserved ranges are absent from /memory instead).
```
`[KEXEC] cpuN mpidr=... still not OFF` means BL31 refused PSCI CPU_OFF: the
new kernel will fail CPU_ON for that core (ALREADY_ON) but still boot.

Rules: hold `$SP/lock.sh pi` (if the orchestrator defines it) across
deploy -> verify -> promote; check `pi-status` shows YOUR trial before promoting.

## Updating the root fs (userland) without reflashing: `pi-sync`
```bash
./scripts/build-all.sh --arch aarch64                 # userland/rootfs build output
python3 scripts/remote.py pi-sync --dry-run           # per-directory summary + total bytes
python3 scripts/remote.py pi-sync --dry-run --list    # every change
python3 scripts/remote.py pi-sync                     # apply (then reboot or pi-boot good)
```
- Manifest = the card-prep packer (`mkfs-f2fs-populated.py`, `LEANDROS_BOARD=rpi5`,
  CYW43455 + BCM4345C0.hcd firmware fetched) in manifest mode; diff by
  `stat` + `sha256sum` on the Pi; changed contents go up in <=64 MiB verified
  bundles via /tmp/pisync and are renamed into place per file after a sha256 check.
- Never touched: /boot (kernel slots), /etc/shadow, /etc/gshadow, any .ssh/, ssh host
  keys, wifi.conf, /etc/leandros/network/profiles.*, /tmp, /run/user, /proc, /dev,
  intercepted /sys nodes. Create-only: /etc/leandros, /home, /root, /var.
  Merged: /etc/passwd, /etc/group (missing entries and group members appended).
  Files the build does not have are left in place.
- A second run must print `up to date: 0 bytes uploaded`.
- Test against QEMU: `$SP/lock.sh qemu scripts/pi-sync-qemutest.sh` (stale-seeded guest,
  apply, idempotence, protected-file hashes, group merge, reboot).
