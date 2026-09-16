# ports/cosmic-session

**There is no LeandrOS patch to cosmic-session any more.** The shipped binary is
upstream [cosmic-session](https://github.com/pop-os/cosmic-epoch) at the
`epoch-1.3.0` submodule revision (`b5ef6c0`), built unmodified with its default
features. This directory keeps the build recipe and the history of the one patch
it used to carry, so nobody reintroduces it.

## The retired patch: `0001-env_rx-timeout-fallback.patch` (2026-07-25 → 2026-09-15)

`cosmic-session` blocks at `env_rx.await` (`src/main.rs`) until `cosmic-comp`
sends `SetEnv{WAYLAND_DISPLAY}` over the `COSMIC_SESSION_SOCK` UnixStream pair.
On LeandrOS that message never arrived, so no session child (panel, bg,
settings-daemon, notifications, …) was ever spawned. The patch raced the await
against a 5 s timeout and fell back to `WAYLAND_DISPLAY=wayland-1`; a second hunk
(`8d0bb66`) made a *late* `SetEnv` a no-op instead of an `unwrap()` panic on the
dropped receiver.

It was recorded as "a tokio-integration residual". That was never demonstrated
and was not true. Two kernel bugs, both fixed on our side, were the cause:

1. **`shutdown(2)` was a socket teardown** (`2d9f0c8`, `servers/net`).
   `handle_shutdown` ignored `how`; `shutdown(fd, SHUT_WR)` destroyed the caller's
   fd and flagged the whole end closed. tokio's `OwnedWriteHalf::drop` issues
   exactly that call when `comp.rs` does `session.into_split()` — before
   cosmic-comp is even spawned — so the session's read half was dead on arrival.
   `scmtest` gained the `*_shutdown_wr*` guards (32 → 35).
2. **Poll wakes were broadcast** (`daaf2cc`, `sched`/`vfs`). Every pipe write
   woke every parked poller in the system; cosmic-comp's unbuffered stderr
   through launch-pad's pipes turned that into a 40 s handshake on 4 vCPUs,
   which is why the 5 s fallback kept winning even after (1) — and why the real
   `SetEnv` then arrived late enough to hit the `unwrap()` that `8d0bb66` papered
   over.

### Retirement measurement (2026-09-15, pristine binary, both arches)

`Starting cosmic-session` → `got environmental variables from cosmic-comp`, read
from the session log (`brush /bin/start-cosmic-leandros > /data/x.log 2>&1 &`):

| arch / accel      | boot 1 (fresh image) | boots 2–5                      |
|-------------------|----------------------|--------------------------------|
| aarch64 / HVF     | 8.86 s               | 1.64 s, 1.75 s, 1.71 s, 1.65 s |
| x86_64 / TCG      | 8.36 s            | 8.35 s, 57.25 s, 7.96 s, 8.32 s                      |

`SetEnv` arrived on every boot; no panic; the desktop (panel, dock, wallpaper)
rendered on every boot. The one x86_64 outlier (57.25 s) shows no compositor exit
or restart in the session log — cosmic-comp was simply slow to say ready that
boot, and the session waited for it instead of racing a fallback. The first boot of a freshly generated image is slower
because cosmic-comp and cosmic-config do their first-run writes then.

What cosmic-comp actually exports is only `WAYLAND_DISPLAY=wayland-1` — the same
value the fallback hard-coded — so the patch's functional cost was never the
*contents* of the child environment, it was *ordering*: every child was spawned
at t+5 s regardless of whether the compositor was ready. Without the patch the
cascade starts when cosmic-comp says so.

### Rule

Do not bring this patch back under any new justification. If the handshake ever
stalls again, the surfaces are all ours and all guarded: `scmtest`
(`fork_exec_inherit*`, the `*shutdown_wr*` deciders), `smpwaketest` (pipe-EPOLLET
herd, forked-child stdout), and the timing read above.

## Build / restage

The build tree is `~/code/leandros-artifacts/m6-session-bins/src/cosmic-session`
and must stay byte-identical to `../cosmic-epoch/cosmic-session` (check with
`diff -r src ~/code/cosmic-epoch/cosmic-session/src`).

```sh
cd ~/code/leandros-artifacts/m6-session-bins
./build-rust.sh src/cosmic-session aarch64
./build-rust.sh src/cosmic-session x86_64
cp src/cosmic-session/target/aarch64-unknown-linux-musl/release/cosmic-session out/cosmic-session-aarch64
cp src/cosmic-session/target/x86_64-unknown-linux-musl/release/cosmic-session out/cosmic-session-x86_64
# then regenerate the f2fs images (scripts/mkfs-f2fs-populated.py)
```

## Kernel fixes this desktop bring-up depends on (in the main tree)

Each has a permanent regression test in `userland/scmtest`:

1. **`fcntl(F_SETFD/F_GETFD)` was a no-op for AF_UNIX socket fds**
   (`kernel/src/syscall.rs` + `servers/net` `NET_SETFD`/`NET_GETFD`). Clearing
   `FD_CLOEXEC` on an inherited `SOCK_CLOEXEC` socket before `execve` (launch_pad's
   `with_fds`, used for the notification sockets) did nothing, so the execve
   cloexec-sweep closed the socket → the child saw `EBADF`. Regression:
   `fork_exec_child_clears_cloexec`.
2. **`memfd_create` collided identically-named memfds onto one tmpfs inode**
   (`kernel/src/syscall.rs`). smithay-client-toolkit creates every `wl_shm`
   `SlotPool` with the one fixed name `"smithay-client-toolkit"` and seals it;
   the next same-name `memfd_create`'s `O_TRUNC` shrank the sealed inode →
   `EPERM`, panicking every winit/libcosmic client. Fixed by making each memfd a
   distinct inode (O_EXCL + monotonic suffix). Regression:
   `memfd_same_name_distinct`.
3. **The global pipe pool was too small** (`servers/vfs` `MAX_PIPES` 16 → 128).
   Every `command.spawn()` holds 3 stdio pipes; the full session's ~14
   components exhausted 16 pipes → `ENFILE` for every later component.
4. **`shutdown(2)` was a teardown, not a half-close** (`2d9f0c8`, above).
   Regression: `fork_exec_inherit_after_shutdown_wr`,
   `socketpair_shutdown_wr_half_close`, `shutdown_wr_keeps_fd_pollable`.
