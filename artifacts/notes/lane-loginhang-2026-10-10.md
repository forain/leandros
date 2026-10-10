# lane/loginhang — 2026-10-10 — logins freeze after a greeter login (nvrm-boot10)

Branch `lane/loginhang` from main `139f451`. Not pushed. Input:
`nvrm-boot10.log` (real PC: RTX 3070, USB-stick root on f2fs over xHCI/BOT,
`nvrm.start=1 nvrm.vktest=0 net.test=1 snd.test=1`), compared with
`nvrm-boot9.log`.

## 1. Symptom

The greeter came up. The password was typed, the screen went back to the
text console, and the desktop never started. After that, root logins on VT1
and VT2 froze right after the password, though VT switching still worked.
The on-stick klog ends at about +63 s, at the moment of the login: the last
`[nvkms] frames` line shows typing, and nothing after it was persisted. There
are no LOCKHOLD, LOCKWAIT, WDOG or KSYNC lines.

## 2. Root cause: a thread reaped inside the kernel while it held a sleeping lock

lane/fsio (2026-10-09) made `F2FS_MOUNTS` (an `IoMutex`) and the per-disk USB
lock (`KMutex`) **sleeping** locks. A thread doing filesystem I/O now parks
in the kernel while it holds them, for example in the USB poll loop's
`sleep_until`, or queued in the FIFO with its `WaitNode` on its kernel stack.

`exit_group`'s group kill (`sched::kill_next_group_member`) reaps every
sibling that is "Ready/Blocked and not on any CPU" **in place**. It removes
the task and frees its kernel stack. Before fsio that was safe: nothing could
be off-CPU in the middle of the filesystem. Now it is not:

- If the reaped thread **held** the lock, the lock stays owned by a dead pid
  for good. Nothing ever unlocks it, and since `[LOCKHOLD]` is printed on
  release, nothing is ever printed about it.
- If the reaped thread was **queued** for the lock, its `WaitNode` stays linked
  from a freed stack. The next unlock writes `granted` into freed memory and
  hands the lock to the dead pid. The result is the same wedge, plus memory
  corruption.

An on-CPU sibling that the kill marked Zombie had the same hole. It was reaped
at its next switch-out, which is usually exactly the park inside the I/O path.

At login the greeter (multi-threaded: tokio/iced) and its compositor exit
while their threads are mid-I/O, which is the `[LOCKHOLD] ...
cosmic-greeter-login held it 2733 ms` kind of I/O seen earlier in boot10. The
filesystem lock and the stick's disk lock leaked. From then on:

- Every login blocks at its first filesystem access after the password
  (`/etc/passwd`, `/etc/shadow`, then exec of `/bin/brush`).
- The session never starts, because exec reads from the disk.
- The klog flusher (which takes the disk lock) and the iostat task (whose
  `writeback_tick` takes `F2FS_MOUNTS`) block. That is why the stick log and
  the `[usbms]`/`[fslock]` summaries simply stop.
- VT switching and the USB keyboard keep working. They never touch the
  filesystem, and the xHCI controller lock is a spinlock that is never held
  across a sleep.

boot9 had no graphical session, so no multi-threaded process exited
mid-I/O, and the VT2 login worked.

### Reproduction (QEMU, Mac)

- `killmt 40 fs_exit` (new mode: 3 workers in root-fs I/O, one calls
  exit_group) on the **plain virtio root**, baseline kernel: `FAIL iteration 0:
  probe child could not read /etc/passwd within 15 s: filesystem wedged`.
  After that the shell could not even write its status file, and there were no
  `[LOCK*]`, `[KSYNC]` or `[WDOG]` lines. This is the PC signature.
- The PC scenario itself, baseline kernel, aarch64 HVF, USB-stick root via
  qemu-xhci with the PC-like knobs `usbms.delay_us=1000 usbms.wdelay_us=40000`
  (the boot10 stick does about 1-3 ms per read and 35-50 ms per write): greeter
  up, greeter login typed, then `[DRM] fb ... removed while scanned out
  (creator closed)` (the greeter's compositor exited). The serial root shell
  then wedged: `cat /proc/*/cmdline` timed out 3 x 200 s. The `[usbms]` and
  `[fslock]` summaries stopped. It reproduced on the first try.
- With the fix: `killmt 300 fs_io`, `killmt 300 fs_exit` and all 13 modes
  x 100 pass, and the same stick scenario logs in and starts the desktop
  (section 6).

### Hypotheses ruled out

- **(A) A write storm / starvation alone.** `F2FS_MOUNTS` is a FIFO sleeping
  mutex, so the klog flusher (which does not even take it) and the logins
  would get their turn. The boot10 stick's writes are 30-50 ms on average,
  not 0.5 s. A storm makes the system slow, not permanently stuck. A storm
  *does* exist (section 3) and is fixed too.
- **(C) A hung USB command.** Every transfer has a 5 s deadline. A timed-out
  transfer whose endpoint recovery fails declares the controller dead and
  halts it, which would also have killed the USB **keyboard** on the same
  xHCI, yet VT switching worked. HID polling uses `try_lock` on the
  controller, so the controller was alive and its lock free. Defensive
  measures were still added (section 4).
- **(B) Classic lock-order inversion** (mmap fault under the lock, klog under
  the lock, NVIDIA paths): none found. The klog flusher takes only its own
  state `try_lock`, the block registry briefly, and the disk lock. The f2fs
  entry points were audited in lane fsio. The deadlock was a *leak*, not an
  inversion.

## 3. Fixes

1. **Held-lock accounting + deferred reaping** (`sched/src/ksync.rs`,
   `sched/src/lib.rs`, `sched/src/signal.rs`, `sched/src/task.rs`).
   - `ksync::klocks_held(pid)` counts the sleeping locks a task owns or is
     queued for: KMutex/KRawMutex (so `F2FS_MOUNTS`, the USB disk locks and
     the RM mutexes) and KRwLock. The unlock decrements the *recorded owner*,
     so a non-leader exec's pid exchange cannot skew it.
   - The group kill skips such a member (`GroupKillStep::Deferred`). The
     killer sleeps 1 ms per round and prints `[EXIT] pid N: waiting ... for
     thread T` every 5 s if it waits long. The member finishes its kernel
     work. At its return to user space, `deliver_pending_signals` sees its
     group dying (`group_exit_owner` is someone else) and parks it for good,
     off-CPU, where the next round reaps it.
   - An on-CPU member marked Zombie (`group_killed`) that has since taken a
     lock is made runnable again instead of being reaped at switch-out.
   - `take_over_leader` (non-leader exec) also defers a lock-holding leader.
     A `stop_pending` thread is not stopped while it holds a sleeping lock,
     because a SIGSTOP'd holder would stall the filesystem until SIGCONT.
   - The leader is reaped last, after every other member.
2. **f2fs open-file slots leaked on every exit and exec** (`servers/vfs`).
   `release_vnode` (used by close-all at exit and by the close-on-exec sweep)
   never sent `VFS_CLOSE` for a `MountedFile`. Every process that died (or
   exec'd) with an f2fs file open lost one of the mount's 256 slots for good.
   `killmt fs_io` ran the pool dry (`[F2FS] EMFILE: mount open-file pool
   FULL`, then EMFILE for every open, logins included) after about 100 kills.
   It now closes on the last reference, the same rule as `handle_close`.
3. **Unlocked checkpoints (A)** (`servers/f2fs`). fsync, the periodic
   write-back and the routine `maybe_flush` trigger now run a checkpoint in
   three phases:
   1. Under the lock: copy the dirty blocks, pin their slots (`wb`), and build
      the commit block.
   2. Without the filesystem lock: write the copies coalesced, flush, write the
      commit block, flush.
   3. Under the lock, briefly: unpin.

   Checkpoints are serialised by `CP_MUTEX`, which is taken before
   `F2FS_MOUNTS` and never inside it. A synchronous checkpoint (unmount,
   remount-ro, shutdown, the 3/4-dirty emergency) or the eviction of a
   dirty or pinned slot first waits for phase 2 (`wait_wb_idle`), so an older
   copy never lands on top of a newer write. The commit block still describes
   exactly the snapshot, written after it. A hard kill of QEMU right after
   `sync` kept every file. In the slow-stick emulation, cosmic-panel's
   fsync storm (about 4 fsyncs/s for ~35 s after the desktop appears) no
   longer holds the lock across write-back. A full login path took 1.6-1.95 s
   before this change and 0.09-0.12 s after. True per-file fsync (f2fs
   roll-forward) was not attempted: this checkpoint format has no recovery
   for it. The new `[fslock]` fields (`N checkpoints, M fsyncs (last pid P)`)
   name the storm's source.
4. **USB MSC (C), defensive** (`drivers/usb/src/msc/scsi.rs`). Every failed
   attempt, every BOT reset recovery and the final outcome are logged
   (`[USB-MSC] lun L cmd 0x2a ...: attempt k failed: Transport(Timeout) (N ms
   into the command)`). Before, they were silent until the whole 6-attempt
   ladder ran out, which could take minutes. The ladder also gives up after
   30 s (`LADDER_BUDGET_NS`), so a dead stick turns into EIO in bounded time.
   msc-tests now builds `platform.rs`: 26/26.
5. **Hung-task detector** (`kernel/src/hungtask.rs`). A kernel thread that
   never takes the filesystem or a disk lock; it only reads atomics and
   try-locks the run queue. Every second it checks whether a task has slept
   >= 5 s for `F2FS_MOUNTS`, the lock has been held >= 5 s, or a USB disk
   command has been in flight >= 5 s. If so, it prints to VT 1, serial and
   the klog, every 10 s while the condition lasts:
   - the oldest waiter;
   - the holder: pid, executable, scheduler state, wait channel, klocks, or
     `GONE (no such task: the lock is LEAKED)`;
   - what the holder is doing: the f2fs operation or the lock site
     `file:line`, and checkpoint write-back progress `k of N blocks written,
     R remaining`;
   - the in-flight USB command: op, LBA, size, age, issuing pid;
   - the disk-lock owners.

   It writes through `console_print_urgent`, past the staged outbox, so a
   saturated UART cannot drop it. Verified with `usbms.wdelay_us=7000000`:

       [HUNG] usbms disk0: WRITE lba 1063032 4 KiB in flight 5.1 s, issued by pid 31 (/bin/sync) state R klocks 2
       [HUNG] F2FS_MOUNTS: 1 waiter(s), oldest asleep 9.7 s: pid 34 (/bin/sleep) state S wchan 0xffffff03 klocks 1
       [HUNG] F2FS_MOUNTS holder (held 9.7 s): pid 33 (/bin/sync) state S wchan 0xffffff03 klocks 2; doing: holding the lock at servers/f2fs/src/lib.rs:2013 for 9.7 s
6. **Line-atomic console and klog** (`kernel/src/main.rs`, `kernel/src/klog.rs`).
   The garbled `[nvuapi]` pairs came from the outbox-overflow path. With the
   real 16550 saturated by the nvuapi flood, a finished line was written
   straight out past `CONSOLE_OUT_LOCK` from several CPUs at once.
   - Each staged kernel line is now pushed into the klog ring as **one**
     reservation when it is queued (`klog::push_bytes`). User writes are one
     reservation per write.
   - On overflow the line is dropped from UART/screen only. It is already in
     the stick log, and the next drain prints `[console] N kernel line(s)
     dropped: UART saturated`.
   - The flusher snapshots `HEAD` only when no reservation is mid-copy.
   - A drain writes at most 1024 entries per call (~90 ms at 115200 baud with
     IRQs masked) instead of up to the whole 8 KiB outbox (~0.7 s).

## 4. Measurements

QEMU on this Mac: aarch64 HVF, stick root via qemu-xhci with the knobs
`usbms.delay_us=1000 usbms.wdelay_us=40000`, `ffsession.py --nofirefox`,
virgl. The "text login" is `printf 'root\nroot\n' | /bin/login` from the
serial root shell after the desktop is up. It runs the real login path
(passwd/shadow/group reads, setuid, exec of /bin/brush, verified to reach the
shell).

| | baseline 139f451 | + reap fix | final (+ unlocked checkpoints, build-all) |
|---|---|---|---|
| greeter up (after serial login) | 7.0 s | 0.8 s | 0.9 s |
| greeter login -> desktop (cosmic-panel) | **never** (fs wedged) | 7.4 s | 8.1 s |
| desktop -> cosmic-term | - | 4.0 s | 8.4 s |
| text logins after the session | hang | 1.62 / 1.95 / 1.64 s | 0.25 / 0.03 / 0.02 / 0.02 / 0.02 / 0.01 s |

Other checks:

| check | result |
|---|---|
| `./scripts/build-all.sh` (both arches, release) | ok (17 min) |
| runtests regress aarch64 (virtio) | 15/15, no KSYNC/WDOG |
| runtests regress + killmt x86_64 (virtio, TCG) | 16/16 (killmt 13 modes x 100, fs_io / fs_exit included) |
| killmt aarch64: fs_io 300, fs_exit 300, all 13 modes x 100 | all PASS (baseline: fs_exit FAIL at iteration 0, wedged) |
| vfstest + f2fstest aarch64 | 65 PASS, 0 FAIL |
| hard kill of QEMU right after `sync`, reboot | all files present with their content |
| COSMIC smoke aarch64 (virgl, virtio) | greeter 0.7 s, desktop 0.7 s, terminal 0.7 s |
| COSMIC smoke x86_64 (TCG, virtio) | greeter 1.3 s, desktop 15.4 s, terminal 1.3 s |
| hung-task detector, stick with `usbms.wdelay_us=7000000` | `[HUNG]` usbms and F2FS_MOUNTS reports every 10 s (section 3.5) |
| drivers/usb/msc-tests (host) | 26/26 |
| x86_64 kernel `--features nvidia` (libnvrm.a rebuilt) | compiles, stack frames OK; default build left in place |

## 5. What the next PC boot should show

- A greeter login starts the desktop. VT1/VT2 root logins work during and
  after the session.
- If anything still wedges the disk, VT1 shows `[HUNG]` lines every 10 s.
  Photograph them: they name the holder (or `GONE`), what it is doing and the
  USB command age.
- `[USB-MSC] ... attempt k failed` lines if the stick misbehaves.
- `[fslock] ... N checkpoints, M fsyncs (last pid P)` while the session
  writes. On the real stick (~40 ms writes) the cosmic-panel storm should
  keep the device busy for roughly the first half minute.
- No more byte-interleaved lines in the stick log. `[console] N kernel
  line(s) dropped` means the UART (not the log) lost lines.

## 6. Not done / untested

- Not run on the real PC: the reaping race window and the stick's own write
  latency differ there.
- x86_64 stick-in-QEMU has no GPU (no greeter), so the stick + greeter test
  ran on aarch64 only.
- True per-file fsync (roll-forward) was not done.
