#!/usr/bin/env bash
# Build Leandros user-space programs.

set -euo pipefail
# macOS: pick an SDK whose libSystem the linker can read (see darwin-sdk.sh).
source "$(dirname "${BASH_SOURCE[0]}")/darwin-sdk.sh"
cd "$(dirname "$0")/.."

TARGET="aarch64-unknown-none"
MODE="debug"
CHECK=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check)   CHECK=true ;;
        --release) MODE="release" ;;
        --target)
            shift
            case "$1" in
                amd64|x86_64) TARGET="x86_64-unknown-none" ;;
                aarch64) TARGET="aarch64-unknown-none" ;;
                *) echo "❌ Invalid target: $1. Use aarch64, x86_64, or amd64"; exit 1 ;;
            esac
            ;;
        *) echo "❌ Unknown option: $1"; exit 1 ;;
    esac
    shift
done

CARGO_ARGS=(--target "$TARGET" --manifest-path userland/Cargo.toml --workspace)

if [[ "$MODE" == "release" ]]; then
    CARGO_ARGS+=(--release)
fi

# Programs that need a real libc (TLS, sigaction/pthread/timer, stdio-less C
# ABI): no_std crates linked against static musl (<arch>-unknown-linux-musl)
# through the zig linker wrapper, the same path as spawnwedge/killmt. Their
# start-up is musl's crt1 -> `main`; userland/muslrt holds the shared pieces.
MUSL_LINKED=(polltest fdmaxtest mqtest pipebw ptytest forktest shmtest racetest mmapbench ping waittest sigchldtest jobtest exectest smpwaketest idletest drmsmoke gpuqtest evtest2 uinputtest evsplit vttest venustest uptrtest i915test strtest pthreadtest timertest sigtest sigtest2 epolltest wakepolltest)

EXCLUDE_ARGS=()
for prog in "${MUSL_LINKED[@]}"; do
    EXCLUDE_ARGS+=(--exclude "$prog")
done

if [[ "$TARGET" == "aarch64-unknown-none" ]]; then
    MUSL_ARCH="aarch64"
else
    MUSL_ARCH="x86_64"
fi
MUSL_TARGET="${MUSL_ARCH}-unknown-linux-musl"
MUSL_RUSTFLAGS="-C linker=$PWD/scripts/linker-${MUSL_ARCH}-musl.sh -C link-self-contained=no -C relocation-model=static"

if $CHECK; then
    echo "[userland] cargo check …"
    cargo check "${CARGO_ARGS[@]}" "${EXCLUDE_ARGS[@]}"

    for prog in "${MUSL_LINKED[@]}"; do
        RUSTFLAGS="$MUSL_RUSTFLAGS" cargo check --manifest-path userland/Cargo.toml -p "$prog" --target "$MUSL_TARGET"
    done

    echo "[userland] OK — type-check passed"
    exit 0
fi

echo "[userland] cargo build …"
RUSTFLAGS="-C link-arg=--entry=_start -C link-arg=-static -C linker=rust-lld -C relocation-model=static" \
cargo build "${CARGO_ARGS[@]}" "${EXCLUDE_ARGS[@]}"

OUT="userland/target/${TARGET}/${MODE}"
mkdir -p "$OUT"

for prog in "${MUSL_LINKED[@]}"; do
    echo "[userland] Building $prog (musl)..."
    RUSTFLAGS="$MUSL_RUSTFLAGS" \
    cargo build --manifest-path userland/Cargo.toml -p "$prog" --target "$MUSL_TARGET" --release
    cp "userland/target/${MUSL_TARGET}/release/$prog" "${OUT}/$prog"
done

# C programs (DOOM, floattest) link libleandros_libc.a and are compiled by clang
# with the hardware float ABI (doubles in xmm0-7). x86_64-unknown-none is
# soft-float (doubles in rdi/rsi/rax, no SSE codegen), so on x86_64 the archive
# C links against is built for the leandros target (+sse,+sse2, hard-float):
# every exported double function and every compiler-rt float builtin
# (__powidf2, __floattidf, ...) then matches the C caller (lane floatabi).
# aarch64-unknown-none is already hard-float, its archive is used as is.
if [[ "$TARGET" == "x86_64-unknown-none" ]]; then
    echo "[userland] Building hard-float libleandros_libc.a for C programs..."
    RUSTFLAGS="-C link-arg=--entry=_start -C link-arg=-static -C linker=rust-lld -C relocation-model=static" \
    cargo build --manifest-path userland/Cargo.toml -p leandros-libc --target targets/x86_64-unknown-leandros.json -Z build-std=core,alloc -Zjson-target-spec --release
fi

# privtest is plain C, shared verbatim with a host Linux run (cc privtest.c)
# so every expectation is checked against a real kernel. Static musl via zig,
# like the other C bits of userland; skipped with a warning without zig.
if command -v zig >/dev/null 2>&1; then
    case "$TARGET" in
        aarch64-unknown-none) ZT=aarch64-linux-musl ;;
        *)                    ZT=x86_64-linux-musl ;;
    esac
    echo "[userland] Building privtest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/privtest" userland/privtest/privtest.c
    # memcpybench-musl: memcpybench built against zig's static musl (its compiler_rt mem*), to compare with the libc/musl-dynamic mem* (lane relibcmem).
    zig cc -target "$ZT" -static -O2 -fno-builtin -s -o "${OUT}/memcpybench-musl" userland/memcpybench-c/memcpybench.c "$(scripts/musl-mem-obj.sh "${ZT%%-*}")"
    # nettest: same idea (ICMP/UDP/TCP basics, checked against a host kernel).
    echo "[userland] Building nettest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/nettest" userland/nettest/nettest.c
    # netlinktest: NETLINK_ROUTE dumps, getifaddrs, if_nameindex, SIOCGIF* (nICEr / Firefox WebRTC ICE gathering).
    echo "[userland] Building netlinktest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/netlinktest" userland/netlinktest/netlinktest.c
    # ip6test: AF_INET6 TCP/UDP on ::1, the family filter (design-ipv6 L1).
    echo "[userland] Building ip6test (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/ip6test" userland/ip6test/ip6test.c
    # udpselftest: UDP to our own interface IP is delivered locally; an unresolved next hop does not block the socket.
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/udpselftest" userland/udpselftest/udpselftest.c
    # udptest: UDP semantics, EPOLLET edges of UDP/ICMP/listeners (checked against a host kernel).
    echo "[userland] Building udptest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/udptest" userland/udptest/udptest.c
    # pollnarrow: blocked /dev/netctl and console readers park on a narrow poll tag.
    echo "[userland] Building pollnarrow (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/pollnarrow" userland/pollnarrow/pollnarrow.c
    # listentag: a TCP listener's pollers park on its own key and see every connect at once.
    echo "[userland] Building listentag (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/listentag" userland/listentag/listentag.c
    # vfstag: blocked flock/signalfd/inotify/device waiters park on a narrow poll tag.
    echo "[userland] Building vfstag (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/vfstag" userland/vfstag/vfstag.c
    # nettag2: blocking socket I/O and unbound/ICMP sockets park on a narrow tag and still wake at once.
    echo "[userland] Building nettag2 (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/nettag2" userland/nettag2/nettag2.c
    # kparks: console poll/epoll, empty poll sets and VT_WAITACTIVE park on a narrow poll tag.
    echo "[userland] Building kparks (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/kparks" userland/kparks/kparks.c
    # netcheck: the end-to-end Internet check behind /bin/net-autotest (net.test=1).
    echo "[userland] Building netcheck (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/netcheck" userland/netcheck/netcheck.c
    # sndtest (lane hda): plays test tones through /dev/pipewire, the path
    # PipeWire's sink uses; run at boot by /bin/snd-autotest (snd.test=1).
    echo "[userland] Building sndtest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/sndtest" userland/sndtest/sndtest.c -lm
    # camtest (lane lapv4l2): V4L2 mmap capture from /dev/video0; --probe is the no-camera regress check.
    echo "[userland] Building camtest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/camtest" userland/camtest/camtest.c
    # syscalltest: splice/tee/vmsplice, rseq and pidfd (checked against a host kernel).
    echo "[userland] Building syscalltest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/syscalltest" userland/syscalltest/syscalltest.c
    # uaccesstest: read(2)/write(2) on never-touched user pages (console, pipe, file, socket, pty).
    echo "[userland] Building uaccesstest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/uaccesstest" userland/uaccesstest/uaccesstest.c
    # proctest: rusage/faults, /proc links, clone3/pdeathsig/CLONE_FS (checked against a host kernel).
    echo "[userland] Building proctest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/proctest" userland/proctest/proctest.c
    # proctest-cold.bin: 2 MiB of real bytes nothing reads before proctest's majflt_cold (staged into /bin).
    yes proctest-cold | head -c 2097152 > "${OUT}/proctest-cold.bin" || true  # yes dies of SIGPIPE under pipefail
    # chroottest: openat2, vfork/clone3, PATH_MAX paths, /proc in a chroot (checked against a host kernel).
    echo "[userland] Building chroottest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/chroottest" userland/chroottest/chroottest.c
    echo "[userland] Building mnttest..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/mnttest" userland/mnttest/mnttest.c -lpthread
    echo "[userland] Building captest..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/captest" userland/captest/captest.c
    # bpftest: bpf(2) cgroup device programs (lane bpfsys).
    echo "[userland] Building bpftest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/bpftest" userland/bpftest/bpftest.c
    # seccomptest: prctl/seccomp(2) behaviour (lane seccore).
    echo "[userland] Building seccomptest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/seccomptest" userland/seccomptest/seccomptest.c
    # preadtest: 8 threads pread/pwrite one shared block-device fd (positional VFS ops).
    echo "[userland] Building preadtest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/preadtest" userland/preadtest/preadtest.c -lpthread
    # kexectest: kexec_file_load of /boot/kexec (load, re-load, bad flags, unload).
    echo "[userland] Building kexectest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/kexectest" userland/kexectest/kexectest.c
    # memfdtest: crun cloned-binary sequence (memfd_create, seals, sendfile, fexecve).
    echo "[userland] Building memfdtest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/memfdtest" userland/memfdtest/memfdtest.c
    # conmontest: syscall sequences conmon performs at start (lane conmondbg).
    echo "[userland] Building conmontest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/conmontest" userland/conmontest/conmontest.c
    # exitracetest: reaped-pid and rename visibility to a freshly exec'd process (lane exitrace).
    echo "[userland] Building exitracetest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/exitracetest" userland/exitracetest/exitracetest.c
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/exitwrap" userland/exitracetest/exitwrap.c
    # suspendtest: one freeze-only device suspend/resume cycle (kernel pm).
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/suspendtest" userland/suspendtest/suspendtest.c
    # kexec: kexec-tools style front end (-l/-u/-e) over kexec_file_load + reboot(KEXEC).
    echo "[userland] Building kexec (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/kexec" userland/kexec/kexec.c
    # kmodctl / kmodctltest: kernel module list/load/unload/info and the finit_module error-path test.
    echo "[userland] Building kmodctl, kmodctltest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/kmodctl" userland/kmodctl/kmodctl.c
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/kmodctltest" userland/kmodctltest/kmodctltest.c
    # serrx: framed, ACKed file receiver over the serial console (scripts/pi-serial-push.py).
    echo "[userland] Building nstest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/nstest" userland/nstest/nstest.c
    echo "[userland] Building tuntest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/tuntest" userland/tuntest/tuntest.c
    echo "[userland] Building brtest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/brtest" userland/brtest/brtest.c
    echo "[userland] Building ovltest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/ovltest" userland/ovltest/ovltest.c
    echo "[userland] Building ovlmknodtest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/ovlmknodtest" userland/ovlmknodtest/ovlmknodtest.c
    echo "[userland] Building inotifytest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/inotifytest" userland/inotifytest/inotifytest.c
    echo "[userland] Building idmaptest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/idmaptest" userland/idmaptest/idmaptest.c
    echo "[userland] Building cgtest (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -O2 -s -Wall -o "${OUT}/cgtest" userland/cgtest/cgtest.c
    echo "[userland] Building serrx (zig cc -target $ZT)..."
    zig cc -target "$ZT" -static -Os -s -Wall -o "${OUT}/serrx" userland/serrx/serrx.c
else
    echo "[userland] WARNING: zig not found — privtest/nettest/udptest/netcheck/sndtest/syscalltest/chroottest/kexectest/kexec not built"
fi

echo ""
echo "[userland] Build complete in ${OUT}"

