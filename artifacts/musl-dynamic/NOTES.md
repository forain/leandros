# musl DYNAMIC linking world — K3-prep lane

Workdir: `/Users/forain/.claude-forain/jobs/afde2e74/tmp/musl-dynamic`
Goal: build a real, DYNAMIC musl userland (`ld-musl-<arch>.so.1` + `libc.so` + crt objects +
headers) for x86_64-linux-musl and aarch64-linux-musl, plus a test corpus (C dynamic exe, dlopen,
Rust dynamic-musl), then extract the exact loader/auxv expectations (PT_INTERP, DT_NEEDED, ET_DYN,
`.rela.dyn`) the K3 kernel PT_INTERP work needs to satisfy.

HOST-ONLY. No QEMU. No leandros repo modifications. No git commands. Task complete.

## VERDICT

**Both architectures: SUCCESS.** musl 1.2.5 builds cleanly from source via `zig cc` on this macOS
host, with real dynamic linking (PT_INTERP executables, `.so` libraries, `dlopen`, and dynamically
linked Rust/std binaries) all verified end-to-end via static ELF analysis. One serious toolchain
landmine was found and fixed (see below) that would otherwise have silently produced a broken
`libc.so` missing `memcpy`/`memset`/`memmove`/`strlen`/`bcmp` and ~60 libm entry points from its
export table.

## Deliverables (paths)

- Toolchain wrappers: `toolchain/{x86_64,aarch64}-linux-musl-{cc,c++,ar,ranlib}` (copied/extended
  from `leandros/ports/mesa/toolchain`, unmodified — the `--version-script` positional-arg fix from
  the S3 mesa spike carries over unchanged and was not itself a problem here).
- Dynamic-link helper: `toolchain/musl-dyn-link.sh` (see "Landmine 1" — bypasses `zig cc`'s
  driver-level musl auto-management for the final link step).
- Raw-linker wrapper for Rust: `toolchain/zig-ld-lld` (`exec zig ld.lld "$@"`).
- Patched compiler-rt shims (see "Landmine 2"): `toolchain/rtlib-shim/{x86_64,aarch64}/libcompiler_rt_patched.a`.
- musl source: `src/musl-1.2.5/` (release tarball from musl.libc.org).
- Build trees: `build/x86_64/`, `build/aarch64/` (out-of-tree via musl's `--srcdir`).
- **Sysroots (the deliverable)**: `sysroot/x86_64/`, `sysroot/aarch64/`, each:
  - `lib/ld-musl-<arch>.so.1` — symlink to `usr/lib/libc.so` (musl's loader IS libc.so; see "Image
    layout" below)
  - `usr/lib/libc.so` — the shared libc + dynamic linker, ET_DYN, self-relocating, no PT_INTERP of
    its own
  - `usr/lib/{crt1,crti,crtn,Scrt1,rcrt1}.o` — crt objects (static/PIE/static-PIE/dynamic-PIE start files)
  - `usr/lib/libc.a`, `libm.a`, `libpthread.a`, etc. (static archives, all now empty/no-op stubs
    except libc.a — musl 1.2.5 merges libm/libpthread/etc into libc.so)
  - `usr/include/` — full musl headers
- Test corpus: `test/hello-dyn/`, `test/dlopen-host/`, `test/hello-dyn-rs/` (binaries for both
  arches sit alongside each `Cargo.toml`/`.c` as `<name>-<arch>`).

## Host tooling used

- zig 0.16.0 (`/opt/homebrew/bin/zig`) — C/C++ cross-compiler frontend (`zig cc`/`zig c++`) AND raw
  linker (`zig ld.lld`, LLD 21.1.8) AND archiver (`zig ar`/`zig ranlib`).
- LLVM 22.1.5 via homebrew (`/opt/homebrew/Cellar/llvm/22.1.5/bin/`) for `llvm-readelf`,
  `llvm-readobj`, `llvm-objdump`, `llvm-objcopy`, `llvm-ar`, `llvm-nm` — NOT on PATH by default,
  must reference the Cellar path directly (no `llvm-readobj`/`llvm-objdump` on bare PATH).
- rustup nightly (`nightly-aarch64-apple-darwin`) with `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl` std targets **already installed** (no `-Zbuild-std` needed).
- musl source: 1.2.5 release tarball, `https://musl.libc.org/releases/musl-1.2.5.tar.gz`.
- No qemu-user (only `qemu-system-*` present, and QEMU is out of scope for this job anyway) — all
  verification below is static ELF analysis, no execution.

## Build recipe (works, reproducible)

```sh
export PATH="$PWD/toolchain:$PATH"
mkdir -p build/<arch> && cd build/<arch>
CC=<arch>-linux-musl-cc AR=<arch>-linux-musl-ar RANLIB=<arch>-linux-musl-ranlib \
  ../../src/musl-1.2.5/configure \
    --target=<arch>-linux-musl --prefix=/usr --syslibdir=/lib \
    --enable-shared --enable-wrapper=no
make -j$(sysctl -n hw.ncpu) LIBCC="-rtlib=none $PWD/../../toolchain/rtlib-shim/<arch>/libcompiler_rt_patched.a"
make install DESTDIR=../../sysroot/<arch> LIBCC="-rtlib=none $PWD/../../toolchain/rtlib-shim/<arch>/libcompiler_rt_patched.a"
```

The `LIBCC=` override is **mandatory** — see Landmine 2. Everything else (configure, compile) needs
no special handling: `./configure`'s compiler-capability probing all passed cleanly against `zig
cc`, and object compilation (`obj/**/*.o`, `obj/**/*.lo`) needed zero patches.

`./configure` rejected `-Wl,--dynamic-list=...` ("no") for both arches — cosmetic, this only affects
whitelisting a handful of extra-exported globals (`environ`, `malloc` family, `optarg`, etc. — see
`dynamic.list` in the musl tree) that aren't otherwise referenced by relocations. Not investigated
further since it didn't block anything material; flag for follow-up if `environ`/`malloc`
interposition matters later.

## Toolchain landmines found

### Landmine 1 — `zig cc`'s driver silently substitutes its OWN bundled musl and defaults to fully static

Compiling a test program the "obvious" way —
`x86_64-linux-musl-cc --sysroot=$SYSROOT -o hello hello.c` (no `-static` requested) — silently
produces a **fully static, non-PIE** ELF (`ET_EXEC`, no `PT_INTERP`, no `DT_NEEDED`), and critically
it does this by **discarding `--sysroot` for CRT/libc selection** and linking against zig's own
internal, separately-built musl in `~/.cache/zig/o/*/`  (`crt1.o`, `libc.a`), not our from-source
sysroot. `-v` confirms: the emitted `ld.lld` line hardcodes `-static` and points at
`~/.cache/zig/o/.../crt1.o` / `libc.a` regardless of `--sysroot`.

Passing `-dynamic` fixes the *staticness* (zig emits `--dynamic-linker /lib/ld-musl-<arch>.so.1`
and links `libc.so`) but **still** uses zig's own cached musl, not ours — `--sysroot` is honored for
`-l`/`-L` search paths generally but zig's driver unconditionally injects its own CRT objects ahead
of anything `--sysroot` would find.

Trying to force it further — `-nostdlib` + explicit CRT paths + `-Wl,-dynamic-linker,...` — makes
zig's driver **error out outright**: `ObjectFilesCannotSpecifyDynamicLinker` /
`LldCannotSpecifyDynamicLinkerForSharedLibraries`. Zig's `cc` frontend, once it detects a
`*-musl` target, insists on fully owning CRT/libc selection and refuses manual overrides that
conflict with its assumptions — this isn't a flag we found a way to disable.

**Fix**: don't use `zig cc` as the final linker at all for dynamic-executable/`.so` link steps. Use
`zig cc` only to compile `.c` → `.o` (with explicit `-fPIC` and `-fno-sanitize=all` — see Landmine
3), then link with **`zig ld.lld` directly** (the raw LLD linker zig bundles, invoked as
`zig ld.lld <args>`, exposed under its GNU-binutils-compatible name), passing our own sysroot's
`Scrt1.o`/`crti.o`/`crtn.o`, `-L$sysroot/usr/lib -lc`, and `--dynamic-linker /lib/ld-musl-<arch>.so.1`
explicitly. `zig ld.lld` has none of `zig cc`'s auto-management — it behaves like a normal
GNU-compatible linker. Encapsulated in `toolchain/musl-dyn-link.sh` (arch, exe|shared, sysroot,
output, objects → correct link line for either a dynamic PIE executable or a `.so`). Used
successfully for `hello-dyn`, `dlopen-host`, and `plugin.so` on both arches.

For Rust: same idea via `-C linker-flavor=ld -C linker=toolchain/zig-ld-lld` (a 1-line
`exec zig ld.lld "$@"` shim, since `-C linker=` wants a single executable, not `zig ld.lld` as two
words) plus `-C link-self-contained=no` (rustc's bundled musl self-contained dir has NO `libc.so`,
only `libc.a` — see "Rust specifics" below) and manually supplied `-C link-args=` for
`--sysroot`/`--dynamic-linker`/CRT objects/`-lc`. `-C link-args` appends at the very end of rustc's
generated link line (after all objects/rlibs) — this works fine functionally (no static
constructors in play that would need strict `.init`/`.fini` fragment ordering from `crti.o`/`crtn.o`
placement) but is worth knowing if a future test program does rely on `.init_array` ordering.

### Landmine 2 — `zig cc` always auto-injects its own `compiler_rt`, which POISONS musl's `memcpy`/`memset`/`memmove`/`strlen`/`bcmp`/~60 libm exports into hidden-local (SEVERE, found + fixed)

This is the big one — it silently produces a **broken** `libc.so` that looks fine (builds, has the
right `PT_INTERP`/`ET_DYN`/segment structure) but is missing `memcpy`, `memset`, `memmove`,
`strlen`, `bcmp`, and ~60 libm functions (`sin`, `cos`, `sqrt`, `exp`, `log`, `floor`, `fmod`, `fma`,
...) from its **dynamic** symbol table — anything not inlined by the compiler that calls these from
outside `libc.so` (which is virtually every nontrivial C or Rust program) fails to link/load.

**Root cause**: `zig cc`, even under `-nostdlib`, unconditionally links its own bundled
`libcompiler_rt_zcu.o` (a single fat object, `~/.cache/zig/o/*/libcompiler_rt.a`) into every link —
there's no `zig cc`-level flag to suppress this (`-fno-compiler-rt`/`-fno-ubsan-rt` are `zig
build-exe`-only flags, not accepted by the clang-compatible `zig cc` frontend: "Unknown Clang
option"). Standard clang `-rtlib=none` DOES suppress it when passed directly to `zig cc` in
isolation, but musl's `Makefile` structure (CFLAGS_ALL vs LDFLAGS_ALL vs `$(LIBCC)` ordering) meant
it had to go in the `LIBCC` make variable specifically to actually take effect for the real
`lib/libc.so` link recipe — putting it in `CFLAGS_ALL` was silently ineffective (compiler_rt still
got linked; root-caused via diffing the actual `-v` command line, not documented flag semantics).

zig's bundled `libcompiler_rt_zcu.o` provides **weak** aliases for ~460 symbols, including
`memcpy`/`memset`/`memmove`/`strlen`/`bcmp` and most of libm (`cos`, `sqrt`, `floor`, `fmod`, `fma`,
`log`, `log2`, `log10`, `exp`, `exp2`, `sin`, `sincos`, `tan`, `round`, `trunc`, `fmax`, `fmin`,
`fabs`, `ceil`, `__stack_chk_fail`, ...) — freestanding-target fallback implementations. musl
defines **strong** versions of all of these. Per normal ELF/ld semantics the strong musl definition
should simply win and the weak compiler-rt one should be discarded with no side effects — but
`ld.lld` here corrupts the *winning* (musl, strong, default-visibility) symbol's visibility down to
`LOCAL HIDDEN` whenever a same-named weak/hidden-adjacent definition is *also* present in the link,
which removes it from `.dynsym` entirely (confirmed: the object files are `GLOBAL DEFAULT` in
isolation *and* in their own `.lo` *before* the final link; only the fully-linked `libc.so` shows
the corruption — i.e. this is `ld.lld`-behavior-at-final-link, not a compile-time visibility
attribute, and reproduces with both `--gc-sections` on and off). Root-caused by diffing our built
`libc.so`'s `.dynsym` against zig's own reference musl build (`~/.cache/zig/o/*/libc.so`, confirmed
correct — `memcpy`/`strlen`/etc all `GLOBAL DEFAULT` there) and then bisecting which extra input
`zig cc` was injecting that zig's own musl build doesn't hit (because zig's own musl build doesn't
link against zig's compiler-rt at all).

We did **not** get to a root-cause deeper than "weak+strong same-name symbol merge across this
specific `ld.lld` build corrupts visibility of the surviving strong symbol" — flag this as a
`ld.lld`/zig-lld bug worth a minimal standalone repro and upstream report if this toolchain sees
more use; we didn't have time to bisect further whether it's an LLD-general issue or zig's LLD build
specifically.

**Fix applied**: extract zig's `libcompiler_rt_zcu.o`, compute the symbol-name overlap between its
weak exports and musl's own strong `libc.a` globals (66 names — see
`toolchain/rtlib-shim/x86_64/` for the exact list logic), and `llvm-objcopy
--localize-symbol=<name>` (not `--strip-symbol` — several are still referenced by *other*
compiler-rt internals via relocations, e.g. `floor` used inside `floor_ceil.o`-equivalent code, so a
hard strip fails with "not stripping symbol ... named in a relocation"; `--localize-symbol` keeps
the definition but drops it from external/weak-export status) into a patched
`libcompiler_rt_patched.a`, then build musl with `LIBCC="-rtlib=none <path-to-patched-archive>"` so
only musl's own symbols are exported and the *needed-and-not-provided-by-musl* compiler-rt helpers
(`__muldc3`/`__mulsc3`/`__mulxc3` for C99 complex multiply, used by musl's `complex.c`) still link.
Verified clean on both arches: `memcpy`/`memset`/`memmove`/`strlen`/`bcmp`/`cos`/`sqrt`/etc all
`FUNC GLOBAL DEFAULT` in the final `.dynsym` post-fix. Scripted reproducibly — see
`toolchain/rtlib-shim/{x86_64,aarch64}/` (each holds the exact patched archive; regeneration
recipe is the shell history in this NOTES.md, not yet turned into a standalone script — worth doing
if this sysroot gets rebuilt again).

**Impact if missed**: this would have been an extremely nasty silent failure for K3 — every
resulting binary looks structurally perfect (right PT_INTERP, right DT_NEEDED, right ET_DYN) and
only fails at *runtime* symbol resolution once the kernel's loader is far enough along to actually
run one. Worth a regression check (`llvm-readelf --dyn-syms $sysroot/usr/lib/libc.so | grep -w
memcpy` should NOT be empty) if this musl is ever rebuilt.

### Landmine 3 — `zig cc` auto-enables UBSan runtime checks by default, needing its own runtime

Even a trivial `-c` compile at default settings embeds calls into `__ubsan_handle_*` (confirmed via
`-###`: zig cc passes a long `-fsanitize=alignment,array-bounds,bool,...` list unconditionally at
default optimization, unrelated to any `-O` level chosen), which then need `libubsan_rt.a` (another
zig-bundled runtime) at link time. Since we're linking with our own sysroot only (Landmine 1's
fix), that runtime isn't present/wanted. **Fix**: always compile test-corpus objects with
`-fno-sanitize=all` (used throughout `musl-dyn-link.sh`'s companion compile step). Musl's own build
was unaffected because its `CFLAGS_ALL` never opted into this and its own `./configure`-probed flags
(`-Werror=...`, `-fno-*`) evidently don't trigger it — only observed on our own hand-invoked
`zig cc -c` calls for the test corpus.

### Landmine 4 (inherited, unchanged) — positional `-Wl,--version-script <path>`

Carried over from the S3 Mesa spike (`leandros/ports/mesa/NOTES.md`): `zig cc` rejects a
`-Wl,--version-script` flag with the path as a *separate* positional token ("unrecognized file
extension"); the wrapper scripts merge it to `-Wl,--version-script=<path>` form. Not exercised
directly by musl or by our tiny test corpus (no `.map`/version scripts in play), but kept in the
wrapper since it's copied from the proven-working mesa wrappers verbatim.

### Rust specifics

- `rustup`'s bundled musl target support (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
  both pre-installed on the `nightly-aarch64-apple-darwin` toolchain here — no `-Zbuild-std`
  needed) ships a `lib/rustlib/<target>/lib/self-contained/` directory with `crt1.o`/`Scrt1.o`/
  `rcrt1.o`/`crti.o`/`crtn.o`/**`libc.a`** — but **no `libc.so`**. Rust's musl target support is
  built assuming either (a) fully static (`crt-static`, the default — self-contained `libc.a`
  suffices), or (b) dynamic on a *real* musl Linux host where the system already has a `libc.so` at
  the standard location. Cross-compiling dynamic musl from macOS needs our own sysroot's `libc.so`
  supplied explicitly (`-C link-self-contained=no` + manual `-L`/`-lc` pointing at our sysroot).
- Disabling `crt-static` (`-C target-feature=-crt-static`) plus linking dynamically still pulls in
  an expectation of `-lgcc_s` (real `_Unwind_*` personality/backtrace routines) even with
  `panic=abort` set in the release profile — `std` itself references backtrace/unwind machinery
  unconditionally. Rust's own bundled `self-contained/libunwind.a` (LLVM libunwind, real
  implementation, not a stub) is the correct fix: copy/symlink it into the sysroot as
  `usr/lib/libgcc_s.a` so `-lgcc_s` resolves to it. An empty stub archive is NOT sufficient — it
  satisfies the `-l` search but leaves `_Unwind_GetIP`/`_Unwind_Resume`/etc undefined.
- `bcmp` (used internally by `core::slice::cmp`) and `memcpy`/`strlen`/etc are exactly the symbols
  Landmine 2 breaks — the Rust build was the thing that actually surfaced Landmine 2's full blast
  radius (the earlier hand-linked C `hello-dyn`/`dlopen-host` binaries didn't call `strlen`/`memcpy`
  via PLT/GOT from outside libc.so in a way that failed to link, since `zig ld.lld` used directly
  for those doesn't inject compiler-rt at all — only `rustc`'s own generated link line, which
  unconditionally references these musl-libc symbols directly, hit the missing-export problem).
- Cargo config lives at `test/hello-dyn-rs/.cargo/config.toml`, one `[target.<triple>]` section per
  arch, both `[build] target` (default) and an explicit `--target` override both work.

## Loader/auxv expectation table (what K3 must satisfy)

All binaries below are **ET_DYN** (`Type: DYN`) — musl's toolchain default is dynamic-PIE for
executables (`-pie`, `FLAGS_1: NOW PIE`) and there is no non-PIE dynamic executable in this corpus
(nor does musl's own crt provide a non-PIE dynamic start file distinct from `Scrt1.o`'s PIE story —
`crt1.o` is for **static**, `rcrt1.o` for **static-PIE**, `Scrt1.o` for **dynamic-PIE**; there is no
"dynamic non-PIE" crt object in musl at all). **K3's loader must therefore support ET_DYN with a
non-zero, kernel-chosen load bias for ordinary executables, not just for `.so` libraries** — this is
the load-bias gap already flagged in `wayland_cosmic_plan.md`/`project_musl_toolchain.md` from the
earlier static-PIE spike, now confirmed to also be **mandatory for plain dynamic executables**, not
just an edge case.

| Artifact | Type | Interp | DT_NEEDED | RELACOUNT | Notes |
|---|---|---|---|---|---|
| `usr/lib/libc.so` (x86_64) | DYN | *(none — IS the interp)* | *(none)* | 65 | Entry 0xad360 = `_dlstart` (ldso bootstrap), not `main`-style |
| `usr/lib/libc.so` (aarch64) | DYN | *(none)* | *(none)* | 67 | Entry 0xb1410 = `_dlstart` |
| `hello-dyn` (x86_64) | DYN, PIE | `/lib/ld-musl-x86_64.so.1` | `libc.so` | 0 (only `.rela.plt`, no `.rela.dyn` — no data relocs needed) | 3 LOAD segs (R / RX / RW) |
| `hello-dyn` (aarch64) | DYN, PIE | `/lib/ld-musl-aarch64.so.1` | `libc.so` | 3 | aarch64 needed `.rela.dyn` even for this trivial program (GOT-relative addressing differs from x86_64) |
| `dlopen-host` (x86_64) | DYN, PIE | `/lib/ld-musl-x86_64.so.1` | `libc.so` | 0 | links `-ldl`; musl folds `libdl` into `libc.so` so still one NEEDED |
| `dlopen-host` (aarch64) | DYN, PIE | `/lib/ld-musl-aarch64.so.1` | `libc.so` | 3 | |
| `plugin.so` (x86_64/aarch64) | DYN, non-PIE-labeled (shared, no `-e`/entry) | *(none — a library, not loaded via PT_INTERP)* | `libc.so` | 0 | Entry point 0x0 (libraries don't have a "start" the same way) |
| `hello-dyn-rs` (x86_64, Rust) | DYN, PIE | `/lib/ld-musl-x86_64.so.1` | `libc.so` | 621 | 4 LOAD segs (extra RW segment vs C — Rust's TLS/data layout); `.gcc_except_table`/`.eh_frame`/`.eh_frame_hdr` present (unwind tables even with panic=abort, since std itself isn't rebuilt panic=abort) |
| `hello-dyn-rs` (aarch64, Rust) | DYN, PIE | `/lib/ld-musl-aarch64.so.1` | `libc.so` | 422 | |

Common structural facts, all binaries:
- `FLAGS: BIND_NOW`, `FLAGS_1: NOW` (and `PIE` for executables) — every `.so`/executable here is
  built `-z now`: the kernel/loader must apply **all** PLT relocations eagerly at load time, there
  is no lazy-PLT/`.plt.got` deferred-binding path to support for this corpus (simplifies K3 — no
  lazy binding trampoline machinery required for these binaries specifically, though a general
  loader should still handle `DT_NEEDED`→symbol-resolution→`.rela.plt` application regardless of
  BIND_NOW).
- Every ELF has exactly one `PT_INTERP` (executables) or none (libraries, and `libc.so` itself)
  pointing at literally `/lib/ld-musl-<arch>.so.1` — **not** an absolute host path, this is the
  path the kernel/userland image must have that file present at, at that exact path, at boot.
  `readelf -x .interp` / the `[Requesting program interpreter: ...]` line in `-l` output both
  confirm the literal string.
  `objdump`-style with a real Linux `.dynamic`).
- `DT_NEEDED` is uniformly a single entry: `libc.so` (bare name, not an absolute path — resolved by
  the interp via its own search path / `/etc/ld-musl-<arch>.path` / default `/lib:/usr/lib`, see
  "Image layout" below).
- `RELACOUNT` (when nonzero) counts `R_*_RELATIVE` entries in `.rela.dyn` — these are the
  self-relocation entries needed once the load bias is known (base-address-relative fixups with no
  symbol lookup), and must be applied by the loader/kernel **before** any code in that object runs
  (this is exactly what `ld-musl`'s own `_dlstart_c`/`dlstart.c` bootstrap does for itself, using
  hidden-visibility direct calls specifically so it can do this before the GOT is even valid — see
  Landmine 2's discovery process for how that bootstrap-ordering constraint surfaces in practice).
  `.rela.plt` entries (present in every executable/lib here, PLT/GOT slot fixups, all
  `R_*_JUMP_SLOT` given `BIND_NOW`) must also be applied eagerly given `FLAGS: BIND_NOW`.
- Entry point semantics: for ordinary executables, `e_entry` is the real `_start` (musl's
  `crt_arch.h` asm trampoline, calls `__libc_start_main` → `main`) and is a **load-bias-relative**
  offset like everything else in an ET_DYN — the kernel must add its chosen load bias before jumping
  there (standard ET_DYN semantics, `AT_ENTRY` auxv value should be `bias + e_entry`). For `libc.so`
  loaded AS a PT_INTERP target (not exec'd directly), the kernel does NOT jump to `libc.so`'s own
  `e_entry` (`_dlstart`) via a fresh auxv-based entry — rather the *executable's* `AT_ENTRY` stays
  the executable's own entry, and the **kernel jumps to the INTERPRETER's entry point** instead
  (standard `PT_INTERP` semantics: `e_entry` used for the jump is the interpreter's, with the
  executable's real entry communicated via `AT_ENTRY` in the auxv for the interpreter to jump to
  itself after it finishes relocating both itself and the main program). K3 must implement this
  hand-off, not just "jump to the executable's `e_entry`".

## Image layout for the kernel (what must exist on-disk at boot for these binaries to run)

- `/lib/ld-musl-<arch>.so.1` — **must be a real file or a resolvable symlink**; musl's own install
  step (`make install`) creates it as `ln -s <libdir>/libc.so /lib/ld-musl-<arch>.so.1` relative to
  `$(prefix)`. Our sysroots keep this as a symlink (`lib/ld-musl-x86_64.so.1 -> /usr/lib/libc.so`).
  **K3's VFS/loader must resolve this symlink** (or LeandrOS's rootfs packaging step should
  materialize it as a hardlink/real copy if symlink-following in the early-boot path is
  unimplemented/unwanted — flag this as a packaging decision, not a kernel-loader requirement per
  se).
- `/usr/lib/libc.so` — the actual file. Same inode/content serves BOTH roles: (a) as `PT_INTERP`
  target when loading any dynamic executable, and (b) it is technically also directly executable
  standalone (`ld-musl` supports being invoked as `./ld-musl-x86_64.so.1 program args...`, musl's
  "run program with a specific interpreter" mode) — not exercised in this corpus but worth knowing
  the same file serves both roles.
- musl's own runtime library search order (from `ldso/dynlink.c`, not modified by us): compiled-in
  default is `/lib:/usr/local/lib:/usr/lib`, overridable by `/etc/ld-musl-<arch>.path` (one path per
  line) and by `LD_LIBRARY_PATH` (env, standard semantics, lower priority than
  `/etc/ld-musl-<arch>.path` if both are present — musl processes the config file rather than
  favoring the env var, unlike glibc). **For LeandrOS specifically**: since this corpus's `DT_NEEDED`
  is always the bare name `libc.so` (never an absolute path), the kernel/rootfs just needs
  `/usr/lib/libc.so` to exist and be in that default search path — no `/etc/ld-musl-<arch>.path` is
  required unless COSMIC/Mesa `.so`s end up somewhere non-standard (e.g. `/usr/lib/gbm/`,
  flagged in the existing Mesa spike notes as a `dlopen`-only path, not `DT_NEEDED`, so also not
  subject to this search order at all — `dlopen(3)` with an explicit relative/absolute path bypasses
  the search path entirely, as our own `dlopen-host` test does with `"./plugin.so"`).
- No separate `/lib` vs `/usr/lib` split is meaningful content-wise here — `syslibdir=/lib` only
  affects where `ld-musl-<arch>.so.1` itself (the symlink) lands; everything else musl installs
  under `/usr/lib` per `--prefix=/usr`. A minimal LeandrOS rootfs for this corpus needs exactly:
  `/lib/ld-musl-<arch>.so.1` (symlink) + `/usr/lib/libc.so` (real file) + whatever
  application/plugin `.so`s a given program needs alongside it.

## Artifact verification commands (for reference / K3 team to re-run)

```sh
LLVM=/opt/homebrew/Cellar/llvm/22.1.5/bin
$LLVM/llvm-readelf -h <binary>   # ELF type, entry point
$LLVM/llvm-readelf -l <binary>   # PT_INTERP, LOAD segments (offsets/vaddrs/flags/align)
$LLVM/llvm-readelf -d <binary>   # DT_NEEDED, RELACOUNT, FLAGS/FLAGS_1 (BIND_NOW/PIE)
$LLVM/llvm-readelf -S <binary>   # confirm .rela.dyn / .rela.plt / .dynamic / .interp presence
$LLVM/llvm-readelf --dyn-syms <binary>  # exported/imported dynamic symbols (Landmine 2 regression check)
```
(`rust-objdump -p` from rustup was also available and cross-checked consistent with the above for
the Rust binaries; `llvm-readelf` was used throughout for uniformity since it works identically on
both the C and Rust artifacts without needing a target-specific objdump.)
