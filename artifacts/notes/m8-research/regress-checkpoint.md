# M8 regression verification checkpoint

Repo: /Users/forain/code/leandros @ e69f71b (clean). Verification/triage only.

## Step log

### 11:25 — took ownership
Found the machine NOT idle:
- pid 93887 `m7v_regress.py aarch64 uefi m8reg3` STILL RUNNING (started 11:25) with a live QEMU.
- pids 76220 / 78754 / 93916: stale `sleep`/`until pgrep` watcher shells (the forbidden pattern).
- pid 91074: shell whose own command line contains the literal `pkill -f qemu-system`
  -> self-matching pattern.
Killed all of them + QEMU + socket_vmnet_client; removed /tmp/leandros-*.sock and the pidfile.

### Evidence on PROBLEM 1 (post-vfstest teardown), run tag `m8reg` @ 11:20:56
- notes/m7v-m8reg-aarch64-vfstest.txt is complete and healthy (`--- vfstest done ---`, prompt back),
  35 PASS / 1 FAIL (xattr_list_f2fs).
- every later test log is only `FileNotFoundError ... s.connect(SOCK)` => /tmp/leandros-serial.sock
  was GONE, i.e. QEMU exited between vfstest and scmtest.
- mtimes prove build-all.sh was writing the disk images DURING that run:
    f2fs-data0-x86_64.img   11:21
    leandros-limine-aarch64.img 11:22
    f2fs-data0-aarch64.img  11:22
    f2fs-data1-aarch64.img  11:23
  i.e. the guest's own backing store was being regenerated underneath the live VM.
- m7v_regress.py clean() uses `pkill -9 -f qemu-system`, which also matches any wrapper
  shell whose command line mentions qemu-system.

Working hypothesis: concurrent build/mkfs over the live images (+ the self-matching pkill),
not a harness-plumbing regression. To be confirmed by a serialized re-run.

## 11:27 — ROOT CAUSE OF PROBLEM 1 CAUGHT IN THE ACT
While my build was running, ANOTHER agent (pid 96750, started 11:26) launched a SECOND
`m7v_regress.py aarch64 uefi m8reg4`, which booted its own QEMU (pid 96827) on top of mine.

That is the whole mechanism:
  run A boots QEMU -> runs vfstest fine
  run B starts -> its clean() does `pkill -9 -f qemu-system` -> KILLS RUN A's QEMU
  run A's remaining tests -> /tmp/leandros-serial.sock gone -> FileNotFoundError, PASS=0 FAIL=0
It is NOT a harness-plumbing regression; the harness is fine when it is the only one running.
The 30-second wall time is the giveaway: run A was cut off, not slow.
Aggravating factor: wrapper shells whose own command line contains the literal
`pkill -f qemu-system` self-match that pattern.

FIX = serialize (one owner) + harness hardening (below). Killed the competitor.

## Harness changes made (/Users/forain/code/leandros-artifacts/m7v_regress.py)
- clean() now uses the non-self-matching pattern `qemu-syste[m]`.
- added vm_alive() (serial socket present AND pidfile pid still alive) checked before and
  after every test; on death it logs a tail of qemu-stderr + serial log and aborts the
  suite loudly instead of emitting nine identical FileNotFoundError logs.

## PROBLEM 2 mechanism found by reading the test (userland/vfstest/src/main.rs:292)
test_xattr_list opens /data/xa_list with O_CREAT|O_TRUNC, then asserts
    raw_listxattr(path, NULL, 0) == 0     (line 300-301)
O_TRUNC clears file DATA but NOT xattrs. On a dirty image the file already carries
user.a and user.b from the previous run, so listxattr returns 14, not 0 -> FAIL on the
very first check. It is the ONLY xattr test with a "must start empty" precondition;
xattr_basic / create_replace / remove are all set-then-check, i.e. idempotent. That is
exactly why this one test, and only this one, fails on a reused f2fs image.
Corroborating: `git diff --stat 93e1a7a..e69f71b` touches ONLY
drivers/src/{drm_device_interface.rs,kms.rs,virtio_gpu.rs} — no VFS/xattr code at all.

## 11:28 — fresh images
Full ./scripts/build-all.sh (both arches, release) then an explicit re-run of
mkfs-f2fs-populated.py for both arches with NOTHING else running, data1 copied from data0.

## Next
Serialized full-suite run: aarch64 (tag m8fresh), then x86_64.
