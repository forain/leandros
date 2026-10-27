# lane/piuserland — 2026-10-27 — DELIVERED

`python3 scripts/remote.py pi-sync [--dry-run] [--list] [--plan-out F] [--board B]`
updates a live LeandrOS root in place from the current aarch64 build (no reflash).

## How
- Manifest: `mkfs-f2fs-populated.py` (the card-prep packer) gained `LEANDROS_ROOT_MANIFEST=<dir>`:
  writes manifest.json (paths incl. hardlink names, mode, uid/gid, size, sha256, symlink targets)
  via `f2fslxpop.Pop.manifest()` and returns before writing an image. pi-sync runs it with
  `LEANDROS_BOARD=rpi5` (default) after `ports/brcmfmac/fetch.sh` + `ports/bluez-firmware/fetch.sh`
  (so BCM4345C0.hcd and the CYW43455 Wi-Fi firmware are in), with LEANDROS_WIFI_CONF /
  LEANDROS_AUTHORIZED_KEYS stripped from the env.
- Remote diff: script 1 = `stat -c` of every path; script 2 = `sha256sum` of regular files whose size
  matches (+ readlink for symlinks). Scripts are uploaded to /tmp/pisync and run with `sh`.
- Upload: no tar in the guest, so changed files are concatenated into <=64 MiB bundles, sent with
  remote.py's resumable sha256-verified uploader, and cut out with `tail -c | head -c` into
  `<dest>.pisync-new`, sha256-checked, chmod/chown, `mv -f` over dest. Hardlink groups: one upload,
  `ln -f` for the other names (coreutils multi-call stays one inode).
- Protection: skip /boot, /proc, /dev, /tmp, /run/user, /mnt, /old_root, /sys/class/block,
  /sys/dev/char/226:*, /etc/shadow, /etc/gshadow, any .ssh/, ssh_host_*, wifi.conf,
  /etc/leandros/network/profiles.*; create-only /etc/leandros, /home, /root, /var; merge
  /etc/passwd + /etc/group (append missing names / group members, never remove). Existing dirs keep
  their mode/owner. Nothing is deleted on the target.

## Evidence (aarch64 QEMU, `scripts/pi-sync-qemutest.sh`, HVF)
Seeded stale: btd/btctl/bthid + BCM4345C0.hcd removed, /usr/share/X11/xkb/symbols removed,
bluetooth group removed, /bin/ps corrupted, /bin/ping chmod 700, markers in /etc/leandros + /home.
- dry run: 216 changes (mkdir 12, new 200, changed 2, meta 1, merge 1), 9,463,609 bytes
- apply: 216/216 in 8 s (1 bundle, 2.6 MB/s over SLIRP)
- second run: `up to date: 0 bytes uploaded`
- sha256 of /etc/shadow + both markers unchanged; bluetooth:…:leandro present, group lines 9 -> 10;
  /bin/btd sha256 == build; /bin/ping back to 755; guest reboots and has the restored files.
- Host checks: `python3 scripts/pi_sync_test.py` (classify + passwd/group merge, idempotence).

## Open / notes
- Main's checked-in-tree f2fs-data0-aarch64.img from 10-25 is the legacy (pre-lxnode) format and is
  refused by the current kernel ("unsupported legacy LeandrOS f2fs format"); if the Pi card predates
  the lx format, pi-sync cannot help (the root would not mount) — it evidently mounts today.
- New users added to /etc/passwd get no shadow entry (shadow is never read or written).
- Pi first run size unknown until the dry run; upper bound = full manifest ~1.8 GiB (2287 inodes).
