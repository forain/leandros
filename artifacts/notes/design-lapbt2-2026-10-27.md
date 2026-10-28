# Design: lane kexit (kthread exit) + lane lapbt2 (laptop Bluetooth userland) — 2026-10-27

Two lanes. `kexit` is small and independent; do it first (it also lets btusb return
instead of parking). `lapbt2` needs one laptop hold and the user for gamepad pairing mode.

---------------------------------------------------------------------------------------------
## Lane kexit — an exiting kernel thread must leave no process behind

### Root cause (established parts)
`sched/src/kthread.rs::kthread_main` ends in `crate::exit(0)`, the **user-process** exit path
(`sched/src/lib.rs:8173`). A kthread is `Task::new_kernel` with `ppid 0, tgid = pgid = sid = pid`,
no address space, no fds. Running process exit on it:
1. **Unreapable zombie.** `log_exit` writes an `ExitRecord` with `parent_tgid 0`,
   `consumed = false` (`parent_autoreaps(0)` is false); nobody can ever wait for pid-0's
   children, so the record is a permanent zombie: counted in `zombie_records()` against
   `MAX_PROCESSES`, and `process_exit_state` keeps reporting it as an unreaped zombie
   ("until a waiter consumes its exit-log record it is still a zombie to userspace").
2. **Process-only hooks run from a kernel thread, at boot**: `EXIT_TEARDOWN_HOOK` (VFS
   close-all IPC for "tgid" = the kthread), `SESSION_LEADER_EXIT_HOOK` (the kthread is a
   session leader: tty hang-up logic), `kill_orphaned_pgrps`, `reparent_children`, the
   pidfd/WAIT poll wakes.
3. **Name race**: `spawn()` calls `set_exe_path(pid, "[name]")` *after* `spawn_with_arg`
   enqueued the task; a closure that returns at once runs `clear_exe_path` first, and the
   spawner then re-inserts `[name]` for a dead pid (stale EXE_PATHS/COMMS entries).

Observed symptom (lapbt x86 regress, `lapbt-x86reg.txt`): pid 1 disappears from ps and
`/proc/1/stat` is unreadable (sudotest starttime "None None", ps -e/-ax/x/aux/-u root).
The exact link from 1-3 to pid 1's /proc entry was **not pinned** within this review's
budget; the prime suspects are (2) the session-leader / teardown hooks acting at boot on
behalf of a task that is not a process, then (1). Step 1 of the lane decides it by
bisection; the fix below removes all three regardless.

### Intended diff
- `sched/src/task.rs`: `pub kthread: bool` on `Task` (default false). Set it only in
  `spawn_with_arg` (not `spawn`, which also builds the boot/init task). Give kthreads
  `sid = 0, pgid = 0` (Linux: kthreads are in session 0) so no session/pgrp logic sees them.
- `sched/src/lib.rs`: `spawn_with_arg_named(func, arg, name: &[u8])`: allocate pid, build
  the task, `set_exe_path(pid, name)` **before** `RUN_QUEUE.lock().enqueue(task)`.
  `pub fn kthread_exit() -> !`:
  ```rust
  pub fn kthread_exit() -> ! {
      let pid = current_pid();
      clear_comm(pid); clear_exe_path(pid);
      {
          let mut rq = RUN_QUEUE.lock();
          if let Some(t) = rq.find_pid_mut(pid) {
              t.exiting = true; t.exit_code = 0; t.wait_reported = true; // born reaped
              t.set_state(TaskState::Zombie);
          }
      }
      yield_now("kthread_exit");
      loop { core::hint::spin_loop(); }
  }
  ```
  and in the dispatch-loop reap (where Zombie slots are removed and `log_exit` is called):
  for `t.kthread`, skip `log_exit` entirely (or log with `consumed = true`), skip the parent
  notice, free the slot + kernel stack as today. Also assert in `exit()`:
  `debug_assert!(!t.kthread)` -> in release, divert to `kthread_exit()` (defensive: any
  kthread calling `exit` directly).
- `sched/src/kthread.rs`: `kthread_main` ends with `crate::kthread_exit()`; `spawn` uses
  `spawn_with_arg_named` and drops its own `set_exe_path`. Update the module doc.
- `drivers/src/bt/usb.rs`: may drop `park()` and return again (keep the no-thread-without-
  controller part).

### Regress test
- **Permanent boot canary** in `kernel/src/init.rs` (after VFS is up, before /bin/init):
  `kthread::spawn("kexit0", || {})` and `kthread::spawn("kexit1", || msleep(50))`, both
  detached. Cost: two kernel stacks for ~50 ms per boot. Every regress run exercises a
  returning kthread from then on.
- `userland/ps/src/test.rs`: new case "returning kthreads leave no trace":
  `ps -e -o pid=,stat=,comm=` has no line containing `kexit`, and no `Z` line whose ppid is 0;
  `/proc/1/stat` reads and parses (22+ fields). Existing ps -e / -ax / x / aux / -u root and
  sudotest's starttime case must pass.
- Optional: `sched` counter `kthread_exits` in `/proc/leandros/...` stats, pstest checks >= 2.
- Lane step 1 (before the fix): build with only the canary, confirm the pstest/sudotest
  failures reproduce in QEMU aarch64, then confirm green with the fix. aarch64 only per lane
  rule; both arches in the merge batch.

---------------------------------------------------------------------------------------------
## Lane lapbt2 — btd/btctl/bthid + COSMIC Bluetooth applet on the laptop

### What already exists (checked in the tree)
- `scripts/mkfs-f2fs-populated.py:429`: `btd btctl bttest bthid` are in the **arch-neutral**
  userland binary list (`userland/target/{target_arch}/release`), so a fresh x86_64 root image
  should already contain them. The laptop's stick predates that (lapbt pushed them to /root/bt).
- `userland/init/src/main.rs:884-900` `bt_services()`: arch-neutral; once `/dev/hci0` exists,
  spawns `/bin/btd` then `/bin/bthid` (`bt=0` / `bthid=0` disable). Nothing x86-specific needed.
- `scripts/build-all.sh:1028`: `ports/cosmic-applet-bluetooth/build.sh "$arch"` for every arch;
  the applet talks bluer -> libdbus -> `leandros-sysbus bluez` (the org.bluez shim over
  btd.sock). Group `bluetooth:x:112:leandro`, btd.sock root:bluetooth 0660.

So lapbt2 is mostly **verification + one root-image refresh**, not new code. Expect fixes
only where x86 differs.

### Steps
1. Build check (x86_64): confirm `userland/target/x86_64-unknown-none/release/{btd,btctl,bthid}`
   are produced and land in `/bin` of the x86 image, the applet binary + its desktop entry are
   in the x86 image (same path as on aarch64), and `leandros-sysbus bluez` is present. Fix any
   aarch64-only gating found (grep `aarch64`/`rpi5` around the bluez shim, the applet port and
   the panel config that lists applets).
2. Confirm `bt_services()` is polled late enough: on the laptop `/dev/hci0` appears ~2 s
   after the root mount (firmware download ~1.3 s). If init only checks during early boot,
   make it check on its existing periodic tick until `BT_STARTED` (the Pi needed the same,
   verify it is not Pi-timed).
3. QEMU x86_64 smoke (no controller): `btd` must not start (no /dev/hci0), applet shows
   "no adapter" without spinning. Optional: `-device usb-host` is not available; skip.
4. Laptop (one `lock.sh laptop` hold; skills `laptop-deploy-kernel` + `laptop-remote-debug`):
   deploy main kernel+initrd **and** refresh userland on the stick (push /bin/btd, btctl,
   bthid, the applet and the sysbus shim, or the full root update path the lane uses);
   reboot; checks over SSH:
   - `dmesg | grep btusb` -> `/dev/hci0 ready`; `ls -l /dev/hci0` = crw------- 10,137.
   - `ps -e | grep -E 'btd|bthid'` -> both running (started by init).
   - `btctl show` -> bdaddr `B4:69:21:8C:5A:7C`, manufacturer 2 (Intel), state ready.
   - `btctl scan 10` -> at least one BR/EDR or LE device; record count + RSSI.
   - Idle cost: `/proc` wake stats for the `btusb` thread before/after the pump-sleep fix
     (review item 5); target < 20 wakes/s with nobody holding /dev/hci0 open... note btd
     holds it, so measure with btd running too.
5. Pairing (needs the user): ask the user to put the **8BitDo Lite 2 (E4:17:D8:82:A2:0A)** in
   pairing mode (per the Pi procedure), then `btctl pair E4:17:D8:82:A2:0A`; expect RESULT 0,
   a line in `/var/lib/bluetooth/linkkeys`; bthid connects PSM 0x11/0x13; `/dev/input/event*`
   for the pad appears; `evtest`-style read shows button presses. Then power-cycle the pad:
   it reconnects with no commands (bthid). Then reboot the laptop: reconnects again (keys on
   the persistent root).
6. COSMIC applet (user logged in leandro/leandro, authorised): the panel shows the Bluetooth
   icon; the applet lists the paired pad as connected; toggle disconnect/connect works.
   Verify via `leandros-sysbus` logs / D-Bus introspection first, screenshot only as last
   resort.

### Known risks to watch
- USB autosuspend/LPM is not implemented, fine; but the xHCI OUT-side `die()` path (review
  item 2) is reachable while pairing (bulk OUT ACL) — keep root-stick health in the log.
- No SCO: headsets are out of scope; A2DP later.
- Intel 8265 + Wi-Fi coexistence: watch Wi-Fi throughput while scanning (shared antenna);
  record iperf before/after `btctl scan`.

### Deliverables
Lane note with the evidence above, any x86 gating fixes, and the pump-sleep fix if not
already done in a separate lane. aarch64 QEMU regress per lane rule; laptop evidence for x86.
