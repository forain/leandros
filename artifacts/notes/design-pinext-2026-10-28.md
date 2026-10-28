# design pinext (2026-10-28): BT autostart, PCIe on by default, root sync

Base: main >= 04005dfd. Pi good = 1f82dbdc (slots good/trial/prev on the Pi; card kernel.img = old 4ad83920, never written).
Lanes test aarch64 only. Only the orchestrator boots the Pi. Recover with serial reset or `pi-boot good`.

---------------------------------------------------------------------------------------------------
## 1. BT autostart: ROOT CAUSE FOUND. init's stray sweep kills btd and bthid

None of the suspected causes is right. It is not "started too early", not "no controller", not the socket path or permissions, not the user, and not btctl's check:
- `bt_services()` (userland/init/src/main.rs:894) waits for /dev/hci0, so it is not early.
- btd starts fine. The Pi serial log shows `[btd] ready bdaddr=43:45:C0:00:1F:AC ...` and `bthid: connecting E4:17:D8:82:A2:0A`.
- btctl's check (userland/btd/src/btctl.rs:33, ENOENT/ECONNREFUSED on /run/bluetooth/btd.sock) is correct.

Evidence: scratchpad pcieboot-a9a61b2b.txt lines 211-223 (pcieboot-1069.txt is the same):
```
starting btd and bthid (/dev/hci0 is up)
[btd] ready ...
bthid: connecting E4:17:D8:82:A2:0A
init: killing stray pid 37 (btd)
bthid: btd went away; restarting
init: killing stray pid 38 (bthid)
killed 2 stray process(es) left by the graphical login
## NO GPU RENDERER: the graphical login (COSMIC) was NOT started.
```
The scratchpad logs contain this kill 59 times for btd and 57 times for bthid.

Mechanism:
1. On the Pi, greetd exits because there is no GPU (DM_EXIT_NO_GPU). The `pid == dm_pid` branch (main.rs:655-663) then calls `sweep_strays`.
2. `sweep_strays` (main.rs:1101) SIGKILLs every child of init whose ppid is init, unless its pid is listed in `is_exempt` (main.rs:1115-1119). The exempt list is login, logger, AUTORUN*, SSHD_PID.
3. `spawn_daemon` (main.rs:906) forks btd and bthid as direct children of init, but nobody records their pids. So they are not exempt, and they are killed whenever /dev/hci0 comes up before greetd gives up.
4. `BT_STARTED` is already true at that point, so they are never started again.

Started by hand over SSH, they survive because `from_ssh(pid)` spares them.

The same bug hits `spawn_netctl_stamper()` (main.rs:555 and 933, from lane piwifiap). `netctl watch --stamp` is also an unrecorded child of init, so the Pi kills it at the same moment. That explains why the stamp has never been seen on the Pi.

bthid autostart is already designed in: `spawn_daemon(BTHID_PATH)` runs, and bthid reconnects the remembered pad at boot (`bthid: connecting E4:17:...`). Once nothing kills it, the 8BitDo should work with no manual steps.

### Lane pibtauto (init only, ~40 lines)
Files: userland/init/src/main.rs.
1. Keep the pids of init-spawned daemons in a small table (`static mut DAEMONS: [(kind, pid, started_s, quick_exits); 4]`) for btd, bthid and netctl-stamp.
   - `is_exempt` returns true for any pid in it.
   - Do not special-case names; list pids, as is done for SSHD_PID.
2. Supervise these daemons in the wait4 loop.
   - When `pid` is in DAEMONS, log `<name> exited (status S) after T s`.
   - Respawn with the sshd backoff (1, 2, 4, 8, 16 s, give up after 5 quick exits in under 60 s), but without the blocking `usleep` in the reap path. Record a `respawn_at` time and respawn from the `pid == 0` tick.
   - btd: respawn only while /dev/hci0 exists and `bt=0` is not set.
   - bthid: respawn only when `bthid=0` is not set.
   - netctl stamper: respawn only when /dev/netctl exists.
   - Exit 127 (exec failed) means do not respawn.
3. In `shutdown_system`, the daemons die with everything else as today. No change.
4. Optional, one line: when `graphical_login_wanted()` runs on a board with no /dev/dri/card0, skip greetd entirely. This gives a cleaner Pi boot, but it does NOT fix the bug on its own. Do step 1 regardless.

Tests:
- Host build: `./scripts/build-all.sh` (release); aarch64 QEMU regress.
- QEMU aarch64 no-GPU boot (where the greeter gives up):
  - expect `killed N stray process(es)` lines to name no netctl/btd/bthid;
  - `ps` shows `netctl watch` alive after the NO GPU banner (if /dev/netctl exists in QEMU; otherwise skip).
- Add a regress grep that fails on `init: killing stray pid [0-9]+ \((btd|bthid|netctl)\)`.
- Pi (orchestrator): `pi-deploy` trial, no pi-sync needed, because init lives in the initrd.
  - Expected:
    - `starting btd and bthid (/dev/hci0 is up)`;
    - NO `killing stray pid .. (btd)`;
    - `btctl show` prints the controller (exit 0) with no manual start;
    - `ps` shows btd and bthid;
    - `kill -9 <btd pid>` gives `btd exited (status 9) ...` then btd back within ~2 s, and `bthid: btd went away; restarting` followed by a reconnect.
  - Gamepad: power the 8BitDo on after boot.
    - Expect bthid's connected line for E4:17:D8:82:A2:0A (the worker greps the exact string in userland/bthid/src/main.rs).
    - Expect /dev/input/eventN to appear and `evtest` to show buttons.
    - Reboot (kexec trial again) with the pad on: it reconnects with no commands.
  - MAME: check /dev/input/eventN mode/group lets the MAME user read it (input group). If MAME runs as root this is moot.
- Caveat to note: pairing is keyed to the placeholder bdaddr 43:45:c0:00:1f:ac. A future lane that programs the real OTP/DT bdaddr will need a re-pair.

---------------------------------------------------------------------------------------------------
## 2. PCIe on by default (`pcie.rpi5`)

### Current state
- The only functional gate is kernel/src/init.rs:134, an exact `pcie.rpi5=1` token match. Flipping the default is a one-line change plus doc comments (drivers/src/lib.rs:152, drivers/src/brcmstb_pcie.rs:6, init.rs:131).
- The early reset (kernel/src/main.rs:1067) already runs only after a kexec (`kexec_prevlog().is_some()`), whatever `pcie.rpi5` says. The default flip does not touch it.
- On cold boot it never runs (pikexecdma2 #3).

### Risk review
| Risk | Assessment | Must be inert / mitigation |
|---|---|---|
| Cold boot from the SD card kernel (old 4ad83920) | Unaffected: it is an old binary and cmdline.txt has no knob, so it never touches pcie2. A cold-state bring-up by the new kernel is proven: pcieboot-1069 starts at `link=down` and reaches `rc link up gen2 x4`. | none |
| kexec from the card kernel into the new default-on kernel | The card kernel never mastered RP1. Early reset runs if the old kernel handed over prevlog, which is harmless (`AlreadyInReset`/`Reset{held:true}`). | `pcie.early_reset=0` stays as an escape. |
| kexec from a default-on kernel into an OLDER kernel without early reset (rollback to prev/older good) | The outgoing kernel's Bus stage (pikexecdma2: quiesce gem/xhci/dma40, then PERST# after the APs park) is the primary defence and runs in the new kernel. Safe as long as the outgoing kernel has it, which every default-on kernel will. | Never kexec FROM a pre-pikexecdma PCIe kernel. Not possible after the flip. |
| Outgoing kernel panics or wedges | Serial reset or PM reset resets the whole SoC, including RP1. | Recover only by serial reset or cold boot, never a kexec from a wedged kernel. |
| Link-training failure | link-poll is bounded at 300 ms; any step failure prints `[pcie] ...`, leaves the bus unregistered and `state=failed..`; RP1 probe needs `brcmstb_pcie::is_up() && stage(16)`. | Must stay inert: no rp1::probe, no macb, no rp1_usb attach, no kexec Bus hook registered, no fan write. Verify that the hook registration is conditional on `is_up`. |
| RP1 BAR reads all-ones (pirp1-bar history) | UBUS error suppression returns all-ones, not an SError; rp1_usb checks GSNPSID; RP1 command write is skipped on all-ones. | Same. |
| Fan (RP1 PWM gpio45) | With PCIe on, the fan is set to a FIXED `level=0xAF/255` with "no thermal yet". With PCIe off, the firmware's state remains (fsel 5 on cold). 69% duty is thermally safe and only noise. | Optional follow-up lane pifanthermal: fan curve from `[thermal]` (avs) with failsafe 255 above 75 C. Not a blocker. |
| GPIO dependence | BT (GPIO24-27 + BT_ON) and Wi-Fi (sdio2, WL_ON) are BCM2712 GPIO, not RP1. PCIe off or failed does not affect them. RP1 GPIO: VBUS (42), PHY reset (32), fan (45), power LED (44). | none |
| Polling cost | macb, xHCI x2 and the hotplug scan are polled (`irq none (polled)`; `rp1.irq=1` stays opt-in). Every default boot now pays it. | Matrix row F measures it. If idle CPU rises by more than 2 points, flip `rp1.irq=1` first in its own trial. |
| Routing | With a cable to the Mac's Internet Sharing, eth0 gets 192.168.2.x and may add a default route. Wi-Fi SSH to 172.16.162.184 still works (connected route). Without a cable, eth0 has no carrier and nothing changes. | Check `ip route` in row A/E. |
| Boot time | Adds about 0.4 s. | none |
| QEMU cannot test it | The rpi5 feature has no QEMU target (raspi4b has no pcie2). | Hardware matrix only. QEMU regress just proves nothing else moved. |

### Lane pipcie-default
1. kernel/src/init.rs:134: wanted = `!cmdline has "pcie.rpi5=0"`. Keep accepting `pcie.rpi5=1`. When off, log one line `[pcie] off (pcie.rpi5=0)`.
2. Update the three doc comments. Keep `rp1.irq` default off.
3. Audit, as a note only: every RP1 registration (kexec Bus/Device hooks, usb_hcd pending list, macb netdev, fan) is reached only through `rp1::probe`, which is only reached on `is_up()`.
4. Build rpi5 and raspi4b (check-rpi5.sh) plus aarch64 regress.

Rollout (orchestrator):
- Trial on the Pi, run the matrix, promote, then update the memory note.
- Escape hatch: `remote.py pi-boot good --append pcie.rpi5=0`. The previous good stays in `prev`. The card kernel never changes, so a power cycle always gets a non-PCIe kernel.

Test matrix (trial = default-on build):
| # | Path | Expected lines |
|---|---|---|
| A | power cycle -> card kernel -> `pi-boot trial` (no knob) | `[pcie] rc link up gen2 x4`, `[pcie] state=up`, `[rp1usb] usb0 xhci up`, `[rp1usb] usb1 xhci up`, `[macb] attached`, eth0 DHCP (cable in), Wi-Fi joins, ssh ok; NO `[pcie] early reset` only if the card kernel handed no prevlog (either is acceptable) |
| B | trial -> trial kexec x3 | `[kexec] quiesced: ... gem xhci0 xhci1 dma40` (no FAILED:), `[kexec] bus quiesced: pcie-perst`, new kernel `[pcie] early reset: RC held in reset before mm init`; no `never reached the park page`, no `CORRUPT LINK`, 4 CPUs |
| C | trial with `--append pcie.rpi5=0` | `[pcie] off (pcie.rpi5=0)`, no `[pcie] stage` lines, no eth0, Wi-Fi + BT fine |
| D | trial -> `pi-boot prev` (1f82dbdc, default off) | clean boot, no CORRUPT LINK (proves the outgoing quiesce alone is enough) |
| E | A with Ethernet unplugged | eth0 no carrier, no stall, ssh over Wi-Fi |
| F | idle 60 s, PCIe on vs C | /proc/stat idle delta <= 2 points; else trial `rp1.irq=1` |
| G | 30 min soak | `[thermal]` stable, no `[XHCI] ... controller gone`, no [WDOG] |
| H | USB (see below) | root hubs listed; keyboard hotplug works |

### USB on RP1 xHCI: should it be checked first?
- It is not a blocker for the flip, and the BT gamepad does not need it (BT is uarta, not PCIe). Fold it into row H; no code is needed to check it.
- lsusb does NOT need /sys/bus/usb or /proc/bus/usb. It uses the kernel syscalls USBDEV_COUNT/INFO (userland/libc/src/devinfo.rs; userland/lsusb). init already runs `lsusb --summary` at boot.
- lapusbc (9154980f, root-hub entries) is an ancestor of 1f82dbdc, so the kernel half is on the Pi now. The root's /bin/lsusb may predate it until pi-sync (section 3) lands, or push it to /tmp with remote.py.
- Expected (pcie.rpi5=1 today):
  - two `Linux Foundation 2.0/3.0 root hub` entries, one per RP1 controller (logs show `xHCI 1.10, 3 ports` on bus 1 and bus 2);
  - plug a USB keyboard in: a hotplug line, the device appears in `lsusb`, and typing works on the VT.
- Gap: the USB HID class is boot-protocol keyboard/mouse only (drivers/usb/src/hid/mod.rs:1). An 8BitDo on a USB cable will enumerate in lsusb but will NOT become a gamepad.
  - That needs a later lane piusbhid: a generic HID or hidraw path, ideally reusing bthid's report parser (userland/bthid/src/report.rs) through uinput.
  - Not needed for MAME over BT.

---------------------------------------------------------------------------------------------------
## 3. Root sync (pi-sync, 45 changes, 22 MB)

Recommendation: YES, run it now, before the pibtauto Pi test.
- It carries the piwifiap /bin/netctl (stamp), the current btd/bthid/lsusb, and makes later lanes' tests honest.
- pibtauto itself needs no sync (init is in the initrd), so the order does not matter for that lane.

Why it is slow and risky:
- pi_sync.py:40 sets `BUNDLE_MAX` to 64 MiB. All 22 MB is ONE bundle, which means:
  - one 22 MB upload into /tmp/pisync (on the SD f2fs; only /run/user is tmpfs);
  - then one detached extract that cuts each file with `tail -c +OFF $B | head -c N` (pi_sync.py:376-378).
- If our tail reads through rather than seeking, that re-reads O(files x bundle) bytes: about 45 x 11 MB, roughly 500 MB of PIO reads. This would explain 25 min better than 44 MB of writes at 0.5 MB/s (about 90 s). This is a HYPOTHESIS; measure it (below).
- The starvation is f2fs writes holding F2FS_MOUNTS (IoMutex) through single-sector PIO writeback. sshd blocks on any f2fs access, such as its log. pisdperf already made the extract detached and SIGHUP-proof with retried polls, so starvation no longer corrupts the sync. It only stalls ssh.

Safe procedure, today, no code:
1. Hold the Pi lock. No other Pi lane runs; the Pi runs good 1f82dbdc (single-sector SD, `sdhci.multi` OFF).
   - Do NOT use `sdhci.multi=1` for a root write: the pisd-multi root cause is unresolved, and the root's integrity is at stake.
2. `LEANDROS_REMOTE=pi LEANDROS_PI_HOST=172.16.162.184 python3 scripts/remote.py pi-sync --dry-run --list`: review the 45 entries (no /etc/passwd or shadow surprises, since merges are handled).
3. `LEANDROS_PISYNC_BUNDLE=$((2<<20)) python3 scripts/remote.py pi-sync`. This gives about 11 bundles of 2 MiB:
   - each upload and extract finishes inside a single poll window;
   - the quadratic re-read shrinks about 10x;
   - a failure loses at most one bundle;
   - per-file `mv` is atomic, and a rerun is idempotent (it is hash-planned).
4. Expected: `bundle k/11 ...` progress lines, then `pi-sync: 45/45 change(s) applied, ...`. A second `--dry-run` shows 0 changes.
5. Then `pi-boot good` (fresh start of the synced userland) and `remote.py pi-status`.

### Lane pisync3 (optional, small, scripts only; do it if step 3 still takes more than 10 min)
1. Measure on the Pi first: `time tail -c +20000001 <22MB file> | wc -c` vs `time cat <file> | wc -c`. If tail is about as slow as cat, the quadratic read is confirmed.
2. Fixes, in order:
   - (a) default BUNDLE_MAX 4 MiB;
   - (b) stage bundles in tmpfs: RTMP=/run/user/pisync (root-only 0755 tmpfs). This halves the SD writes; at most 4 MiB in RAM;
   - (c) cut with a seek, `dd if=$B of=X bs=4096 skip=OFF count=N iflag=skip_bytes,count_bytes` (if our dd supports it; else keep tail and rely on small bundles);
   - (d) a `sync` at the end of each bundle bounds dirty data per extract.
3. Tests: pi-sync-qemutest.sh all ok (raspi4b), plus one real Pi run reporting wall time vs the 25 min baseline.

Kernel follow-up, NOT now: SD throughput is the real fix (ADMA2 DMA, or a resolved `sdhci.multi`) together with not holding F2FS_MOUNTS across writeback. These are lanes pisdma and f2fsfair. Start them only after the pisd-multi split (`sdhci.multitest=1` on the Pi) is done.

---------------------------------------------------------------------------------------------------
## Order
1. pi-sync now (section 3, no code). 2. pibtauto (code + Pi test). 3. pipcie-default (code + matrix A-H; row H answers the USB question).
4. Optional: pisync3, pifanthermal, piusbhid.
