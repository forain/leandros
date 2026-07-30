# cosmic-greeter port (LeandrOS)

cosmic-greeter's binary dispatches on the invoking username: a non-`cosmic-greeter`
user runs `locker::main` (the lock screen). cosmic-session spawns it in-session
with infinite restart + exponential backoff. LeandrOS has no systemd, no logind,
no PAM stack and no greetd, so the greeter needs two adjustments to be buildable
and safe to run in-session.

## 1. Built with `--no-default-features`

The default `logind` feature makes the locker subscribe to `org.freedesktop.login1`
on the zbus **system** bus. That service does not exist on LeandrOS, and the
failed subscription does `std::process::exit(1)` → cosmic-session restarts it →
infinite crash-loop. Building without default features removes the logind
subscription (and its zbus system-bus dependency).

## 2. `0001-locker-idle-without-logind.patch`

Disabling logind exposes the other half of the problem: upstream's **non-logind**
startup arm in `locker::init()` locks the screen *immediately* at process start,
and the non-logind `SessionLockEvent::Unlocked` handler calls `process::exit(0)`.
Under cosmic-session's restart supervision the loop becomes
`start → lock → unlock → exit(0) → restart → lock → …` — the desktop can never
stay unlocked.

The patch changes ONLY that startup arm to mirror the logind arm: lock at startup
**only** when recovering a previously-locked session (the lock file exists),
otherwise `Task::none()` (idle). LeandrOS has nothing that triggers a lock at
boot, so the locker starts idle and stays out of the way. This is the only `.rs`
change made to cosmic-greeter.

Residual behavior: the non-logind `Unlocked` handler still `process::exit(0)`s,
so an actual unlock exits the process; cosmic-session restarts it and it comes
back **idle** (no lock file) rather than re-locking. That is the intended,
startup-safe outcome.

## PAM

pam-client → pam-sys is an unconditional dependency and links `libpam` at build
time. LeandrOS ships a small **libpam shim** (soname `libpam.so.0`) instead of a
real PAM stack; its `pam_authenticate` drives the conversation callback for the
password and verifies it against `/etc/shadow` using the same `$sha256$salt$hex`
scheme as `/bin/login`. Everything else is a benign `PAM_SUCCESS` stub. The shim
source and its bindgen headers live with the build tree at
`~/code/leandros-artifacts/m6-session-bins/src/libpam-shim/` and install into the
m3 sysroot (mirroring the libseat/libudev shims).

## Build

```sh
D=~/code/leandros-artifacts/m6-session-bins
# 1. shim (once per arch) — installs libpam.so.0 + security/*.h into the sysroot
sh $D/src/libpam-shim/build-shim.sh aarch64
sh $D/src/libpam-shim/build-shim.sh x86_64
# 2. vendored greeter source + cargo config
rsync -a --exclude .git ~/code/cosmic-epoch/cosmic-greeter/ $D/src/cosmic-greeter/
sh $D/gen-cargo-config.sh src/cosmic-greeter
patch -p1 -d $D/src/cosmic-greeter < ports/cosmic-greeter/0001-locker-idle-without-logind.patch
# 3. cross-build the root binary (bindgen needs the sysroot PAM headers; vergen
#    needs git vars because .git is excluded from the vendored tree)
export BINDGEN_EXTRA_CLANG_ARGS="--target=aarch64-unknown-linux-musl --sysroot=$D/../m3-gl-stack/sysroot-aarch64 -I$D/../m3-gl-stack/sysroot-aarch64/usr/include"
export VERGEN_GIT_SHA=leandros VERGEN_GIT_COMMIT_DATE=2026-07-26
sh $D/build-rust.sh src/cosmic-greeter aarch64 --no-default-features
```
