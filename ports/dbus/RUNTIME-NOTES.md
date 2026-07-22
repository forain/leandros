# S5 busd runtime roundtrip + session packaging — RESULT

Job: L3 (Wayland/COSMIC round 2 parallel lanes) — busd runtime roundtrip
(deferred S5 tail) + session packaging for M5/M6. Repo stayed read-only
throughout (no git writes, no OS builds, no QEMU — those are owned by
other agents/lanes).

## Task 1 — container availability + roundtrip: DONE, PASSED both arches

Checked for container runtimes: `docker` present (`/usr/local/bin/docker`,
Docker Desktop 29.3.0), `podman`/`colima`/`lima`/`container` all absent.
Docker daemon was NOT running at task start (`docker info` failed:
"Cannot connect to the Docker daemon"). I started Docker Desktop
(`open -a Docker`) — this is a pre-installed app, not a new install — and
it came up within one ~3s poll interval.

Host is Apple Silicon (`arm64`), Docker's Linux VM is native `aarch64`
(linuxkit 6.12.76). Ran the roundtrip on **both** arches:
- `--platform linux/arm64` — native, no emulation.
- `--platform linux/amd64` — via Docker Desktop's emulation; also passed
  cleanly, so both of the job's existing static busd builds
  (`busd/target/{x86_64,aarch64}-unknown-linux-musl/release/busd`, from
  the earlier build probe, `EXIT:0` both arches) are runtime-verified,
  not just build-verified.

Built a new crate `zbus-test-client/` (not present before this session)
using the proven musl recipe from `project_musl_toolchain.md`: `cargo
+nightly`, target `{x86_64,aarch64}-unknown-linux-musl`, `linker =
"rust-lld"`, `RUSTFLAGS`/`.cargo/config.toml` rustflags `-C
relocation-model=static`. Both targets built clean on the first try
(`file` confirms "statically linked, stripped" ELF, matching busd's own
binaries). Depends on the same zbus git fork busd uses
(`https://github.com/z-galaxy/zbus`, `features = ["tokio"],
default-features = false` — no `bus-impl`/`p2p`, since this is a plain
client, not a broker); Cargo pulled the fork's current HEAD
(`5cb328f0`, zbus 5.18.0) rather than busd's pinned `c127e4d8` (zbus
5.14.0) since this is an independent crate/lockfile — harmless, D-Bus is
a stable wire protocol and the zbus client API used here (`Builder`,
`#[zbus::interface]`, `call_method`) is unchanged between those two
points.

`zbus-test-client/src/main.rs` does exactly the M5 exit-criterion shape
("busd running; zbus client owns a name") plus one step further (an
actual broker-routed method call):
1. Connection A → `Builder::address(...).serve_at(...).name(...).build()`
   — connects, Hello-handshakes (implicit in zbus's `build()`), owns
   `org.leandros.Test`, serves `Ping` at `/org/leandros/Test`.
2. Connection B → independent second connection to the same address.
3. Asserts A and B got distinct unique names (`:busd.1` / `:busd.2` in
   the actual run — proves they're not accidentally the same peer).
4. B calls `Ping` on A **by well-known name**, i.e. routed through busd,
   not a direct pipe — this is the real proof busd's broker/routing
   logic works, not just its listener/accept path.

**Result, both arches, full log:**
```
=== busd --version ===
busd 0.5.0

=== dbus-run-session -- zbus-test-client ===
[zbus-test-client] connecting to `unix:path=/run/user/0/bus` ...
[zbus-test-client] connection A up: unique name = :busd.1, owns `org.leandros.Test`
[zbus-test-client] connection B up: unique name = :busd.2
[zbus-test-client] reply body = "pong:hello-from-b"
ROUNDTRIP_OK
=== dbus-run-session exit status: 0 ===
OK: no busd process left running
OK: socket file cleaned up
```
S5's deferred runtime tail is now closed. M0 exit criteria (D4 confirmed)
now has an actual execution proof, not just a clean build.

Test harness: `run-container-test.sh` (job tmp, not a shipped artifact —
it stages binaries into canonical paths like `/usr/libexec/busd` inside
the *container's* throwaway rootfs and drives the real launcher). Repro:
```sh
docker run --rm --platform linux/arm64 \
  -v <job-tmp>/s5-busd-probe:/work:ro \
  alpine:latest sh -c '
    apk add --no-cache --quiet procps;
    cp /work/run-container-test.sh /tmp/rt.sh; chmod +x /tmp/rt.sh;
    /tmp/rt.sh aarch64-unknown-linux-musl'
```
(swap `linux/arm64`/`aarch64-unknown-linux-musl` for the amd64 pair to
run the other arch.)

## Task 2 — session packaging: DONE, tested end-to-end (Task 1 succeeded)

Artifacts land in `session-pkg/` (these ARE meant to ship, unlike
`run-container-test.sh`):

### `session-pkg/session.conf`
Minimal busconfig busd's `Config::read_file` (config/mod.rs) actually
parses: `<type>session</type>`, `<auth>EXTERNAL</auth>`, a default
`<listen>unix:path=/run/user/0/bus</listen>`. Deliberately omits
`<policy>` (busd is alpha, no groups/users story we can rely on, and the
brief said ship nothing we don't have — everything is allowed by default
absent a policy) and all activation/servicedir/daemonize/user-drop
elements (no `.service` activation, no forking, single-user root
bring-up). Full rationale is in the file's own header comment, including
the one non-obvious fact I verified by reading `bus/mod.rs`: for a UNIX
listener busd **hardcodes** `AuthMechanism::External` regardless of
`Config::auth` (auth_mechanism is derived from the transport kind in
`Bus::for_address`, config's `<auth>` is parsed but never threaded
through) — so `<auth>EXTERNAL</auth>` here is correct documentation of
intent, not load-bearing today.

### `session-pkg/dbus-run-session`
POSIX-sh launcher (`sh -n` clean). Behavior: start busd with
`--config session.conf --address unix:path=$RUN_DIR/bus --ready-fd 3`,
wait for readiness, export `DBUS_SESSION_BUS_ADDRESS`
(`unix:path=$RUN_DIR/bus`) and `DBUS_SESSION_BUS_PID`, run the given
command as a child (not `exec` — see below), kill busd + clean up the
socket/ready-file via an `EXIT INT TERM` trap, propagate the child's
exit status.

**Important deviation from the "obvious" design, found by reading
LeandrOS source before writing this, not by trial and error:** the usual
`--ready-fd 3 3>fifo` + blocking `read` idiom (what systemd/s6 and most
dbus-launch-alikes use) does **not** work on LeandrOS:
- No `mkfifo` userland binary exists (`userland/` has no coreutils-style
  FIFO tool at all — the whole userland is a short list of
  purpose-built test/utility binaries, brush the shell, and not much
  else).
- Even `mknod(path, S_IFIFO)` doesn't help: LeandrOS's tmpfs
  `handle_mknod` (`servers/vfs/src/lib.rs`) only fabricates a dirent that
  *reports* as `S_IFIFO`/`DT_FIFO` to `stat`/`getdents64` — there's no
  real blocking open()/read() rendezvous behind it, per that function's
  own scope-note comment. A `read` against it would return immediate EOF
  instead of blocking, races every time.
- There's also no `sleep` binary and no shell `sleep`/`coproc` builtin
  (checked brush's builtin registry, `brush-builtins/src/factory.rs` —
  the only timing-adjacent thing is `read -t`, which only works when
  stdin is attached to something that won't hit EOF immediately, not
  guaranteed for a script invoked non-interactively from another script).

**Fix shipped:** `--ready-fd 3` still points at a real regular tmpfs
file (`3>"$READY_FILE"`, plain POSIX numbered-fd redirection, no bashism)
instead of a FIFO. Regular-file writes don't need blocking rendezvous to
be observed, so the script busy-polls `test -s "$READY_FILE"` in a loop
bounded by an iteration count (`MAX_POLL_ITERS`, overridable via
`DBUS_RUN_SESSION_MAX_POLL_ITERS`) rather than wall-clock time, checking
`kill -0 "$BUSD_PID"` each iteration to fail fast if busd has already
died. CPU-noisy for the sub-second startup window, correct everywhere.
This is documented at length in the script's own header comment so a
future editor doesn't "simplify" it back into the broken FIFO idiom.

Does **not** `exec "$@"` for the child — on purpose: an `exec` would
replace the script's own process image, and the `EXIT` trap (which kills
busd) would never fire, orphaning busd. Runs the child as a normal
foreground job and lets the trap handle cleanup after it returns.

### Task 2, item 3 — tested for real
Since Task 1's container worked, I didn't stop at "these files look
right" — `run-container-test.sh` stages exactly these two files (plus
the busd/zbus-test-client binaries) into an Alpine container's real
filesystem paths and runs `dbus-run-session -- zbus-test-client`
unmodified, under busybox ash (a stricter POSIX `/bin/sh` than brush, if
anything — good extra portability signal). **Passed clean on both
arches**, log above. Post-run checks also confirm the cleanup trap
actually works: no leftover `busd` process, no leftover socket file.

## Bottom line for the orchestrator
- Container path: **available and used** (Docker Desktop, daemon started
  by me this session — was not running at task start).
- Runtime roundtrip: **PASSED**, both arches, busd 0.5.0 + zbus 5.18.0
  client — Hello, name ownership, broker-routed method call, all real.
- `session-pkg/session.conf` and `session-pkg/dbus-run-session`: ready to
  copy into the LeandrOS image at `/usr/share/dbus-1/session.conf` and
  (e.g.) `/usr/bin/dbus-run-session` respectively for M5/M6 — validated
  end-to-end in a container standing in for the real target layout.
- New one-time finding worth folding into the plan doc / musl-toolchain
  memory: LeandrOS's `mknod(S_IFIFO)` is stat-only (no blocking
  semantics) and there's no `mkfifo`/`sleep` in userland or brush's
  builtins — any future script relying on a blocking-FIFO or sleep-based
  wait idiom needs the same busy-poll-on-a-regular-file workaround used
  here.
