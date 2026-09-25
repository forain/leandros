# cosmic-greeter port (LeandrOS)

**One source patch, `0001-blur-opt-in-under-softpipe.patch` (2026-09-25), and
it is a performance decision, not a functional one** — see "3. Background blur"
below. The older `0001-locker-idle-without-logind.patch` stays retired by a
staging decision — see "Why there is no patch" below. Apart from that, this is
the build recipe and the two facts a rebuild has to honour.

## 1. Built with `--no-default-features`

The default `logind` feature makes the lock screen subscribe to
`org.freedesktop.login1` on the zbus **system** bus. That service does not exist
on LeandrOS, and the failed subscription does `std::process::exit(1)`. Building
without default features removes the subscription and its zbus system-bus
dependency. This is a build-configuration flag, which the "run COSMIC unmodified"
rule allows.

## 2. Why there is no patch

The binary picks its role purely from the invoking username: `main.rs` matches
`pwd::Passwd::current_user()` against the literal `"cosmic-greeter"` and runs
`greeter::main()` for that name, `locker::main()` for every other. The patch
existed only because `cosmic-session` spawns the greeter in-session
unconditionally, where it necessarily takes the **locker** arm; upstream's
non-logind startup arm locks immediately, its `Unlocked` handler
`process::exit(0)`s, and under cosmic-session's restart supervision that becomes
`start → lock → unlock → exit(0) → restart → lock → …`.

Nothing in that chain is about the binary's contents. It is about being reachable
under the name `cosmic-session` spawns. So the binary is staged as
**`/bin/cosmic-greeter-login`**, and `start_component("cosmic-greeter")` simply
finds nothing: `launch_pad`'s `ProcessManager::start` propagates the
`Command::spawn` error before it spawns the supervising `process_loop`, so the
failure costs exactly one error line per boot and cannot restart-storm. The lock
screen is not started at all, which is both the intent of the patch and one fewer
process in the session.

The greeter role is reached the other way round, by the login path actually
running as an account named `cosmic-greeter` (`userland/greeter-launch` drops to
it before `execve`).

Consequences worth stating so they are not rediscovered:

- **`libpam.so.0` stays staged.** It is `DT_NEEDED` by this ELF whether or not
  the lock-screen code path ever executes, so the dynamic loader still has to
  resolve it. Unstaging it turns every greeter launch into a load failure.
- Anything that wants the lock screen back must both restore a
  `/bin/cosmic-greeter` name and deal with the immediate-lock loop again.

## 3. Background blur is opt-in (`0001-blur-opt-in-under-softpipe.patch`)

`Common::blur_rects` asks the compositor (ext-background-effect) to blur what is
behind the login card. cosmic-comp implements that as a multi-pass
downsample/upsample shader over the card region, redone on every frame, and on
LeandrOS the compositor renders with Mesa **softpipe** (an interpreted TGSI
rasterizer). Measured on x86_64/KVM (lane greeterlag, 2026-09-25): cosmic-comp's
render thread sat in softpipe (`fetch_source`, `img_filter_2d_linear`,
`exec_instruction`, ...) ~80 % of a CPU with the greeter idle, one frame took ~4 s,
and every keystroke waited for the next frame. With blur off and nothing else
changed, per-key latency went from p50 10.8 s to 0.16 s.

The patch returns early unless `/etc/leandros/greeter-blur` exists, so blur can be
turned back on without a rebuild (`touch /etc/leandros/greeter-blur`, restart the
greeter) once the compositor has a GPU renderer (virgl/Zink/v3d). Rebuild:
`cd ~/code/leandros-artifacts/m6-session-bins/src/cosmic-greeter && patch -p1 <
<this dir>/0001-blur-opt-in-under-softpipe.patch && sh ../../build-greeter.sh <arch>`
then copy `target/<arch>-unknown-linux-musl/release/cosmic-greeter` to
`../../out/cosmic-greeter-<arch>` (unpatched originals kept as
`out/cosmic-greeter-<arch>.orig-pre-noblur`).

## PAM

pam-client → pam-sys is an unconditional dependency and links `libpam` at build
time. LeandrOS ships a small **libpam shim** (soname `libpam.so.0`) instead of a
real PAM stack; its `pam_authenticate` drives the conversation callback for the
password and verifies it against `/etc/shadow` using the same `$sha256$salt$hex`
scheme as `/bin/login`. Everything else is a benign `PAM_SUCCESS` stub. The shim
source and its bindgen headers live with the build tree at
`~/code/leandros-artifacts/m6-session-bins/src/libpam-shim/` and install into the
m3 sysroot (mirroring the libseat/libudev shims).

In the **login** role none of that is used: `greeter.rs` contains no PAM calls at
all and authentication crosses greetd IPC, which authenticates through
`ports/greetd/pam-leandros`. The shim matters only as a link-time and load-time
dependency.

## Build

```sh
D=~/code/leandros-artifacts/m6-session-bins
# 1. shim (once per arch) — installs libpam.so.0 + security/*.h into the sysroot
sh $D/src/libpam-shim/build-shim.sh aarch64
sh $D/src/libpam-shim/build-shim.sh x86_64
# 2. vendored greeter source + cargo config — NO patch step any more
rsync -a --exclude .git ~/code/cosmic-epoch/cosmic-greeter/ $D/src/cosmic-greeter/
sh $D/gen-cargo-config.sh src/cosmic-greeter
# 3. cross-build the root binary (bindgen needs the sysroot PAM headers; vergen
#    needs git vars because .git is excluded from the vendored tree)
export BINDGEN_EXTRA_CLANG_ARGS="--target=aarch64-unknown-linux-musl --sysroot=$D/../m3-gl-stack/sysroot-aarch64 -I$D/../m3-gl-stack/sysroot-aarch64/usr/include"
export VERGEN_GIT_SHA=leandros VERGEN_GIT_COMMIT_DATE=2026-07-26
sh $D/build-rust.sh src/cosmic-greeter aarch64 --no-default-features
```

The vendored tree at `$D/src/cosmic-greeter` may still carry the old patch from a
previous build. Re-run the `rsync` above to restore it byte-identical to
`~/code/cosmic-epoch/cosmic-greeter` before rebuilding, and confirm with
`git -C ~/code/cosmic-epoch status --porcelain` that the source tree itself is
clean.
