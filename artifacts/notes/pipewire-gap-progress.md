# pipewire-gap progress

## Task
Make cosmic-settings-daemon LINK+RUN with pipewire functionality gracefully inert.
Host-only, repo-read-only. Workdir ~/code/leandros-artifacts/pipewire-gap/.

## Steps
- [x] Read M6 manifest + toolchain scripts
- [ ] Scope symbol surface (pipewire-sys/libspa-sys imports)
- [ ] Read audio-server + cosmic-pipewire connect-failure behavior
- [ ] Evaluate options a/b/c
- [ ] Prototype recommended option (x86_64 min)
- [ ] Link cosmic-settings-daemon against it
- [ ] Write pipewire-gap-design.md

## Log
- Started. M6 lane confirms: chain is cosmic-settings-daemon -> audio-server (cosmic-settings-audio-server) -> cosmic-pipewire -> pipewire-sys/libspa-sys build.rs pkg-config probe. No feature gate anywhere.
- Pins: pipewire 0.10.0, pipewire-sys 0.10.0, libspa 0.10.0, libspa-sys 0.10.0 (pipewire-rs). Sources cached in ~/.cargo/registry.
- LINK surface: pipewire-sys `links=pipewire-0.3` (DT_NEEDED libpipewire-0.3.so.0). libspa-sys `links=libspa-0.2` + system-deps also lists libpipewire-0.3. libspa-sys compiles its OWN C reexports (type-info.c/command.c/node-command.c/meta.c/static_fns.c) so most spa_* are internal.
- Both sys build.rs run bindgen (needs real pipewire+spa HEADERS + libclang) and system-deps pkg-config probe (needs .pc files). => stub approach still needs real headers + .pc, only the .so is faked.
- RUNTIME (decisive): cosmic_pipewire::run spawns a DETACHED std::thread. First sys call = pw_init (no-op ok) then pw_main_loop_new. Every constructor maps NULL->Error::CreationFailed (verified error.rs + main_loop/context rc.rs). run() loop: on CreationFailed sleeps attempt^2 s and retries FOREVER (quadratic backoff, benign detached thread). Daemon main + all other subsystems unaffected. Audio just never populates. NO struct ever dereferenced => zero UB from a NULL-returning stub.
- VERDICT forming: Option (a) stub shim clearly wins. Option (b) real pipewire buys nothing (no daemon to connect to => connect fails anyway) at huge build cost (meson+dbus+plugins).
- Fetched pipewire 1.2.7 source (headers only). Laid out inc/pipewire-0.3/pipewire (47 hdrs + extensions), inc/spa-0.2/spa (148 hdrs), generated version.h (PW_MAJOR=1 MINOR=2 MICRO=7, API "0.3" => PW_CHECK_VERSION(0,3,x) all true).
- Integration decision: use system-deps ENV OVERRIDES (SYSTEM_DEPS_LIBPIPEWIRE_*, SYSTEM_DEPS_LIBSPA_*) NOT .pc, so PKG_CONFIG_SYSROOT_DIR stays pointed at m3 sysroot for the daemon's libudev probe without mangling host paths. libspa header-only => its LIB points at the SAME libpipewire-0.3 stub (real spa runtime syms live in libpipewire upstream anyway). bindgen pointed at musl sysroot via BINDGEN_EXTRA_CLANG_ARGS for correct linux parsing.
- Built EMPTY x86_64 stub libpipewire-0.3.so.0 (+ .so symlink) for enumeration pass.
- Copied daemon src to pipewire-gap/build/cosmic-settings-daemon. gen-config.sh + build-daemon.sh written.
- PASS 1 (enumeration): launched daemon build (bg bpshswdkh) with --unresolved-symbols=ignore-all + --error-limit=0 => will produce a binary with pw_*/spa_* left undefined; nm -u it to get the exact stub symbol set.
- SECOND GAP FOUND (orthogonal to pipewire): daemon full build hits openssl-sys. Chain: geonames (daemon workspace member, `geonames = {path="geonames"}`, non-optional) -> reqwest 0.12 (default features => default-tls) -> hyper-tls/tokio-native-tls -> native-tls -> openssl/openssl-sys. m3 sysroot has no openssl.pc; no OPENSSL_*_DIR set => build fails. Fix is rustls (geonames Cargo.toml one-liner: reqwest default-features=false, features=["rustls-tls",...]) — a SOURCE PATCH needing orchestrator approval, OR an openssl stub (same technique). NOT the pipewire gap. To prove pipewire closure without entanglement, link cosmic-pipewire (sole pipewire consumer; no openssl in its closure) as a --tests ELF against the stub — its pipewire symbol set == the daemon's.
- CC LANDMINE: cc-rs (compiling libspa-sys reexports type-info.c/command.c/node-command.c/meta.c/static_fns.c) injects --target=x86_64-unknown-linux-musl which `zig cc` rejects ("UnknownOperatingSystem" — zig wants x86_64-linux-musl, 3-field). M6's CC wrapper never exercised cc-rs so didn't strip it. Wrote pipewire-gap/cc/{x86_64,aarch64}-cc that drop --target=/-target/-m64 and force zig -target. bindgen (via BINDGEN_EXTRA_CLANG_ARGS musl sysroot) parsed all headers fine BEFORE this failure => header layout approach validated.
- PASS 1 relaunched (bg b10uq32k9) with fixed CC.
- ENUMERATED via rlib nm (ignore-all drops UND from final dynsym so enumerate from rlibs): 164 undefined pw_/spa_, of which ALL 101 spa_ are *_libspa_rs (libspa-sys's own compiled reexport archive — must NOT be stubbed, would dup-def). Real external surface = 63 pw_* functions. Zero non-_libspa_rs spa_ symbols. => stub = 63 pw_ funcs only.
- Generated stub/stub-x86_64.c (63 funcs return 0/NULL) -> lib/x86_64/libpipewire-0.3.so.0 (SONAME set, 63 FUNC exports) + .so symlink.
- PASS 2 STRICT (no ignore-all) cosmic-pipewire --tests: LINKED but DT_NEEDED libpipewire absent — test harness GC'd the pipewire code (no test calls run()). Weak. Built a real harness crate (pipewire-gap/harness) that CALLS cosmic_pipewire::run.
- PIPEWIRE PROOF (harness, x86_64): ET_DYN + PT_INTERP /lib/ld-musl-x86_64.so.1; DT_NEEDED=[libpipewire-0.3.so.0, libc.so]; 15 pw_ imports bound to stub incl pw_init + pw_main_loop_new; closure CLOSED. Stub genuinely used. DONE.
- Now attempting the literal deliverable (full cosmic-settings-daemon link) by ALSO stubbing openssl (separate gap). Homebrew openssl@3 (3.6.3) headers present -> use as OPENSSL_INCLUDE_DIR; dump full libssl+libcrypto export set (10942 syms) -> one-shot stub libssl.so.3 + libcrypto.so.3 (guaranteed superset, no enumeration). OPENSSL_LIB_DIR/INCLUDE_DIR/NO_VENDOR env in build-daemon.sh; openssl-sys emits rustc-link-search itself.
- CRITICAL openssl scoping insight: reqwest is used ONLY in geonames/src/main.rs (the auto-detected `geonames` BIN, a build-time geodata fetch tool). The daemon links geonames' LIBRARY (lib.rs: GeoPosition + bitcode::decode of an embedded GEODATA blob) which references NO reqwest. So openssl is a BUILD-TIME-ONLY blocker (openssl-sys build.rs runs for the whole dep graph and fails at pkg-config); the daemon binary won't reference reqwest/native-tls => openssl likely NOT even DT_NEEDED. => openssl stub is runtime-irrelevant; graceful-inert is automatic. (Same applies to the rustls fix — both just unblock openssl-sys build.rs.)
- Launched full daemon strict build (bg bc3d9qat6). Long pole (~libcosmic/iced tree).
- FULL DAEMON LINK x86_64: SUCCESS. ET_DYN + PT_INTERP /lib/ld-musl-x86_64.so.1; DT_NEEDED=[libpipewire-0.3.so.0(stub), libudev.so.1(m4), libc.so(m3)]; 15 pw_ imports bound to stub; 0 residual pw_/spa_; openssl NOT DT_NEEDED + 0 residual SSL_/crypto (openssl was build-time-only, exactly as reasoned). out/cosmic-settings-daemon-x86_64 (13.5 MB). verify-closure: only libpipewire-0.3.so.0 shows "missing" (our stub, unknown to the m3/m4/m5 verifier) => sole new runtime lib to stage on-target.
- Design doc written: notes/pipewire-gap-design.md (option comparison, 63-sym table, on-target checklist, landmines).
- aarch64: built lib/aarch64/libpipewire-0.3.so.0 (63 syms) + openssl/lib/aarch64/{libssl,libcrypto}.so.3; per-arch OPENSSL_LIB_DIR. Launched aarch64 daemon build (bg bzbxfkz82).
- FULL DAEMON LINK aarch64: SUCCESS, identical to x86_64. ET_DYN + PT_INTERP /lib/ld-musl-aarch64.so.1; DT_NEEDED=[libpipewire-0.3.so.0(stub), libudev.so.1, libc.so]; 15 pw_ imports; 0 residual pw_/spa_; openssl NOT DT_NEEDED. out/cosmic-settings-daemon-aarch64.
- DONE both arches. Deliverables: notes/pipewire-gap-design.md; pipewire-gap/{inc,lib,openssl,harness,out,cc,*.sh}. (Redundant waiter bzc4bqspz failed on a path typo — irrelevant; real builds notified fine.)
