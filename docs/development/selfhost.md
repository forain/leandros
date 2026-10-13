# Self-hosting: building the kernel inside LeandrOS

The first self-hosting milestone was reached on 2026-10-13: the LeandrOS kernel
builds inside a running LeandrOS guest, with the pinned nightly toolchain, in an
Alpine chroot. The result boots, and it passes the regression suite.

## Result (aarch64, HVF, 8 GiB guest, 4 vCPUs, `cargo -j2`)

| | host (macOS, M-series, `-j2`) | guest (LeandrOS under HVF, `-j2`) |
|---|---|---|
| `cargo build` of the standard kernel, clean | 28 s | 141 s |
| kernel ELF | 7 118 656 bytes | 7 118 704 bytes |
| `.text` / `.rodata` / `.data` / `.bss` | 1 319 752 / 134 000 / 5 076 280 / 7 245 840 | identical sizes |
| functions (name with crate hashes stripped, size) | 2463 | 2463, all equal |
| boot + `runtests.py aarch64` | 19/19 | 19/19 |

The two ELFs are not byte-identical, and they cannot be with an unpatched cargo.
Cargo mixes the output of `rustc -vV` into each crate's `-C metadata`, and that
output names the host triple (`aarch64-apple-darwin` on the Mac,
`aarch64-unknown-linux-musl` in the guest). The crate disambiguators in symbol
names therefore differ, and the linker orders functions by those names. Every
function has the same size on both sides, and `.rodata` differs only in the
function pointers that the different layout moves. The strings are identical,
because both builds remap the source, `CARGO_HOME` and sysroot paths. Wall time
also varies. A fetch or a host build running alongside stretches the guest.

## How it runs

```text
host                                            guest (root shell)
scripts/selfhost/stage.sh aarch64               /usr/sbin/alpine-sysroot /sk busybox curl tar xz gcc musl-dev ca-certificates
scripts/selfhost/serve.py <stage> <out>  <--->  chroot /sk ... sh guest-build-kernel.sh http://<gateway>:8765 2
scripts/selfhost/kernel-cargo.sh . <td> aarch64 (reference build)
scripts/selfhost/compare.py host.elf guest.elf
```

- **`stage.sh [arch]`** fills `~/.cache/leandros/selfhost/<arch>/` with the
  following inputs. It is reproducible and checks nothing against the network
  at build time.
  - The nightly pinned in `rust-toolchain.toml`: `rustc`, `cargo`,
    `rust-std` (the musl host) and `rust-src`. They are downloaded from
    `static.rust-lang.org` and verified against the channel manifest's
    sha256.
  - `cargo-home.tar.gz`: a `CARGO_HOME` holding the registry index and crate
    files that the workspace needs (both bare-metal targets) and the
    build-std'd library needs. The guest builds `--offline`.
  - `git archive HEAD`, without `artifacts/`.
  - The guest driver and `SHA256SUMS`.
- **`serve.py`** serves that directory over HTTP and takes `PUT` uploads (the
  kernel and the log) into an output directory. The guest reaches the host at
  its default gateway: `192.168.105.1` under socket_vmnet, `10.0.2.2` under
  SLIRP.
- **`guest-build-kernel.sh URL [JOBS]`** runs inside the chroot. It fetches
  and verifies the inputs, then unpacks the toolchain into `/opt/rust` in
  place, using `--strip-components`, with no second copy. It then runs
  `kernel-cargo.sh` and uploads the result.
- **`kernel-cargo.sh SRC TD ARCH [JOBS]`** is the command both sides run. It is
  `build-all.sh`'s standard (Limine) kernel command plus `--remap-path-prefix`
  for the three absolute paths that end up in panic locations.

To boot a guest-built kernel, swap it into the boot image's FAT partition:
`mcopy -o -i leandros-limine-aarch64.img@@1M kernel-aarch64-guest ::/kernel.elf`.
`build-all.sh` (or `m7z2-kernel-only.sh`) puts the host kernel back.

### Resources

- **Memory and CPUs.** `LEANDROS_QEMU_MEM=8G` is enough (the default is 4G),
  with 4 vCPUs.
- **Disk.** The root disk ships about 1.8 GB free. The build needs about
  1.3 GB: the toolchain unpacks to 920 MB, the downloads take 160 MB, the
  chroot about 170 MB and the target directory about 250 MB.

## Kernel bugs the build found (all fixed on `lane/selfkernel`)

1. **execve truncated argv and envp at 64 strings / 8 KiB, silently.**
   - *Symptom.* `xargs sha256sum` hashed 60 of 200 files and exited 0. More
     importantly, `gcc` → `collect2` → `ld` lost the tail of its command line,
     including `crtn.o`. Every build script linked by Alpine's gcc then had a
     half `_init` and died with SIGILL on its first instruction past it.
   - *Fix.* Linux limits: ARG_MAX is a quarter of the stack (2 MiB) and one
     string is at most 128 KiB, with E2BIG beyond either. A bad pointer is
     EFAULT.
2. **f2fs getdents cursors were entry ordinals.** `rm -rf` reads a batch,
   unlinks it and reads on, so it skipped as many survivors as it had removed:
   "Directory not empty" on the old toolchain. Cursors are now dentry-slot
   positions, as Linux f2fs uses (`bit_pos`). **tmpfs** had the same bug and now
   uses pool indices.
3. **poll treated any fd 0 as the console.** A file redirected onto stdin never
   became readable to poll, so busybox `read` (which polls first) hung
   `while read ...; done < file`.
4. **fchmodat read its non-existent 4th argument as flags; fchmodat2 was
   missing.** GNU tar restores directory modes with
   `fchmodat(..., AT_SYMLINK_NOFOLLOW)`. musl tries fchmodat2 first, then
   falls back to `chmod("/proc/self/fd/N")`, which fails in a chroot without
   /proc: "Cannot change mode".
5. **SVE/SME access traps raised SIGSEGV instead of SIGILL** (aarch64).
   OpenSSL's capability probe in the static `cargo` executes an SME instruction
   under a SIGILL handler. `cargo -V` died.

## Known gaps worked around, owned elsewhere

- **No /proc (and no /dev) inside a chroot.** This is owned by `lane/chrootgaps`.
  - musl computes the main program's `$ORIGIN` from `/proc/self/exe`, so rustc
    could not find `librustc_driver`. `guest-build-kernel.sh` sets
    `LD_LIBRARY_PATH=/opt/rust/lib` while `/proc/self/exe` is missing.
  - `/sk/dev/null` ends up a regular file, which is harmless for this build.
- **`AT_HWCAP` is 0 on aarch64.** Programs that select code paths from it (OpenSSL,
  memchr-style SIMD dispatch) take their generic paths.
