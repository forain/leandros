# lane/termsegv, 2026-10-25: cosmic-term teardown SEGV (openbugs L12 #47)

## Verdict: UNREPRODUCED flake (no fix). Main c4d0b9e6 (= bd44a329 + notes), release builds.

## Original fault (integ14, /tmp/ffsession/cs-integ14-x86_64/serial.log:1218-1219)
- `[PF] SEGV pid=505 tgid=496 thr=2 exit_owner=0`, W to 0x401e7d18, pc=0x300a0b34, 131.12 s, 0.7 s after Super+Q closed the window.
- pc is in musl libc.so (loaded at base 0x30000000): offset 0xa0b34 = `memcpy` (src/string/x86_64/memcpy.s:18).
- Target 0x401e7d18 lay in no VMA (VMA below ended at 0x401e6000; PTE 0). It is NOT tid 505's own stack (Ctrl-T dump: sp=0x4169b060).
- Only 2 threads were left (thr=2), and no exit_group was in progress (exit_owner=0). The leader was doing ordinary teardown: its last syscall was close at the 1st fault, then munmap at the 2nd, and nvma fell from 394 to 388 between them.
  tid 505 is an epoll event-loop thread (Ctrl-T dump: blocked in epoll_pwait, like 507/519 and the leader).
- Two PF lines per fault is expected. The first raises SIGSEGV, Rust std's handler finds no guard page, resets the handler to default and returns, and the second fault kills the task.
- Reading: a worker thread memcpy'd into a mapping that the leader had just munmapped during app teardown. The likeliest culprits are a shm or GPU buffer mapping, or a mallocng mmap'd chunk.
  Nothing in the logs points at the kernel: there was no exit_group race, and the VMA really was gone. A kernel munmap that removed the wrong range can't be ruled out from one sample, but there's nothing to support it.

## Reproduction (coldstart-bench, CS_STEPS=term, virgl, one LEANDROS_RUN_ID per run, under lock.sh qemu)
- x86_64 (TCG): 6 valid runs (1,2,3,7,8,9), 0 cosmic-term SEGV. Runs 4-6 were invalid (see below).
- aarch64 (HVF): 3 valid runs (4,5,6), 0 SEGV. The first 3 attempts failed to start because VNC :9 was held by the stale x86 run-4 QEMU.
- integ13 (714ee4fa) and the integ14 rerun were clean too: 1 in ~9 x86 runs overall.

## NEW, separate: virtio-blk timeout kills the root shell (x86_64 TCG, run 4)
`[VIRTIO-BLK] request type 0 sector 866008 timed out (device_status 0x0f); disk marked dead` (IO_TIMEOUT_NS = 30 s, drivers/src/virtio_blk.rs:31)
-> `[F2FS] device read failed: mount is now read-only`. After that, /bin/brush pid 14 took a read fault on a valid lazy file VMA (0x1000000-0x110e000 R, page=0x0)
-> page-in failed -> SEGV. Login reported "no shell prompt within 20s", and that run's QEMU was left running, which broke runs 5-6 (image lock) and the aarch64 attempts (VNC port).
Log: /tmp/ffsession/cs-termsegv-x86_64-4/serial-full.log. The host was loaded with other lanes' QEMUs at the time. Open questions: whether one 30 s stall is enough to mark the disk dead, and why
coldstart-bench doesn't kill its QEMU on a login failure.
