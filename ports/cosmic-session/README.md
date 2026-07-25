# ports/cosmic-session

LeandrOS-local patch(es) to [cosmic-session](https://github.com/pop-os/cosmic-epoch)
for the COSMIC desktop bring-up.

## 0001-env_rx-timeout-fallback.patch

A startup-rendezvous workaround for the `cosmic-session` ↔ `cosmic-comp`
readiness handshake. `cosmic-session` blocks at `env_rx.await` waiting for
`cosmic-comp` to send `SetEnv{WAYLAND_DISPLAY}` over the `COSMIC_SESSION_SOCK`
UnixStream pair; under LeandrOS's tokio async-read integration that oneshot
never resolves, so no session component is ever spawned. The patch races the
await against a 5 s timeout and falls back to `WAYLAND_DISPLAY=wayland-1` (the
socket cosmic-comp actually creates at `/run/user/0/wayland-1`).

This is **not** a functional COSMIC change — it is a bring-up rendezvous
workaround for a known-hard tokio-integration gap. The kernel's socket / fork /
execve / fd-inherit / epoll-wake path is independently verified sound by
`userland/scmtest`'s `fork_exec_inherit` and `fork_exec_child_clears_cloexec`
deciders (both pass on both arches).

### Build / restage

The build tree is `~/code/leandros-artifacts/m6-session-bins/src/cosmic-session`
(byte-identical to upstream). Apply the patch there, then:

```sh
cd ~/code/leandros-artifacts/m6-session-bins
./build-rust.sh src/cosmic-session aarch64
./build-rust.sh src/cosmic-session x86_64
cp src/cosmic-session/target/aarch64-unknown-linux-musl/release/cosmic-session out/cosmic-session-aarch64
cp src/cosmic-session/target/x86_64-unknown-linux-musl/release/cosmic-session out/cosmic-session-x86_64
# then regenerate the f2fs images (scripts/mkfs-f2fs-populated.py)
```

## Kernel fixes this desktop bring-up depends on (in the main tree)

Three real kernel bugs were found and fixed while bringing the session up — each
has a permanent regression test in `userland/scmtest`:

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

## Residual (not yet resolved)

With all of the above, `cosmic-comp` composites and `cosmic-bg` renders the
wallpaper, and `cosmic-panel` connects, binds globals, creates its output and
spawns all 16 applets, reaching "Waiting for configure event" — but then exits
with code 101 (a silent exit, no panic message even at `RUST_BACKTRACE=full`)
and launch_pad restart-loops it. Root cause not yet isolated; needs the full
cosmic-session context (notification-fd handoff + workspaces D-Bus service) to
reproduce. This is the remaining blocker to a panel-bearing desktop.
