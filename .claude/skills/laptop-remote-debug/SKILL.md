---
name: laptop-remote-debug
description: Debug LeandrOS on the real laptop (HP EliteBook 840 G5, booted from the USB stick) over SSH instead of re-flashing - run commands, push/pull files, deploy userland binaries, fetch the console log (dmesg) and other logs, reboot and wait, recover from a bad boot. Use whenever a change must be tried on the real laptop/PC hardware, or to collect its boot logs. Also works against a QEMU VM (LEANDROS_REMOTE=qemu).
---

All of it is `scripts/remote.py` (Mac OpenSSH -> the guest's russh sshd, root by
key) plus `/usr/sbin/espctl` in the guest. Run from the repo root.

## Reach the machine

```sh
export LEANDROS_REMOTE=<laptop-ip>        # or host:port; `qemu` / `qemu:<run-id>` for a VM
python3 scripts/remote.py discover        # scans the Mac's /24 for the russh banner
python3 scripts/remote.py status          # kernel, uptime, booted slot, boot entries
```

- Key: `~/.ssh/leandros_remote` (override `LEANDROS_REMOTE_KEY`). Root logs in by
  key only. The key reaches the stick through the ESP (`write-usb.sh` copies
  `~/.ssh/leandros_remote.pub` to `/leandros/authorized_keys`; `espctl boot`
  imports it into `/root/.ssh` on every boot). No key on the stick ->
  `remote.py setup` prints the one-time steps.
- Host keys go to `~/.ssh/leandros_known_hosts`. A re-flashed stick has a new host
  key: `remote.py forget-host`.
- The laptop gets its address by DHCP (ethernet or WPA2 from /etc/leandros/wifi.conf).
  The serial console line `DHCP configured, address: X` names it too.

## Everyday commands

```sh
remote.py run 'uname -a; cat /proc/cmdline'      # exit status passes through
remote.py shell                                  # interactive (pty)
remote.py push ./file /root/file                 # sha256-verified, temp + rename, skips identical
remote.py push ./dir /root/dir                   # recursive
remote.py pull /var/log/sshd.log ./              # sha256-verified
remote.py deploy-files userland/target/x86_64-unknown-none/release/foo:/bin/foo:755 ...
remote.py deploy-files --manifest files.txt      # lines LOCAL:REMOTE[:MODE]
remote.py logs                                   # -> artifacts/remote-logs/<time>-<host>/
remote.py reboot                                 # sync, reboot, wait for sshd, report, fetch logs
```

- `logs` saves `klog-current.txt` (this boot's console log = kernel + console
  output, the dmesg), `klog-all.txt` (the 4 boots kept in the stick's log
  partition), `status.txt`, `partitions.txt`, `limine.conf`, `var-log/` (sshd.log,
  ...) and `/tmp/*.log`. In the guest: `espctl klog [--all|--list|--boot N]`;
  run `sync` first for the newest lines.
- Userland binaries: a replaced file is renamed over the old one, so a running
  binary keeps its old inode; restart the process (or reboot) to use the new one.
  The root fs is the stick's f2fs and persists; /tmp is RAM.
- Kernel + initrd go through the boot entries, never `push`: see the
  `laptop-deploy-kernel` skill.

## When a boot fails

`remote.py reboot` / `deploy-kernel --reboot` wait (default 300 s) and, if sshd
never comes back, print what to do. The rules:

1. Trial kernel armed and it got as far as init: init already set the default
   back to the fallback -> a power cycle boots the good kernel by itself.
2. Trial kernel died before init (or Limine refused it: a Limine `PANIC`, any
   key returns to the menu): a power cycle boots it again -> pick
   `LeandrOS good <id>` (or `LeandrOS factory`) in the Limine menu, which shows for
   3 s whenever more than one entry exists. That boot's `espctl boot` sees the
   trial was skipped and disarms it.
3. Once up again: `remote.py logs` (klog-all.txt holds the failed boot if the
   kernel got far enough to log), `remote.py discard` to drop the bad trial.
4. Last resort: re-flash the stick on the Mac (`scripts/write-usb.sh x86_64
   /dev/diskN`, keeps /home), or read its log partition there with
   `scripts/read-usb-log.sh /dev/rdiskN --all`.

Ask the user for the physical steps (power button, menu choice, re-flash); say
exactly which entry to pick.

## QEMU stand-in

```sh
LEANDROS_RUN_ID=x LEANDROS_NET=user LEANDROS_USB_STICK=/path/stick.img python3 .claude/skills/run-leandros/driver.py start x86_64
LEANDROS_RUN_ID=x LEANDROS_REMOTE=qemu python3 scripts/remote.py status
```

`qemu` reads the SLIRP hostfwd port the driver recorded in
`/tmp/leandros-<run-id>-ssh`. Use `LEANDROS_NET=user`: under socket_vmnet the
Mac cannot reach the guest ("No route to host"). Build the stick image with `scripts/write-usb.sh
<arch> --image <img> [--gpt]` so the ESP, log partition and boot entries are
the laptop's. A guest reboot keeps QEMU running (UEFI reset), as on hardware.
