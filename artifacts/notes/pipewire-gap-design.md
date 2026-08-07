# PipeWire gap — design + prototype (cosmic-settings-daemon, M6)

**Lane:** host-only, repo-read-only. Workdir `~/code/leandros-artifacts/pipewire-gap/`.
**Goal:** make `cosmic-settings-daemon` (fatal-at-spawn under cosmic-session) LINK and RUN
with its pipewire-dependent audio-device functionality gracefully inert, WITHOUT patching
any COSMIC source (Cargo feature flags / build-config only).

**Pins (from the daemon Cargo.lock):** `pipewire 0.10.0`, `pipewire-sys 0.10.0`,
`libspa 0.10.0`, `libspa-sys 0.10.0` (the pipewire-rs family).

---

## 1. Symbol surface (evidence)

Dependency chain (all non-optional, **no `[features]` gate anywhere** — re-confirmed):
`cosmic-settings-daemon` → `cosmic-settings-audio-server` → `cosmic-pipewire` →
`pipewire`/`pipewire-sys` + `libspa`/`libspa-sys`.

Link-time `links` keys / `DT_NEEDED`:
- `pipewire-sys`: `links = "pipewire-0.3"`  ⇒ `libpipewire-0.3.so.0`
- `libspa-sys`:   `links = "libspa-0.2"` + system-deps also lists `libpipewire-0.3`.
  **libspa-sys compiles its OWN C reexport archive** (`type-info.c`, `command.c`,
  `node-command.c`, `meta.c`, and bindgen `wrap_static_fns` → `static_fns.c`, suffix
  `_libspa_rs`). Upstream `libspa` is header-only; its real runtime symbols live in
  `libpipewire`.

**Actually-bound external symbol set** (enumerated by `llvm-nm` of the compiled
`pipewire-sys`/`libspa-sys`/`pipewire`/`libspa` rlibs — undefined `U` entries):

| class | count | disposition |
|-------|------:|-------------|
| `pw_*` functions | **63** | external → **must be provided by the stub** |
| `spa_*` (all `*_libspa_rs`) | 101 | **internal** — satisfied by libspa-sys's own reexport archive; must NOT be stubbed (would duplicate-define) |
| non-`_libspa_rs` `spa_*` | **0** | — |

So the **entire external pipewire/libspa surface is 63 `pw_*` functions in one library
(`libpipewire-0.3.so.0`)**. No separate `libspa-0.2.so` is required (its system-deps entry
is pointed at the same stub; upstream ships spa as header + plugins with an empty `Libs:`).

Full 63-symbol table with per-symbol runtime behavior: **§5**.

---

## 2. Runtime connect-failure behavior (decisive for shim depth)

Startup path (all verified in source):
- `cosmic-settings-audio-server::Context::run` (a tokio task spawned at daemon start) calls
  `cosmic_pipewire::run(on_event, on_sender)`.
- `cosmic_pipewire::run` **spawns a detached `std::thread`** running a retry loop around
  `run_service()`.
- `run_service()` first calls, in order:
  `MainLoopRc::new` → `pw_init()` then `pw_main_loop_new()`;
  `ContextRc::new` → `pw_context_new()`;
  `connect_rc` → `pw_context_connect()`;
  `get_registry_rc` → `pw_core_get_registry()`.
- **Every one of these maps a NULL C return to `pipewire::Error::CreationFailed`**
  (verified `error.rs` + `main_loop/*.rs` + `context/rc.rs`: `NonNull::new(raw).ok_or(Error::CreationFailed)?`).
- The retry loop (`cosmic-pipewire/src/lib.rs`):
  ```rust
  if let Err(why) = run_service(...) {
      if let pipewire::Error::CreationFailed = why {
          std::thread::sleep(Duration::from_secs(attempt.pow(2))); attempt += 1; continue; // forever
      }
      tracing::error!(?why, "failed to run pipewire thread");
  }
  break;
  ```

**Consequence:** a stub whose `pw_main_loop_new` returns NULL short-circuits at the earliest
point. `run_service` returns `CreationFailed`; the **detached thread** retries with quadratic
backoff (1 s, 4 s, 9 s, …) — benign, mostly sleeping, and **never touches the daemon's main
loop or any other subsystem**. Audio-device settings simply never populate.

**No pipewire struct is ever dereferenced on this path** (it fails before any pointer is
used) ⇒ **zero UB risk** from NULL-returning stubs, and the 61 downstream `pw_*` symbols are
linked-but-never-called. The shim needs **no state-machine fidelity at all** — the only two
functions reached at runtime are `pw_init` (no-op) and `pw_main_loop_new` (return NULL).

---

## 3. Option comparison

### (a) Stub shim `libpipewire-0.3.so.0` — **RECOMMENDED**
- 63 `pw_*` functions, each `return 0/NULL`. Enough because runtime reaches only
  `pw_init`+`pw_main_loop_new` and the wrapper turns NULL into `Error::CreationFailed`
  (§2). No `pw_main_loop_run` blocking concern — it is never called.
- Build still needs the real pipewire+spa **headers** (bindgen runs in both sys build.rs)
  and a linkable `.so`. Headers are compile-time only; the `.so` is faked.
- Rust-wrapper panic/UB risk: **none** — no NULL is dereferenced (fails at the first
  constructor). Verified against every constructor's error mapping.
- Cost: ~2.5 MB source tarball for headers (once), a generated ~63-line C file, one `.so`.
  **This is the libseat/libudev-shim precedent (D3) applied to a far smaller surface.**

### (b) Build REAL pipewire (meson/C) as a link-only lib
- Buys **nothing**: there is no pipewire daemon on LeandrOS, so `pw_context_connect` fails
  at runtime exactly as the stub does — same inert outcome.
- Cost is large: pipewire's meson build pulls libdbus, alsa, and a plugin tree; cross-musl
  with zig is a multi-day port. Rejected.

### (c) Cheaper alternatives
- **pipewire-rs feature to skip linking:** none — `links` keys are unconditional, no
  `dlopen`/optional-link feature exists in 0.10.
- **Cargo `[patch]`:** cannot help — the blocker is the `system-deps` pkg-config probe +
  `links`, not a version pin; `[patch]` swaps source, not the `links`/feature wiring.
- **system-deps env overrides** (`SYSTEM_DEPS_LIBPIPEWIRE_*`, `SYSTEM_DEPS_LIBSPA_*`): the
  right lever — build-config only, no source patch. **Used by the prototype** to point both
  libs at the stub + headers and to keep `PKG_CONFIG_SYSROOT_DIR` free for the daemon's real
  `libudev` probe. This is what makes option (a) a pure feature-flag/build-config solution.

**Verdict: (a), driven by system-deps env overrides.**

---

## 4. Prototype status (x86_64-musl)

Artifacts in `~/code/leandros-artifacts/pipewire-gap/`:
- `inc/pipewire-0.3/`, `inc/spa-0.2/` — real pipewire 1.2.7 headers + generated `version.h`
  (PW_MAJOR/MINOR/MICRO = 1/2/7 so all `PW_CHECK_VERSION(0,3,x)` guards are satisfied).
- `lib/x86_64/libpipewire-0.3.so.0` (+ `.so`) — the **63-symbol stub** (SONAME set).
- `lib/pw-symbols.txt`, `stub/stub-x86_64.c` — the symbol list + generated stub source.
- `cc/{x86_64,aarch64}-cc` — zig-cc wrappers that strip cc-rs's `--target=` (see landmine).
- `harness/` — a host-side crate that calls `cosmic_pipewire::run`, used to prove the stub
  is genuinely linked (test binaries GC the pipewire path away — see below).

**Pipewire closure proof (harness, strict link, x86_64):**
- `Type: DYN` + `PT_INTERP /lib/ld-musl-x86_64.so.1` ✓
- `DT_NEEDED = [libpipewire-0.3.so.0, libc.so]` ✓ (the stub is actually bound)
- 15 `pw_*` imports on the `run()` path, incl. `pw_init` + `pw_main_loop_new`; **closure
  CLOSED** (all resolved by the stub). The stub exports all 63, a superset of any daemon path.

**Full `cosmic-settings-daemon` link (x86_64): SUCCESS.**
- `Type: DYN` + `PT_INTERP /lib/ld-musl-x86_64.so.1` ✓
- `DT_NEEDED = [libpipewire-0.3.so.0 (stub), libudev.so.1 (m4-input-ship), libc.so (m3)]` ✓
- 15 `pw_*` imports bound to the stub (same set as the harness — the daemon's real
  `cosmic_pipewire::run` path); residual undefined `pw_`/`spa_` = 0 ✓
- **openssl is NOT `DT_NEEDED`** and residual undefined `SSL_/EVP_/X509_/CRYPTO_` = 0 —
  confirming the openssl stub was needed only to unblock `openssl-sys build.rs`, and reqwest
  is unreachable from the daemon (runtime-irrelevant, as reasoned).
- M6 `verify-closure.sh` reports every `NEEDED` resolved except `libpipewire-0.3.so.0`
  (which it doesn't know — it's our new stub); i.e. **the sole new runtime library the image
  must stage is `libpipewire-0.3.so.0`**. Binary: `out/cosmic-settings-daemon-x86_64`.

**Full `cosmic-settings-daemon` link (aarch64): SUCCESS** — identical result:
`Type: DYN` + `PT_INTERP /lib/ld-musl-aarch64.so.1`,
`DT_NEEDED = [libpipewire-0.3.so.0 (stub), libudev.so.1, libc.so]`, 15 `pw_*` imports,
0 residual `pw_`/`spa_`, openssl not `DT_NEEDED`. Binary: `out/cosmic-settings-daemon-aarch64`.
Stub: `lib/aarch64/libpipewire-0.3.so.0` (63 syms). **Both architectures complete.**

### Second, orthogonal gap found: openssl (build-time only)
Building the *whole* daemon additionally trips `openssl-sys` (the M6 build lane never reached
it — it stopped at the pipewire pkg-config probe). Chain: `geonames` (daemon workspace member,
non-optional path dep) → `reqwest 0.12` (default features → `default-tls`) → `native-tls` →
`openssl`/`openssl-sys`. **But `reqwest` is used only in `geonames/src/main.rs`** — the
auto-detected `geonames` *binary* (a build-time geodata-fetch tool). The daemon links the
geonames *library* (`GeoPosition` + `bitcode::decode` of an embedded blob), which references
**no reqwest**. So openssl is a **build-time-only** blocker (openssl-sys's `build.rs` runs for
the whole dependency graph and fails at pkg-config); the daemon binary does not reference
native-tls, so openssl is (expected) **not even `DT_NEEDED`** and is runtime-irrelevant.
Fixes: (i) same stub technique — a stub `libssl.so.3`+`libcrypto.so.3` + `OPENSSL_*_DIR` env
(**used by this prototype**, no source patch); or (ii) `geonames/Cargo.toml`
`reqwest { default-features = false, features = ["rustls-tls"] }` — a one-line **source patch**
that needs orchestrator approval. Both merely unblock `openssl-sys build.rs`. This gap is
**outside the pipewire task** and should be tracked separately by the M6 orchestrator.

---

## 5. Symbol table (63 `pw_*`, stub behavior)

All stubbed as `long f(void){return 0;}` (returns NULL/0/failure). Runtime-reached ones
marked ★ (the only two the daemon actually calls before short-circuiting):

- ★ `pw_init` — no-op; wrapper ignores return. Safe.
- ★ `pw_main_loop_new` — returns NULL ⇒ `MainLoopRc::new` → `Error::CreationFailed`. **This
  is the single symbol whose NULL return drives the entire graceful-inert path.**
- Never called at runtime (linked only; each returns NULL/0):
  `pw_main_loop_destroy`, `pw_main_loop_get_loop`, `pw_main_loop_quit`, `pw_main_loop_run`,
  `pw_loop_new`, `pw_loop_destroy`, `pw_context_new`, `pw_context_destroy`,
  `pw_context_connect`, `pw_context_connect_fd`, `pw_context_get_properties`,
  `pw_context_update_properties`, `pw_core_disconnect`, `pw_deinit`,
  `pw_properties_new`, `pw_properties_new_dict`, `pw_properties_copy`,
  `pw_properties_clear`, `pw_properties_free`, `pw_properties_get`,
  `pw_proxy_add_listener`, `pw_proxy_destroy`, `pw_proxy_get_id`, `pw_proxy_get_type`,
  `pw_client_info_free`, `pw_device_info_free`, `pw_factory_info_free`,
  `pw_link_info_free`, `pw_module_info_free`, `pw_node_info_free`, `pw_port_info_free`,
  `pw_stream_new`, `pw_stream_destroy`, `pw_stream_connect`, `pw_stream_disconnect`,
  `pw_stream_flush`, `pw_stream_dequeue_buffer`, `pw_stream_queue_buffer`,
  `pw_stream_get_name`, `pw_stream_get_node_id`, `pw_stream_get_properties`,
  `pw_stream_get_state`, `pw_stream_get_time`, `pw_stream_set_active`,
  `pw_stream_set_control`, `pw_stream_set_error`, `pw_stream_update_params`,
  `pw_thread_loop_new`, `pw_thread_loop_destroy`, `pw_thread_loop_start`,
  `pw_thread_loop_stop`, `pw_thread_loop_lock`, `pw_thread_loop_unlock`,
  `pw_thread_loop_wait`, `pw_thread_loop_timed_wait`, `pw_thread_loop_timed_wait_full`,
  `pw_thread_loop_signal`, `pw_thread_loop_accept`, `pw_thread_loop_get_loop`,
  `pw_thread_loop_get_time`, `pw_thread_loop_in_thread`.

(The `pw_stream_*` / `pw_thread_loop_*` families are referenced by the pipewire wrapper's
compiled code but are unreachable from `cosmic_pipewire`'s `run()` path; they exist purely to
close the link. Providing all 63 keeps the stub a superset for any future daemon path.)

---

## 6. On-target verification checklist (for the M6 wave)

The runtime behavior below is analyzed statically (this lane cannot run QEMU). The on-target
verifier should confirm:

1. **Boot/link sanity:** `cosmic-settings-daemon` loads (interpreter `/lib/ld-musl-<arch>.so.1`
   resolves), `libpipewire-0.3.so.0` (the stub) present on the image at the loader's search
   path; `ldd`/loader reports no missing `NEEDED`.
2. **Daemon survives pipewire absence:** it does **not** exit/abort at spawn under
   cosmic-session. Expect a detached worker thread; over time a quadratic-backoff cadence of
   retry attempts (no log spam beyond that — `CreationFailed` does not even hit the
   `tracing::error!` branch). The main daemon (brightness, battery, theme, wayland, time,
   a11y, config) must be fully responsive.
3. **Audio settings inert, not crashing:** `cosmic-settings` → Sound page shows no devices /
   empty state; toggling it must not crash the daemon (the audio Context frontend future keeps
   running; the pipewire backend future blocks on a Notify that never fires).
4. **No pipewire connect side effects:** no attempt should reach a real socket
   (`/run/pipewire-0`); `pw_context_connect` is never called (short-circuits at
   `pw_main_loop_new`).
5. **openssl (if the stub route is taken):** confirm `libssl.so.3`/`libcrypto.so.3` are
   **not** in the daemon's `DT_NEEDED` (expected — reqwest is not reachable from the daemon).
   If they are, the geonames library unexpectedly pulled a TLS path; investigate before
   trusting the NULL-returning openssl stub at runtime.
6. **Session milestone:** cosmic-session reaches its "session up" state with
   settings-daemon among the running children (the M6 exit criterion).

---

## 7. Reproduce
```
PG=~/code/leandros-artifacts/pipewire-gap
# headers: inc/ already laid out from pipewire-1.2.7 source (version.h generated)
# stub:
sh $PG/gen-stub.sh x86_64 $PG/lib/pw-symbols.txt          # -> lib/x86_64/libpipewire-0.3.so.0
# pipewire closure proof (harness that calls cosmic_pipewire::run):
sh $PG/gen-config.sh x86_64 $PG/harness
sh $PG/build-harness.sh x86_64 pw-harness
llvm-readelf -d $PG/harness/target/x86_64-unknown-linux-musl/release/pw-harness | grep NEEDED
# full daemon (adds openssl stub + OPENSSL_*_DIR env, see build-daemon.sh):
sh $PG/gen-config.sh x86_64 $PG/build/cosmic-settings-daemon
sh $PG/build-daemon.sh x86_64
```
Toolchain wiring mirrors the M6 lane (zig ld.lld against the m3 sysroot; `-crt-static` + `-pie`
⇒ ET_DYN + PT_INTERP). Key env: `SYSTEM_DEPS_LIB{PIPEWIRE,SPA}_*` (bypass pkg-config for our
libs), `BINDGEN_EXTRA_CLANG_ARGS=--target=<arch>-linux-musl --sysroot=<m3 sysroot>` (bindgen
parses the headers with correct musl types), `cc/<arch>-cc` (strip cc-rs `--target=`).

## 8. Landmines
- **cc-rs vs zig:** cc-rs injects `--target=x86_64-unknown-linux-musl`; `zig cc` rejects the
  4-field rust triple (`UnknownOperatingSystem`). The M6 CC wrapper never exercised cc-rs, so
  it doesn't strip it. `libspa-sys` is the first crate in this whole effort to compile C via
  cc-rs. Fixed with `cc/<arch>-cc` wrappers that drop `--target=`/`-target`/`-m64`.
- **Enumerate from rlibs, not the final binary:** `--unresolved-symbols=ignore-all` (used to
  produce an enumeration binary) makes ld.lld *drop* unprovided undefined symbols from
  `.dynsym`, so `nm -u` on the output shows zero — enumerate undefined `pw_`/`spa_` from the
  dependency **rlibs** instead.
- **Test binaries GC the pipewire path:** `cargo build -p cosmic-pipewire --tests` links but
  no test calls `run()`, so `--gc-sections` removes every `pw_*` reference and
  `libpipewire-0.3.so.0` is not even `DT_NEEDED`. Prove linkage with a harness that actually
  calls `cosmic_pipewire::run`.
- **`*_libspa_rs` are internal:** the 101 `spa_*` undefined symbols are libspa-sys's own
  reexport wrappers; stubbing them causes duplicate-definition. Stub `pw_*` only.
