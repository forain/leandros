# Laptop module-unload trial — 2026-11-01/02

Trial kernel `dc4e6a5cba0e` (`#1 lapunload`, main 8eb1533b, laptop defconfig +
`MODULES=y`, `=m` for HDA, E1000E, USB_HID, USB_VIDEO_CLASS). One-shot trial,
never promoted; good stays `aa47abc412a5`.

## Boot path (dual-boot NVMe)

The firmware boots systemd-boot, whose default is EndeavourOS. `remote.py
deploy-kernel` / `espctl arm` only arm the trial inside Limine, so reaching
LeandrOS needs a one-shot from Linux:

    ssh forain@<laptop> 'sudo -n bootctl set-oneshot leandros.conf && sudo -n systemctl reboot'

LeandrOS has no Wi-Fi link on this install; use wired eth0 (192.168.2.5).

## Results (real hardware)

| module | unload/reload | notes |
|---|---|---|
| uvc | 10/10 | `rmmod` refused (rc 1) while `/dev/video0` is open |
| hda | 10/10 | 34 DMA pages freed per unload; `rmmod` refused while `/dev/pipewire` is open |
| usb_hid | skipped | not loaded (no USB keyboard attached) |
| e1000e | **machine lost** | after the detached loop started, the laptop answered on neither address; netwatch did not reboot it either, so the kernel most likely hung during an e1000e unload or reload on the real I219. QEMU e1000e passes 20 rounds under a ping flood. Needs the previous-boot klog after a power cycle. |

## Caveats found

- The NVMe root has no `/lib/modules` (modules live in the initrd only), so a
  manual reload needs the `.kmod` pushed somewhere (`/root/km`).
- Over `remote.py run`, a `( ... ) &` subshell dies with the session even with
  `trap '' HUP`; `sh -c '...' > f 2>&1 < /dev/null &` survives. The guest has
  no `setsid`/`nohup`.

## Follow-ups

- e1000e reload hang on I219: fetch klog, diagnose (deep-reasoner), fix (fast-worker).
- `deploy-kernel` should set the systemd-boot one-shot itself on a dual-boot ESP.
- Keep initrd modules reachable for manual reload on a disk root.

## Merged-batch regress (main 8eb1533b)

x86_64 66/0 bad; aarch64 66 with 1 bad (futextest `pi_handoff`, stolen=true),
which then passed 3/3 alone: a TCG timing flake. An earlier wedged run was the
box's `/tmp` tmpfs at 100 % (QEMU pauses guests on ENOSPC), not a kernel hang.

## e1000e: cause and fix verified on the real I219, 2026-11-02

Power cycle, then `remote.py logs`: the lost trial boot ended in
`[BUDDY] CORRUPT LINK ... node[..]=... 0xa8c00102a8c08df4` /
"buddy free list corrupt" (mm/src/buddy.rs:75). The corrupted free-list node
holds packet bytes (192.168.x addresses). On the 2nd unload, 1.4 s after a
reload and while the driver was still waiting for link, the I219 kept DMAing
RX into the 258 ring pages exit() had just freed. That is the race the
lane/e1kfix review ranked first: the driver kthread could re-enable the datapath
under the unload.

Fix merged main 08a04c25 (lane/e1kfix2 d9c0c435). The gate is marked stopping
first, then a new `unload_quiesce` pm op runs while bus mastering is still on:
the driver thread is waited out, IRQs are masked, RX goes off, the I219 ring is
flushed when DESC_RING_STATUS asks for it, TX goes off, and DRV_LOAD is kept.
Box regress: 66/0 on both arches.

Trial 80bc981615ff (`#1 lapunload2`, never promoted): `LOOP e1000e ok=10 bad=0`
with 10 s between rounds. There were 10 unloads, each with "[kmod] unload quiesce:
e1000e: interrupts masked, RX off, ring flush not needed (DESC_RING_STATUS 0000),
TX off, DRV_LOAD kept", 32 link-up lines, eth0 traffic afterwards, and no panic,
BUDDY or zombie line. The laptop is back on good aa47abc412a5.

## Follow-ups closed, 2026-11-02

- **Dual-boot reboot** (lane/dbreboot 29739f27): `remote.py reboot` and
  `deploy-kernel --reboot` take `--via-linux USER@HOST` (or
  `LEANDROS_DUALBOOT_LINUX`). After LeandrOS goes down the script waits for the
  default Linux (`uname -s` must say Linux), runs `bootctl set-oneshot
  leandros.conf` and reboots. LeandrOS has no EFI runtime services, so it cannot
  set the one-shot itself. Laptop: deploy → trial and back to good, 64 s each
  way, no manual step.
- **Modules on a disk root** (lane/kmodroot 2e127ace + lane/kmodroot2 1ab0e0b8):
  `/proc/kmod_buildid` and `kmodctl load <name>`. kmodroot assumed initrd files
  stay openable after pivot_root. The laptop refuted that, so init now stages the
  initrd's `lib/modules/<id>/` into `/run/modules/<id>` when the disk root has none
  (laptop: 5 files, 291 KiB). `kmodctl load uvc` and `load uvc.kmod` then work on
  the NVMe root, and a missing module lists both searched paths. `deploy-kernel`
  swaps only the kernel and initrd, so the disk root's /bin/kmodctl stays old until
  a rootfs update; the trial used a pushed copy.
- **kmodregress kmodctl step** (lane/kmodctlverdict 0f031ee4): runtests' 2nd
  argument is the run tag, so `kmodctl kmodctl` ran bare kmodctl (usage, rc 2) and
  kmodctltest never ran. Now `kmodctl --suite kmodctl`: RC=0, overall PASS.
- **Worktree pitfall:** build-all.sh looks for ripgrep at `$ROOT_DIR/../ripgrep`, so
  a worktree under `.claude/worktrees/` builds an image without `/bin/rg`, and the
  guest `grep` (an rg front end) fails. unloadtest's NETCNT parse then crashes.
  The virtio_net reload itself is healthy.
