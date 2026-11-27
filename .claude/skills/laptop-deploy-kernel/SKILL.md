---
name: laptop-deploy-kernel
description: Put a newly built LeandrOS kernel (and initrd) on the real laptop's USB stick over SSH and boot it once, safely - a bad kernel cannot brick the stick because the previous good kernel and the flashed factory kernel stay as Limine entries. Use for every kernel change that must be tested on the laptop/PC hardware, for kernel command-line knob A/B tests, and to promote or roll back a kernel.
---

Prerequisites: the `laptop-remote-debug` skill (LEANDROS_REMOTE set, key login
works). Release builds only.

## Round trip

```sh
./scripts/build-all.sh --arch x86_64                 # release; LEANDROS_KERNEL_TAG=<label> to name it
python3 scripts/remote.py deploy-kernel --reboot     # upload, verify, arm, reboot, check, fetch logs
python3 scripts/remote.py promote                    # only after the new kernel proved itself
```

`deploy-kernel` takes `target/final-<arch>/kernel` + `initrd-<arch>.cpio`
(override `--kernel/--initrd`; arch = the target's `uname -m`). It uploads them
sha256-checked, and `espctl install` copies them to the ESP as
`/leandros/k/<id>/{kernel.elf,initrd.gz}`, reads them back from the medium, and
only then rewrites limine.conf with the new `LeandrOS trial <id>` entry as the
default. With `--reboot` it waits for sshd and prints `NEW KERNEL RUNNING: trial
<id>` plus `uname -v` (`#1 <build tag>`), or says it is not. `--promote` promotes
right away when the check passed.

`--netwatch S` appends `netwatch=S` to the **trial** entry's cmdline only
(default 180 on real machines, off on QEMU; `--netwatch 0` = off). The trial
kernel then watches the network for the whole trial, not only at boot
(`kernel/src/netwatch.rs`):
- no IPv4 address S seconds after boot -> reboot;
- trial watch (only when booted as `leandros.slot=trial`): every
  `netwatch.every=` (10 s) it pings `netwatch.host=a.b.c.d` (default: the
  default gateway, which must answer ARP first, so an associated-but-dead data
  path counts as down). No reply for `netwatch.reach=` (300 s; `0` = off) AND
  the last `netwatch.fails=` (3) probes failed -> reboot. Hysteresis: a Wi-Fi
  blip shorter than `reach` only logs `[netwatch] <gw> not answering` /
  `answers again`, so a long measurement survives it;
- `--trial-max M` (`netwatch.trial_max=M`, minutes; `90s` also works; default
  off): an un-promoted trial reboots after M minutes regardless.
The reboot lands on good: `espctl boot` in init already made the fallback the
default when the trial reached init. The reason line is in the previous boot's
klog (`remote.py logs`). Tune with `--append 'netwatch.reach=600'`. Example:
`remote.py deploy-kernel --reboot` (netwatch 180 + reach 300 implied).

**f2fs roll-forward guard.** A kernel older than da51b45e (f2fsync) ignores the
f2fs roll-forward fsync records, so after an unclean shutdown of a new kernel it
would lose every acknowledged fsync of the last <=30 s. `deploy-kernel` reads the
build revision of the good entry's kernel from the ESP (`espctl cat`, the
`LEANDROS_KERNEL_TAG` prefix) and, if it is not a descendant of da51b45e (or
unknown), appends `f2fs.rf=0` to the **trial** cmdline (every fsync is then a
full journaled checkpoint). It prints the decision; `--no-rf-guard` skips it.
`promote` strips `f2fs.rf=0` from the good entry (set a deliberate one again with
`espctl cmdline good`). Current state: the laptop's good kernel postdates
f2fsync, so no knob is added; only the flashed `factory` entry (a3f226cc)
predates it. It is never guarded, so booting factory after an unclean shutdown
of a new kernel loses the last <=30 s of fsyncs until the stick is re-flashed.

**systemd-boot dual boot (EliteBook NVMe).** The firmware boots systemd-boot whose default is Linux; LeandrOS is `leandros.conf`. The kernel has no EFI runtime SetVariable, so LeandrOS cannot set `LoaderEntryOneShot` itself. Use `--via-linux USER@HOST` (or `LEANDROS_DUALBOOT_LINUX=forain@172.16.149.179`) with `reboot` / `deploy-kernel --reboot`: after the trial is armed and LeandrOS reboots, remote.py waits for the Linux sshd, runs `sudo -n bootctl set-oneshot leandros.conf && sudo -n systemctl reboot`, then waits for LeandrOS as usual. Opt-in; a no-op when unset, and ignored on QEMU and the Pi.


Promote: `remote.py promote` (or `deploy-kernel --reboot --promote`, which
promotes only when the check passed). Promoting copies the trial's cmdline to
the good entry and then strips `netwatch=`/`netwatch.*` from it, and `espctl
promote` disarms the running kernel's watchdog (`NETCTL_NETWATCH_DISARM`;
console: `[netwatch] disarmed (promoted)`), so a promoted kernel never reboots
itself over a network outage, neither now nor on later boots.

Kernel arguments: `--cmdline 'i915.kmsdump=3 klog.flush=2'` replaces the extra
arguments of the new entry (default: the good entry's). Same kernel, other knobs:
deploy the same build again with another `--cmdline`. Change a slot in place:
`remote.py espctl cmdline good <args...>`.

## The boot entries (Limine on the stick's ESP)

| entry | what | when it boots |
|---|---|---|
| `LeandrOS trial <id>` | last deployed kernel | once: `espctl boot` in init sets the default back to the fallback as soon as the trial reaches init |
| `LeandrOS good <id>` | last promoted kernel | default otherwise |
| `LeandrOS factory` | the flashed `/kernel.elf` | fallback when there is no good one; never modified |

- Each entry's cmdline carries `leandros.slot=<slot> leandros.kid=<id>`;
  `espctl status` / `remote.py status` say what is running.
- The menu shows 3 s when there is more than one entry (`espctl timeout N`).
- limine.conf is replaced through a shadow copy (`/limine/limine.conf`, which
  Limine finds first), so a power cut at any point leaves a complete config.
  The flashed config is kept as `/leandros/factory.conf`; `espctl reset`
  restores it and removes all slots.

Other commands (`remote.py espctl <cmd>` runs any): `status`, `arm` (boot the
trial once more), `disarm`, `discard` (drop the trial), `default
<trial|good|factory>`, `promote [--force]`, `ls/cat/get/put/rm` on the ESP.

## If the new kernel does not come back

See "When a boot fails" in `laptop-remote-debug`: trial reached init -> power
cycle boots the fallback; died earlier -> the user picks `LeandrOS good`
(or `factory`) in the Limine menu. Then `remote.py logs` and `remote.py discard`.
Never `promote` a kernel that did not boot.

## Rules

- **Never print `/proc/cmdline`** (or `espctl status -v` output containing it):
  it can carry `wifi.psk=` / `iwlwifi.psk=`. `remote.py logs` redacts both
  (`<redacted>`) in the files it saves; do not bypass that by dumping raw logs
  or cmdlines into chat, notes or commits. Never write a password or PSK
  anywhere.
- **Playback checks need a known-good video and a timeout.** Use
  https://www.youtube.com/watch?v=aqz-KE-bpKQ (Big Buck Bunny). Videos get
  removed ("This video isn't available anymore"), and a check that waits for
  playback without a limit then stalls forever. Treat playback as started only
  when the pipewire sink counters advance and RDD is busy. If neither happens
  within 2 minutes, report the check as failed.
- **Screen-lit rule for Firefox/YouTube checks.** After about 15 minutes
  without input COSMIC blanks the panel and the compositor stops sending frame
  callbacks, so a Firefox started then sits at ~0 % CPU on every kernel. That
  looks like a hang but is not one (`artifacts/notes/lane-ffhang-2026-10-18.md`,
  `docs/development/remote.md`, "Measuring on the laptop"). Start the check
  within ~15 minutes of the login, or have someone touch the laptop, and only
  compare kernels under the same screen state. The i915 `flips N/s` lines in
  the klog show whether the panel was updating.
- **Starting the desktop remotely** (so the 15 minutes start from a known
  point): from a root SSH login, speak greetd's IPC to the greeter's socket
  (`create_session`, answer the auth message, `start_session`), as described in
  "Starting the desktop remotely" in `docs/development/remote.md`. The password
  is supplied only at run time (environment variable or prompt), never stored in
  a script, file, log or commit. Then start Firefox as `leandro` with `su` /
  `sudo -u leandro env WAYLAND_DISPLAY=wayland-1 ...`.

## Use scripts/device-update.sh

Prefer `scripts/device-update.sh laptop <expected-tag> [--via-linux USER@HOST]`
(release x86_64 build in the current tree; LEANDROS_REMOTE set). It re-execs
itself under ONE `lock3.sh laptop` hold and does deploy-kernel --reboot, checks
(booted trial id == deployed id, `uname -v` has the tag, ssh, klog without
panic/WDOG/oops, no netwatch reboot within its window) and `promote` in that
same hold; on any failure nothing is promoted and it exits 1. Do not split
deploy and promote into two lock holds.
