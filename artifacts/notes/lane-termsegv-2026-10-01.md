# Lane termsegv — 2026-10-01

Branch `lane/termsegv` @ `73b13f8` (base origin/main `cbe9947`).

## Verdict: PARTIAL
The kernel bug is fixed. A residual use-after-free remains in the application, and Linux has it too.

## What SEGVs on close
- **Who.** iced_winit's SCTK event-loop thread in cosmic-term, the calloop/`polling` thread with the Wayland fd 512 registered. It is always tgid+5.
- **What.** A write to NULL+0x254/0x25c at `wl_map_insert_at` (libwayland-client vaddr 0xe0c5, `movq %rcx,(%rax,%rdx,8)`, entries data == NULL).
- **When.** The main thread is in teardown at the time (`leader_last` close / munmap / exit_group): it has disconnected and freed the `wl_display` that this thread still dispatches through.
- **Variant.** Sometimes the fault comes first as a kernel-mode EFAULT on a just-munmapped user buffer.

## The kernel's part
Epoll interests are keyed by fd number, and close never removed them.
- On every pass, `epoll_wait` probed the closed number, got EBADF and reported **POLLNVAL**. Linux removes the item when the file is released and has no EPOLLNVAL.
- So the main thread's close of the wl socket *woke* the parked SCTK thread straight into the freed display.
- Diagnostic build v3: `[EPNVAL] pid=793 fd=512` was printed 10 ms before `[PF] SEGV pid=793`. There were 7 EPNVAL wakes in 24 closes.

## Fix (`kernel/src/syscall.rs`, `servers/vfs/src/lib.rs`)
- `close`, `dup2`/`dup3` and `close_range` release the fd's registrations **before** the VFS frees the number.
  - The first version did this after the close. That raced with number reuse: a sibling thread got the number for a new eventfd, registered it, and had it deleted.
  - Measured result: the mio waker of notify was lost, and cosmic-term's exit hung in tokio `BlockingPool::shutdown` (2–3 terminals per run in v4–v9).
- If another fd in the same process still names the same object (a dup), the registration moves to it (`vfs::find_alias_fd`).
- `epoll_wait` and the readiness re-probe skip POLLNVAL instead of reporting it, and do not remove the registration.
- `[PF]` refusal line now reads `SEGV pid tgid exe addr R/W pc (user|kernel) VMA/pc_in leader_last exit_owner`.
- Gated Ctrl-T dumps (both off by default): `EPOLL_DUMP_INTERESTS`, and `DUMP_X86_RET` for cosmic-term return addresses.
- New test: polltest `epoll_close_drops_registration` (close, number reuse, dup2, live registration).

## Evidence (24 open / 24 close, 4 GiB)
"SEGV terms" counts terminals with at least one SEGV.

| run | SEGV terms | hung exits |
|---|---|---|
| x86_64 KVM virgl, pre-fix (term20 image + 3 diagnostic builds) | 8/96 | 0 |
| x86_64 KVM virgl, final f1 + f3 | 2/48 | 0 |
| x86_64 KVM **Venus**, final | 3/24 | 0 |
| aarch64 HVF virgl, pre-fix (old main image, 16 opened) | 4/16 | 0 |
| aarch64 HVF virgl, final | **0/24** | 0 |

Earlier term20 baseline: 9 PF lines in 24 closes.

- **Census (x86 virgl final).**
  - comp fds: 97/ev41 → 291/ev114 → 95/ev40
  - `ev_fence`: 21 → 70 → 20
  - dmabuf: 17 → 66 → 16
  - DRMH `blob_objs`: 99 → 1661 → 97
  - fdtables: 30 → 78 → 30
- **Census (Venus).** comp 88 → 280 → 88, `blob_objs` 83 → 415 → 83.
- So the weak_window fix holds on KVM, with no residue.
- **Suites (release, no GPU).** polltest 8/8, vfstest 46, drmsmoke `failed=0`, epolltest 11/11, wakepolltest 45/0, on both x86_64 KVM and aarch64 HVF.

## Residual (application, not kernel)
- iced's SCTK thread holds a *foreign* `wl_display`, which winit disconnects on exit while the thread is still awake.
- The thread is woken by real compositor traffic or iced's own shutdown ping.
- musl mallocng unmaps the freed chunk, so Linux with musl would SEGV the same way. glibc would usually hide it.
- The fix belongs in iced or COSMIC (stop and join the SCTK loop before dropping winit), and per the user rule COSMIC sources were not patched.

## Open
- `close_range` closes VFS fds only and ignores `CLOSE_RANGE_CLOEXEC`. This is pre-existing and was not changed here.
- Epoll is still keyed by number. A file kept alive only by another process (a fork child) loses its registration on close.
- Shared files touched: `kernel/src/syscall.rs` (epoll and close paths), `servers/vfs`, `sched/src/lib.rs` (fault report and dump), the arch fault handlers (`note_fault_pc`), and `userland/polltest`.
- Worktrees are removed on the Mac and the desktop. The desktop runs used tmpfs `/tmp/termsegv` because the samsung disk is full; it has been deleted.
