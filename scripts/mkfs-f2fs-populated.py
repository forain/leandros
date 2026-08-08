#!/usr/bin/env python3
"""Create a pre-populated F2FS image for LeandrOS with all userland binaries.

Matches the on-disk layout expected by servers/f2fs/src/lib.rs.
"""
import hashlib
import struct
import sys
import os

BLOCK_SIZE     = 4096
BLOCKS_PER_SEG = 512       # 2 MB segments (log_blocks_per_seg = 9)
F2FS_MAGIC     = 0xF2F52010
CP_UMOUNT_FLAG = 0x00000008

# Segment layout
SEGMENT0_BLKADDR = BLOCKS_PER_SEG      # 512
SEG_CNT_CKPT = 2
SEG_CNT_SIT  = 1
SEG_CNT_NAT  = 2
SEG_CNT_SSA  = 1

CP_BLKADDR   = SEGMENT0_BLKADDR
SIT_BLKADDR  = CP_BLKADDR  + SEG_CNT_CKPT * BLOCKS_PER_SEG
NAT_BLKADDR  = SIT_BLKADDR + SEG_CNT_SIT  * BLOCKS_PER_SEG
SSA_BLKADDR  = NAT_BLKADDR + SEG_CNT_NAT  * BLOCKS_PER_SEG
MAIN_BLKADDR = SSA_BLKADDR + SEG_CNT_SSA  * BLOCKS_PER_SEG

META_SEGS  = 1 + SEG_CNT_CKPT + SEG_CNT_SIT + SEG_CNT_NAT + SEG_CNT_SSA

NODE_INO = 1
META_INO = 2
ROOT_INO = 3

NAT_ENTRY_SIZE = 9
SIT_ENTRY_SIZE = 74
SIT_PER_BLK    = BLOCK_SIZE // SIT_ENTRY_SIZE
MAX_ACTIVE_LOGS = 8
NODE_FOOTER_OFF = 4076

# Directory constants
NR_DENTRY_IN_BLK   = 214
DENTRY_BITMAP_SIZE = 27
DENTRY_RESERVED    = 3
DENTRY_ENTRIES_OFF = DENTRY_BITMAP_SIZE + DENTRY_RESERVED # 30
DENTRY_SLOT_LEN    = 8
DENTRY_NAMES_OFF   = DENTRY_ENTRIES_OFF + NR_DENTRY_IN_BLK * 11 # 2384
DENTRY_ENTRY_SIZE  = 11

DT_DIR = 4
DT_REG = 8
# Symlink. The f2fs server stores the Linux d_type byte in the dentry rather
# than the F2FS_FT_* enum (servers/f2fs/src/lib.rs:169-174), and its path walker
# follows a component iff that byte is DT_LNK (resolve_path_ex, :1766). A
# symlink inode is otherwise an ordinary file inode whose mode is S_IFLNK|0777
# and whose data is the target path — exactly what handle_symlink writes
# (:2333-2341) — so add_files_to_dir needs no new machinery beyond emitting the
# right ftype for a link mode.
DT_LNK = 10

S_IFMT  = 0o170000
S_IFLNK = 0o120000

def w8 (b, o, v): b[o] = v & 0xFF
def w16(b, o, v): struct.pack_into('<H', b, o, v & 0xFFFF)
def w32(b, o, v): struct.pack_into('<I', b, o, v & 0xFFFFFFFF)
def w64(b, o, v): struct.pack_into('<Q', b, o, v & 0xFFFFFFFFFFFFFFFF)

def write_superblock(image, base, total_blocks, total_segs, main_segs):
    s = base + 1024
    w32(image, s +  0, F2FS_MAGIC)
    w16(image, s +  4, 1)     # major_ver
    w16(image, s +  6, 12)    # minor_ver
    w32(image, s +  8, 9)     # log_sectorsize
    w32(image, s + 12, 3)     # log_sectors_per_block
    w32(image, s + 16, 12)    # log_blocksize
    w32(image, s + 20, 9)     # log_blocks_per_seg
    w32(image, s + 24, 1)     # segs_per_sec
    w32(image, s + 28, 1)     # secs_per_zone
    w32(image, s + 32, 0)     # checksum_offset
    w64(image, s + 36, total_blocks)
    w32(image, s + 44, main_segs)
    w32(image, s + 48, total_segs)
    w32(image, s + 52, SEG_CNT_CKPT)
    w32(image, s + 56, SEG_CNT_SIT)
    w32(image, s + 60, SEG_CNT_NAT)
    w32(image, s + 64, SEG_CNT_SSA)
    w32(image, s + 68, main_segs)
    w32(image, s + 72, SEGMENT0_BLKADDR)
    w32(image, s + 76, CP_BLKADDR)
    w32(image, s + 80, SIT_BLKADDR)
    w32(image, s + 84, NAT_BLKADDR)
    w32(image, s + 88, SSA_BLKADDR)
    w32(image, s + 92, MAIN_BLKADDR)
    w32(image, s + 96, ROOT_INO)
    w32(image, s +100, NODE_INO)
    w32(image, s +104, META_INO)

def build_dentry_blocks(entries):
    """Lay out directory entries across as many 4096-byte blocks as needed.

    Returns a list of blocks. Spilling matters now that /bin holds the ~100
    hardlinked coreutils names: a single block fits only NR_DENTRY_IN_BLK
    slots, and long names burn two slots each. The f2fs server already reads
    multi-block directories (it loops over blocks derived from the inode
    size), so the image writer was the only single-block component.
    """
    blocks = []
    block = bytearray(4096)
    bitmap = bytearray(27)
    current_slot = 0

    def flush():
        block[0:27] = bitmap
        blocks.append(block)

    for name, ino, ftype in entries:
        name_len = len(name)
        slots_used = (name_len + DENTRY_SLOT_LEN - 1) // DENTRY_SLOT_LEN
        if slots_used > NR_DENTRY_IN_BLK:
            raise ValueError(f"Directory entry name too long: {name!r}")
        if current_slot + slots_used > NR_DENTRY_IN_BLK:
            flush()
            block = bytearray(4096)
            bitmap = bytearray(27)
            current_slot = 0
        for i in range(slots_used):
            slot = current_slot + i
            byte = slot // 8
            bit = slot % 8
            bitmap[byte] |= (1 << bit)
        e_off = DENTRY_ENTRIES_OFF + current_slot * DENTRY_ENTRY_SIZE
        w32(block, e_off + 0, 0)
        w32(block, e_off + 4, ino)
        w16(block, e_off + 8, name_len)
        block[e_off + 10] = ftype
        n_off = DENTRY_NAMES_OFF + current_slot * DENTRY_SLOT_LEN
        block[n_off : n_off + name_len] = name
        current_slot += slots_used

    flush()
    return blocks

def build_inode_block(mode, links, size, block_addrs, nid):
    block = bytearray(4096)
    w16(block, 0, mode)
    w8 (block, 2, 0)
    w8 (block, 3, 0)
    w32(block, 12, links)
    w64(block, 16, size)
    for idx, addr in enumerate(block_addrs):
        w32(block, 364 + idx * 4, addr)
    w32(block, NODE_FOOTER_OFF + 0, nid)
    w32(block, NODE_FOOTER_OFF + 4, nid)
    w32(block, NODE_FOOTER_OFF + 8, 0)
    w32(block, NODE_FOOTER_OFF + 12, 0)
    w32(block, NODE_FOOTER_OFF + 16, 1)
    return block

def write_checkpoint(image, blkaddr, ver, valid_blocks, valid_nodes, free_segs, next_nid, main_segs,
                      cur_node_segno, cur_node_blkoff, cur_data_segno, cur_data_blkoff):
    o = blkaddr * BLOCK_SIZE
    w64(image, o +  0, ver)
    w64(image, o +  8, 0)
    w64(image, o + 16, valid_blocks)
    w32(image, o + 24, 0)
    w32(image, o + 28, 1)
    w32(image, o + 32, free_segs)
    for i in range(MAX_ACTIVE_LOGS):
        w32(image, o + 36 + i * 4, 0xFFFFFFFF if i > 0 else cur_node_segno)
    for i in range(MAX_ACTIVE_LOGS):
        w16(image, o + 68 + i * 2, cur_node_blkoff if i == 0 else 0)
    for i in range(MAX_ACTIVE_LOGS):
        w32(image, o + 84 + i * 4, 0xFFFFFFFF if i > 0 else cur_data_segno)
    for i in range(MAX_ACTIVE_LOGS):
        w16(image, o + 116 + i * 2, cur_data_blkoff if i == 0 else 0)
    w32(image, o + 132, CP_UMOUNT_FLAG)
    w32(image, o + 136, 5)
    w32(image, o + 140, 1)
    w32(image, o + 144, valid_nodes)
    w32(image, o + 148, valid_nodes)
    w32(image, o + 152, next_nid)

def write_nat_entry(image, ino, blk_addr):
    nat_blk = NAT_BLKADDR + ino // (BLOCK_SIZE // NAT_ENTRY_SIZE)
    idx     = ino % (BLOCK_SIZE // NAT_ENTRY_SIZE)
    o = nat_blk * BLOCK_SIZE + idx * NAT_ENTRY_SIZE
    w8 (image, o + 0, 1)
    w32(image, o + 1, ino)
    w32(image, o + 5, blk_addr)

def coreutils_command_names(coreutils_dir):
    """The exact set of commands the coreutils multicall binary answers to.

    Derived from upstream's own feature graph rather than a listing of
    src/uu, so it stays in step with what build-all.sh actually compiled.
    Listing the directory instead would install names the binary cannot
    dispatch (chcon and runcon are SELinux-only and not in the musl set) plus
    checksum_common, which is a shared module rather than a command.

    Must mirror build_coreutils in build-all.sh: it builds
    --features feat_os_unix_musl, which is upstream's musl set and already
    excludes stdbuf (that util needs a cdylib, which a static musl target
    cannot produce).
    """
    import tomllib

    with open(os.path.join(coreutils_dir, "Cargo.toml"), "rb") as f:
        features = tomllib.load(f)["features"]

    visited = set()
    def expand(name):
        if name in visited:
            return
        visited.add(name)
        for entry in features.get(name, []):
            expand(entry.split("/")[0].rstrip("?"))

    expand("feat_os_unix_musl")
    # Intersect with src/uu so the feature-graph bookkeeping drops out and only
    # real command names survive: the feat_* group names, and dependency
    # aliases like uu_test (which backs the `test` feature — that one has to be
    # matched on the feature name, since its leaf is the alias, not `test`).
    uu_dir = os.path.join(coreutils_dir, "src", "uu")
    available = set(os.listdir(uu_dir))
    return sorted(visited & available)


def shadow_hash(salt, password):
    """Match userland/login's verify_password: $sha256$<salt>$<hex>, where
    hex is the lowercase SHA-256 hexdigest of (salt bytes ++ password bytes)."""
    digest = hashlib.sha256(salt.encode('ascii') + password.encode('ascii')).hexdigest()
    return f"$sha256${salt}${digest}"


def main():
    if len(sys.argv) < 3:
        print("Usage: mkfs-f2fs-populated.py <output_img> <arch>")
        sys.exit(1)
        
    out_file = sys.argv[1]
    arch = sys.argv[2]
    
    # 1. Gather files to place in F2FS
    target_arch = "aarch64-unknown-none" if arch == "aarch64" else "x86_64-unknown-none"
    userland_dir = f"userland/target/{target_arch}/release"
    
    bin_files = []
    bins = [
        "shell", "login", "hello", "aplay", "memtest", "vfstest", "f2fstest", "tput",
        "pthreadtest", "timertest", "sigtest", "polltest", "forktest", "racetest",
        "waittest", "sigchldtest", "scmtest", "epolltest", "wakepolltest", "idletest", "drmsmoke", "evtest2", "venustest",
        "mount", "umount", "fstab", "lsblk", "lspci", "lsusb", "ping", "xattr",
        "meminfo",
    ]
    for b in bins:
        p = os.path.join(userland_dir, b)
        if os.path.exists(p):
            bin_files.append((b, p, 0o100755))

    # Static-musl tokio binaries from the S1 spike — K2 acceptance
    # (tokio-echo-selftest regression + the idle-CPU cross-check).
    musl_target = "aarch64-unknown-linux-musl" if arch == "aarch64" else "x86_64-unknown-linux-musl"
    tokio_dir = os.path.expanduser(
        f"~/code/leandros-artifacts/s1-musl-spike/target/{musl_target}/release")
    for tb in ("tokio-echo-selftest", "tokioidle"):
        p = os.path.join(tokio_dir, tb)
        if os.path.exists(p):
            bin_files.append((tb, p, 0o100755))
            
    p = f"../doomgeneric/doom-{arch}"
    if os.path.exists(p):
        bin_files.append(("doom", p, 0o100755))
    p = "../doomgeneric/doom1.wad"
    if os.path.exists(p):
        bin_files.append(("doom1.wad", p, 0o100644))
        
    bottom_target = "aarch64-unknown-linux-musl" if arch == "aarch64" else "x86_64-unknown-linux-musl"
    p = f"../bottom-leandros/target/{bottom_target}/release/btm"
    if os.path.exists(p):
        bin_files.append(("btm", p, 0o100755))
        bin_files.append(("bottom", p, 0o100755))

    p = f"../mame/mame-{arch}"
    if os.path.exists(p):
        bin_files.append(("mame", p, 0o100755))

    brush_target = "aarch64-unknown-linux-musl" if arch == "aarch64" else "x86_64-unknown-linux-musl"
    p = f"../brush/target/{brush_target}/release/brush"
    if os.path.exists(p):
        bin_files.append(("brush", p, 0o100755))

    # uutils/coreutils — cat, ls, cp, mv, rm and friends. One multicall binary
    # that dispatches on argv[0], so every name below is a hardlink to the same
    # inode (add_files_to_dir dedupes by host path); the content is stored once.
    coreutils_target = "aarch64-unknown-linux-musl" if arch == "aarch64" else "x86_64-unknown-linux-musl"
    p = f"../coreutils/target/{coreutils_target}/release/coreutils"
    if os.path.exists(p):
        bin_files.append(("coreutils", p, 0o100755))
        for util in coreutils_command_names("../coreutils"):
            bin_files.append((util, p, 0o100755))


    lib_files = []
    relibc_target = "aarch64-unknown-leandros" if arch == "aarch64" else "x86_64-unknown-leandros"
    p = f"../relibc/target/{relibc_target}/release/librelibc.a"
    if os.path.exists(p):
        lib_files.append(("libc.a", p, 0o100644))
        
    root_files = []
    p = "userland/aplay/car-horn.wav"
    if os.path.exists(p):
        root_files.append(("car-horn.wav", p, 0o100644))

    # MAME ROM sets as zips in / — run with `mame -rompath / <game>`.
    # A zip sidesteps this script's flat directory model (no nested
    # /roms/<game>/ dirs needed); MAME's util/unzip.cpp reads it directly.
    p = "../mame/roms/captcomm.zip"
    if os.path.exists(p):
        root_files.append(("captcomm.zip", p, 0o100644))

    # /etc/fstab — vdb is the root F2FS volume userland/init already mounts
    # via a hardcoded bootstrap call before pivot_root (chicken-and-egg: fstab
    # itself lives on that filesystem); vdc is the second data disk QEMU has
    # always attached but nothing mounted until init started consulting this
    # file for secondary mounts.
    etc_files = [("fstab", (
        b"# <device>   <mountpoint>  <fstype>  <options>  <dump>  <pass>\n"
        b"/dev/vdb     /             f2fs      rw         0       1\n"
        b"/dev/vdc     /data         f2fs      rw         0       2\n"
    ), 0o100644)]

    # /etc/passwd and /etc/group — musl's getpwuid/getgrgid (used by brush via
    # the uzers crate) read these; /bin/login also parses passwd directly.
    etc_files.append(("passwd", (
        b"root:x:0:0:root:/root:/bin/brush\n"
        b"leandro:x:1000:1000:leandro:/home/leandro:/bin/brush\n"
    ), 0o100644))
    etc_files.append(("group", (
        b"root:x:0:\n"
        b"leandro:x:1000:\n"
    ), 0o100644))

    # /etc/shadow — /bin/login's only source of password hashes. Salts are
    # fixed per user so this Python hasher and the Rust verifier in
    # userland/login/src/main.rs (verify_password) agree byte-for-byte.
    shadow_lines = (
        f"root:{shadow_hash('lnd0', 'root')}:\n"
        f"leandro:{shadow_hash('lnd0', 'leandro')}:\n"
    )
    etc_files.append(("shadow", shadow_lines.encode('ascii'), 0o100600))

    # ── K3 dynamic-linking corpus (musl ld.so + test ladder) ──────────────────
    # Real dynamic musl world built host-side (see the K3 NOTES.md). ld-musl IS
    # libc.so: the SAME host file is packed at both /lib/ld-musl-<arch>.so.1 and
    # /usr/lib/libc.so — add_files_to_dir dedupes by host path, so these become
    # hardlinks to one inode and the kernel needs no symlink resolution to load
    # the interpreter. The test ladder binaries are dynamic-PIE ELFs whose
    # PT_INTERP points at /lib/ld-musl-<arch>.so.1.
    dyn_root = os.path.expanduser("~/code/leandros-artifacts/musl-dynamic")
    libc_so = f"{dyn_root}/sysroot/{arch}/usr/lib/libc.so"
    usr_lib_files = []
    if os.path.exists(libc_so):
        lib_files.append((f"ld-musl-{arch}.so.1", libc_so, 0o100755))
        usr_lib_files.append(("libc.so", libc_so, 0o100755))
    for name, rel in (
        ("hello-dyn",    f"test/hello-dyn/hello-dyn-{arch}"),
        ("hello-dyn-rs", f"test/hello-dyn-rs/hello-dyn-rs-{arch}"),
        ("dlopen-host",  f"test/dlopen-host/dlopen-host-{arch}"),
    ):
        p = f"{dyn_root}/{rel}"
        if os.path.exists(p):
            bin_files.append((name, p, 0o100755))
    # plugin.so is dlopen("./plugin.so")'d by dlopen-host: place it both in /bin
    # (next to dlopen-host) and in / so a relative open resolves from either cwd.
    plugin = f"{dyn_root}/test/dlopen-host/plugin-{arch}.so"
    if os.path.exists(plugin):
        bin_files.append(("plugin.so", plugin, 0o100755))
        root_files.append(("plugin.so", plugin, 0o100755))

    # ── K4 GL ship set (kmscube, M3) ──────────────────────────────────────────
    # Runtime libraries for the kms_swrast GBM + GLES2 path, packed under their
    # SONAMEs into /usr/lib (the loader resolves DT_NEEDED by soname, so no
    # symlink support is needed — same trick as ld-musl above). The soname paths
    # in the merged sysroot are symlinks that Python follows to the real
    # versioned .so, so the content is stored once under the soname name.
    gl_root = os.path.expanduser("~/code/leandros-artifacts/m3-gl-stack")
    gl_lib_dir = f"{gl_root}/sysroot-{arch}/usr/lib"
    gbm_files = []
    for so in ("libEGL.so.1", "libGLESv2.so.2", "libgbm.so.1", "libdrm.so.2",
               "libgallium-25.3.6.so", "libexpat.so.1", "libz.so.1",
               "libwayland-client.so.0", "libwayland-server.so.0",
               # libwayland-egl.so.1 is dlopen()ed at runtime by wayland-sys
               # (wayland-egl.rs egl.rs:25 tries "libwayland-egl.so.1" then
               # ".so") the moment a client creates a WlEglSurface. cosmic-panel
               # is the first client to use client-side Wayland-EGL; without it
               # the panel panics ("Library libwayland-egl.so could not be
               # loaded.") right after "Waiting for configure event".
               "libwayland-egl.so.1", "libffi.so.8",
               # libpam.so.0 is the LeandrOS shadow-auth shim (source:
               # m6-session-bins/src/libpam-shim), DT_NEEDED by cosmic-greeter's
               # in-session locker; verifies $sha256$ /etc/shadow like /bin/login.
               "libpam.so.0"):
        p = f"{gl_lib_dir}/{so}"
        if os.path.exists(p):
            usr_lib_files.append((so, p, 0o100755))
    # GBM backend, dlopened by absolute path /usr/lib/gbm/dri_gbm.so.
    dri = f"{gl_lib_dir}/gbm/dri_gbm.so"
    if os.path.exists(dri):
        gbm_files.append(("dri_gbm.so", dri, 0o100755))
    # kmscube itself (dynamic ET_DYN, PT_INTERP=/lib/ld-musl-<arch>.so.1).
    kc = f"{gl_root}/out/kmscube-{arch}"
    if os.path.exists(kc):
        bin_files.append(("kmscube", kc, 0o100755))

    # ── M4 input/XKB ship set (anvil compositor) ──────────────────────────────
    # anvil (ET_DYN, PT_INTERP=/lib/ld-musl-<arch>.so.1) + the wl_shm/xdg_shell
    # test client, plus anvil's input stack. Input libraries are packed under
    # their SONAME into /usr/lib (same soname trick as the GL set: the loader
    # resolves DT_NEEDED by soname and open() follows the staged soname symlink
    # to the real versioned file). The XKB keymap data + libinput quirks form a
    # deep file tree under /usr/share, enumerated here for size accounting and
    # packed recursively further below.
    m4_root   = os.path.expanduser(f"~/code/leandros-artifacts/m4-input-ship/{arch}")
    anvil_bin = os.path.expanduser(f"~/code/leandros-artifacts/m3-gl-stack/out/anvil-{arch}")
    wlclient  = os.path.expanduser(f"~/code/leandros-artifacts/m4-client/wlclient-{arch}")
    if os.path.exists(anvil_bin):
        bin_files.append(("anvil", anvil_bin, 0o100755))
    if os.path.exists(wlclient):
        bin_files.append(("wlclient", wlclient, 0o100755))
    m4_lib_dir = f"{m4_root}/usr/lib"
    # libseat.so.1 / libudev.so.1 are the two shims whose C source is tracked
    # (ports/input-stack/shims) and built by ports/input-stack/build-shims.sh,
    # wired into the normal build via scripts/build-all.sh. Prefer that fresh
    # output over the m4-input-ship blob so a tracked-source edit can never
    # again be silently absent from the image; fall back to the blob if the
    # shim wasn't rebuilt this run (e.g. zig unavailable). The other libraries
    # here (libxkbcommon, libinput, ...) are upstream C, not built from this
    # repo, and always come from the blob.
    shim_build_dir = f"target/input-stack-sysroot/{arch}/usr/lib"
    for so in ("libxkbcommon.so.0", "libdisplay-info.so.3", "libseat.so.1",
               "libudev.so.1", "libinput.so.10", "libpixman-1.so.0",
               "libmtdev.so.1", "libevdev.so.2"):
        built = f"{shim_build_dir}/{so}"
        p = built if os.path.exists(built) else f"{m4_lib_dir}/{so}"
        if os.path.exists(p):
            usr_lib_files.append((so, p, 0o100755))
    # Recursively enumerate the /usr/share data tree. m4_share_files holds
    # (image_dir_abspath, name, hostpath); m4_share_dirs holds every directory
    # (and its ancestors down to /usr/share) that must be created. Directory
    # inodes are registered into dir_nodes/subdirs after those are defined; the
    # files are packed after the other add_files_to_dir calls. No symlinks exist
    # in the staged tree (they were dereferenced when staged), so a plain walk
    # yields only regular files.
    m4_share_files = []
    m4_share_dirs  = set()
    m4_share_src   = f"{m4_root}/usr/share"
    if os.path.isdir(m4_share_src):
        for dirpath, _dirnames, filenames in os.walk(m4_share_src):
            rel = os.path.relpath(dirpath, m4_root)   # e.g. usr/share/X11/xkb/symbols
            parts = rel.split("/")
            for i in range(2, len(parts) + 1):        # register usr/share ... down (usr already exists)
                m4_share_dirs.add("/" + "/".join(parts[:i]))
            image_dir = "/" + rel
            for fn in sorted(filenames):
                hp = os.path.join(dirpath, fn)
                if os.path.isfile(hp):
                    m4_share_files.append((image_dir, fn, hp))

    # ── Synthetic sysfs for the DRM nodes ────────────────────────────────────
    #
    # This tree used to be four empty directories, enough only for anvil's
    # drm-rs (DrmNode::from_path -> is_device_drm() stat()s
    # /sys/dev/char/<maj>:<min>/device/drm, and node_with_type(Primary)
    # read_dir()s it for a "card0" entry). Mesa's Venus Vulkan ICD needs much
    # more: virtgpu_open() (mesa/src/virtio/vulkan/vn_renderer_virtgpu.c:1677)
    # calls drmGetDevices2(), which classifies a node ENTIRELY from sysfs and
    # drops any node it cannot classify. The exact contract, read off libdrm
    # 2.4.134's xf86drm.c (the version ports/mesa/build-libdrm.sh builds), for
    # each /dev/dri/<name> that readdir(3) turns up:
    #
    #   1. stat("/sys/dev/char/<maj>:<min>/device/drm")            [drmNodeIsDRM, :3324]
    #      must succeed, or process_device() rejects the node outright.
    #   2. readlink("/sys/dev/char/<maj>:<min>/device/subsystem")  [get_subsystem_type, :3577]
    #      must succeed; libdrm takes strrchr(target,'/') and matches it against
    #      "/pci", "/usb", "/platform", "/virtio", ... A failed readlink yields a
    #      negative subsystem type, which process_device()'s switch (:4558) sends
    #      to `default: return -1`. THIS IS THE ONE HARD SYMLINK REQUIREMENT —
    #      no regular file will do. The target is never resolved, only string-
    #      matched, so it may dangle (we point it at a real dir anyway).
    #   3. realpath("/sys/dev/char/<maj>:<min>/device") -> pci_path             [get_pci_path, :3651]
    #      then, if pci_path's last component starts with "/virtio", strip it.
    #      Because `device` here is a real directory rather than Linux's symlink
    #      into /sys/devices/..., realpath() returns the path unchanged and the
    #      "/virtio" strip is a no-op — i.e. we present the shape of a plain PCI
    #      GPU, which is libdrm's primary path and needs no virtio parent walk.
    #      (musl 1.2.5's realpath is a pure userspace readlink loop and treats a
    #      non-symlink component's EINVAL as "not a link", which is exactly what
    #      the f2fs server returns — servers/f2fs/src/lib.rs:2387.)
    #   4. fopen(pci_path + "/uevent") and find a "PCI_SLOT_NAME=" line, parsed
    #      with "%04x:%02x:%02x.%1u".        [sysfs_uevent_get :3536, drmParsePciBusInfo :3725]
    #   5. fopen(pci_path + "/{vendor,device,subsystem_vendor,subsystem_device}"),
    #      each read with fscanf("%x"); "revision" is read first only when the
    #      caller passes DRM_DEVICE_GET_PCI_REVISION.   [parse_separate_sysfs_files :3836]
    #      If that whole step fails, libdrm falls back to reading 64 raw bytes
    #      from pci_path + "/config".                   [parse_config_sysfs_file :3876]
    #
    # Venus calls drmGetDevices2(0, ...) — no revision flag — and then requires
    # vendor_id==0x1af4 && device_id==0x1050 and available_nodes & (1<<
    # DRM_NODE_RENDER) (vn_renderer_virtgpu.c:1598-1611). The render bit is why
    # both 226:0 and 226:128 get a full attribute set with IDENTICAL contents:
    # drmFoldDuplicatedDevices (:4589) merges two nodes into one device iff
    # their drmPciBusInfo compare equal, and only then does the surviving device
    # carry both the primary and the render node.
    DRM_MAJOR = 226
    # The virtio-gpu PCI identity. vendor/device/class are what the guest's own
    # scan reports (drivers/src/pci.rs::scan prints 0x1AF4:0x1050; class_rev's
    # top byte is 0x03 = display, subclass 0x80 = other) and are fixed
    # properties of QEMU's virtio-gpu-pci / virtio-gpu-gl-pci — the two differ
    # only in whether virgl is enabled, never in PCI ids.
    PCI_VENDOR      = 0x1af4
    PCI_DEVICE      = 0x1050   # 0x1040 + 16 (VIRTIO_ID_GPU), i.e. modern virtio
    PCI_REVISION    = 0x01     # QEMU stamps revision 1 on every modern-only virtio device
    PCI_SUBVENDOR   = 0x1af4
    PCI_SUBDEVICE   = 0x1100   # PCI_SUBDEVICE_ID_QEMU
    PCI_CLASS       = 0x038000 # display / other / prog-if 0
    # Bus address. The image is built before the guest enumerates anything, so
    # this has to be baked; it is the slot QEMU actually assigns, with the GPU
    # the 4th -device on the bus after drive0/data0/data1 (scripts/run-qemu.sh
    # and .claude/skills/run-leandros/driver.py agree on that order for both
    # machines: aarch64 "virt" puts the host bridge at 00:00.0, q35 puts the MCH
    # there and its ICH9 functions at 00:1f.x, so either way the four -device
    # entries land on slots 1..4). Nothing in this stack makes a decision from
    # the value — libdrm only parses it into drmPciBusInfo, and its one load-
    # bearing property is that BOTH nodes report the same string so
    # drmFoldDuplicatedDevices merges them.
    PCI_SLOT_NAME = "0000:00:04.0"

    # sysfs_files carries (image_dir, name, content_bytes, mode); it is folded
    # into the same per-inode packing table as the /usr/share tree below. The
    # content is inline bytes rather than a host path, which add_files_to_dir
    # already understands (it skips the hardlink dedup for those).
    sysfs_files = []

    def _sysfs_attr(image_dir, name, content, mode=0o100444):
        data = content.encode('ascii') if isinstance(content, str) else content
        sysfs_files.append((image_dir, name, data, mode))

    # A 64-byte slice of PCI configuration space, laid out per the PCI spec at
    # exactly the offsets libdrm's parse_config_sysfs_file reads: vendor 0x00,
    # device 0x02, revision 0x08, class triple 0x09..0x0b, subsystem 0x2c/0x2e.
    # Only reached if the individual attribute files above ever fail to open.
    _cfg = bytearray(64)
    struct.pack_into('<HH', _cfg, 0x00, PCI_VENDOR, PCI_DEVICE)
    _cfg[0x08] = PCI_REVISION
    _cfg[0x09] = PCI_CLASS & 0xFF          # prog-if
    _cfg[0x0a] = (PCI_CLASS >> 8) & 0xFF   # subclass
    _cfg[0x0b] = (PCI_CLASS >> 16) & 0xFF  # base class
    struct.pack_into('<HH', _cfg, 0x2c, PCI_SUBVENDOR, PCI_SUBDEVICE)

    _uevent = (
        "DRIVER=virtio-pci\n"
        f"PCI_CLASS={PCI_CLASS:X}\n"
        f"PCI_ID={PCI_VENDOR:04X}:{PCI_DEVICE:04X}\n"
        f"PCI_SUBSYS_ID={PCI_SUBVENDOR:04X}:{PCI_SUBDEVICE:04X}\n"
        f"PCI_SLOT_NAME={PCI_SLOT_NAME}\n"
        f"MODALIAS=pci:v{PCI_VENDOR:08X}d{PCI_DEVICE:08X}"
        f"sv{PCI_SUBVENDOR:08X}sd{PCI_SUBDEVICE:08X}"
        f"bc{(PCI_CLASS >> 16) & 0xFF:02X}sc{(PCI_CLASS >> 8) & 0xFF:02X}"
        f"i{PCI_CLASS & 0xFF:02X}\n"
    )

    # The bus directory the "subsystem" links point at. Only its name matters to
    # libdrm (it string-matches the final component), but a link that resolves
    # is cheaper to reason about than one that dangles.
    for d in ("/sys", "/sys/bus", "/sys/bus/pci", "/sys/bus/pci/devices"):
        m4_share_dirs.add(d)

    for _minor, _node in ((0, "card0"), (128, "renderD128")):
        _base = f"/sys/dev/char/{DRM_MAJOR}:{_minor}"
        _dev  = _base + "/device"
        # `device/drm/<node>` holds only THIS node's directory. Linux would list
        # both siblings there, but drm-rs's node_with_type() enumerates that
        # directory to find a peer node, and the compositor path that uses it is
        # the most fragile thing in the tree — listing only the node that owns
        # the char device keeps card0's discovery byte-identical to before.
        for d in ("/sys/dev", "/sys/dev/char", _base, _dev,
                  _dev + "/drm", _dev + "/drm/" + _node):
            m4_share_dirs.add(d)
        # `../../../../bus/pci`: from /sys/dev/char/<maj>:<min>/device, four
        # levels up is /sys. Only the trailing "/pci" is ever inspected.
        _sysfs_attr(_dev, "subsystem", "../../../../bus/pci", 0o120777)
        _sysfs_attr(_dev, "uevent",           _uevent)
        _sysfs_attr(_dev, "vendor",           f"0x{PCI_VENDOR:04x}\n")
        _sysfs_attr(_dev, "device",           f"0x{PCI_DEVICE:04x}\n")
        _sysfs_attr(_dev, "subsystem_vendor", f"0x{PCI_SUBVENDOR:04x}\n")
        _sysfs_attr(_dev, "subsystem_device", f"0x{PCI_SUBDEVICE:04x}\n")
        _sysfs_attr(_dev, "revision",         f"0x{PCI_REVISION:02x}\n")
        _sysfs_attr(_dev, "class",            f"0x{PCI_CLASS:06x}\n")
        _sysfs_attr(_dev, "config",           bytes(_cfg))

    # ── M2 Venus ship set (Mesa's virtio Vulkan ICD + the vktest smoke test) ──
    # Sourced from the venus-lane stage dir directly rather than by copying into
    # the m3-gl-stack sysroot. Every ship set above already reads from the stage
    # dir of the lane that produced it (m3-gl-stack, m4-input-ship, m4-client,
    # m5-session-*), so a venus_root of its own is the established shape; folding
    # these into another lane's sysroot would mutate an artifact tree this script
    # does not own and lose the file whenever that sysroot is rebuilt.
    #
    # libvulkan_virtio.so's DT_NEEDED closure is libz.so.1, libdrm.so.2,
    # libwayland-client.so.0, libexpat.so.1 (all in the GL loop above),
    # libdisplay-info.so.3 (the M4 input loop) and libc.so — i.e. nothing new to
    # stage. There is deliberately NO Khronos loader on LeandrOS, so the ICD is
    # never resolved through DT_NEEDED; vktest dlopen()s it by absolute path and
    # the JSON below exists so anything that does read an ICD manifest finds the
    # same library_path.
    venus_root = os.path.expanduser(f"~/code/leandros-artifacts/venus-lane/stage-{arch}")
    _icd_so = f"{venus_root}/usr/lib/libvulkan_virtio.so"
    if os.path.exists(_icd_so):
        usr_lib_files.append(("libvulkan_virtio.so", _icd_so, 0o100755))
    _vktest = f"{venus_root}/usr/bin/vktest"
    if os.path.exists(_vktest):
        bin_files.append(("vktest", _vktest, 0o100755))
    _vkrender = f"{venus_root}/usr/bin/vkrender"
    if os.path.exists(_vkrender):
        bin_files.append(("vkrender", _vkrender, 0o100755))
    # vkswap — the VK_EXT_headless_surface swapchain test. Same staging shape
    # as vkrender; absent from the artifact tree it is simply not packed.
    _vkswap = f"{venus_root}/usr/bin/vkswap"
    if os.path.exists(_vkswap):
        bin_files.append(("vkswap", _vkswap, 0o100755))
    # vkwl — M4: the same swapchain, but on a real Wayland surface handed to
    # cosmic-comp instead of a headless one. Its DT_NEEDED closure is libc.so
    # plus libwayland-client.so.0, both already packed by the GL loop above,
    # because it is built with the m3-gl-stack musl toolchain that produced
    # wlclient rather than in the Alpine container the rest of venus-lane uses.
    _vkwl = f"{venus_root}/usr/bin/vkwl"
    if os.path.exists(_vkwl):
        bin_files.append(("vkwl", _vkwl, 0o100755))
    # The ICD manifest rides the shared /usr/share tree machinery. EVERY ancestor
    # has to be in m4_share_dirs — /usr is static (ino 15) but /usr/share only
    # exists because the M4 walk happens to create it, which is not a dependency
    # worth having.
    _icd_json = f"{venus_root}/usr/share/vulkan/icd.d/virtio_icd.{arch}.json"
    if os.path.exists(_icd_json):
        for d in ("/usr/share", "/usr/share/vulkan", "/usr/share/vulkan/icd.d"):
            m4_share_dirs.add(d)
        m4_share_files.append(("/usr/share/vulkan/icd.d",
                               f"virtio_icd.{arch}.json", _icd_json))

    # ── M5 session/compositor ship set (cosmic-comp + D-Bus session + fonts) ──
    # cosmic-comp (ET_DYN, PT_INTERP=/lib/ld-musl-<arch>.so.1) reuses the exact
    # M3 GL + M4 input ship sets already packed above; its DT_NEEDED closure is
    # fully covered there (verified: m5-session-ship/verify-closure.sh), so no
    # new libraries are needed — only the compositor binary, the session bus
    # (busd + its POSIX-sh launcher + config) and the default UI fonts.
    #
    # The bus binary must be executable — dbus-run-session gates on `test -x`
    # $BUSD_BIN and busd is exec'd directly (static ET_EXEC) — so busd and
    # dbus-run-session are packed 0755 via m5_exec_files (resolved to their
    # target-dir inode after the /usr tree registration below). session.conf
    # and the fonts are plain data (0644) and ride the shared /usr tree walk
    # (m4_share_files/m4_share_dirs, generalized here beyond /usr/share).
    m5_arch_root = os.path.expanduser(f"~/code/leandros-artifacts/m5-session-ship/{arch}")
    m5_fonts_src = os.path.expanduser("~/code/leandros-artifacts/m5-session-ship/share/fonts")
    cosmic_comp  = os.path.expanduser(f"~/code/leandros-artifacts/m3-gl-stack/out/cosmic-comp-{arch}")
    if os.path.exists(cosmic_comp):
        bin_files.append(("cosmic-comp", cosmic_comp, 0o100755))
    # (image_dir_abspath, name, hostpath) packed 0755 after inode registration.
    m5_exec_files = []
    if os.path.isdir(m5_arch_root):
        for dirpath, _dn, filenames in os.walk(m5_arch_root):
            rel = os.path.relpath(dirpath, m5_arch_root)   # e.g. "usr/libexec"
            if rel == ".":
                continue
            parts = rel.split("/")
            for i in range(1, len(parts) + 1):             # register /usr, /usr/bin, ...
                m4_share_dirs.add("/" + "/".join(parts[:i]))
            image_dir = "/" + rel
            for fn in sorted(filenames):
                hp = os.path.join(dirpath, fn)
                if not os.path.isfile(hp):
                    continue
                if fn in ("busd", "dbus-run-session"):     # must be executable
                    m5_exec_files.append((image_dir, fn, hp))
                else:                                       # session.conf etc. = data
                    m4_share_files.append((image_dir, fn, hp))
    # Default UI fonts -> /usr/share/fonts/<family>/… — the exact scan dir
    # fontdb's load_no_fontconfig() walks on LeandrOS (no fontconfig config
    # present). Zero fonts is a soft-fail (blank text, no panic); packing them
    # makes the compositor UI legible. Plain data (0644).
    if os.path.isdir(m5_fonts_src):
        for dirpath, _dn, filenames in os.walk(m5_fonts_src):
            rel = os.path.relpath(dirpath, m5_fonts_src)   # e.g. "open-sans" or "."
            sub = "" if rel == "." else "/" + rel
            image_dir = "/usr/share/fonts" + sub
            parts = ("usr/share/fonts" + sub).split("/")
            for i in range(2, len(parts) + 1):
                m4_share_dirs.add("/" + "/".join(parts[:i]))
            for fn in sorted(filenames):
                hp = os.path.join(dirpath, fn)
                if os.path.isfile(hp):
                    m4_share_files.append((image_dir, fn, hp))

    # ── M6 full COSMIC session ship set ───────────────────────────────────────
    # The desktop session (cosmic-session + its fatal-at-spawn set and tolerant
    # applets) on top of the M5 compositor/bus/font base. launch-pad spawns every
    # child by BARE NAME via PATH, so all session binaries install into /bin
    # (which is on the launcher's PATH) under the EXACT name cosmic-session execs
    # them by — note cosmic-session spawns "cosmic-app-library" but the built
    # binary file is named cosmic-applibrary (main.rs:330).
    #
    # cosmic-settings-daemon is one of the four fatal-at-spawn children (a missing
    # binary panics the whole session); it links libpipewire-0.3.so.0, satisfied
    # by the inert stub staged into /usr/lib. All eight libcosmic binaries reuse
    # the M3 GL + M4 input closures already packed (Wayland/EGL are dlopen'd, not
    # DT_NEEDED); only libudev (cosmic-settings) and the pipewire stub are new,
    # both already present. No source patches — feature flags only (see manifest).
    m6_out = os.path.expanduser("~/code/leandros-artifacts/m6-session-bins/out")
    pw_out = os.path.expanduser("~/code/leandros-artifacts/pipewire-gap/out")
    m6_session_bins = [
        ("cosmic-session",         f"{m6_out}/cosmic-session-{arch}"),
        ("cosmic-panel",           f"{m6_out}/cosmic-panel-{arch}"),
        ("cosmic-notifications",   f"{m6_out}/cosmic-notifications-{arch}"),
        ("cosmic-bg",              f"{m6_out}/cosmic-bg-{arch}"),
        ("cosmic-osd",             f"{m6_out}/cosmic-osd-{arch}"),
        ("cosmic-launcher",        f"{m6_out}/cosmic-launcher-{arch}"),
        ("cosmic-app-library",     f"{m6_out}/cosmic-applibrary-{arch}"),  # spawn name != file name
        ("cosmic-settings",        f"{m6_out}/cosmic-settings-{arch}"),
        ("cosmic-settings-daemon", f"{pw_out}/cosmic-settings-daemon-{arch}"),
        # M7z tolerant-children completion: the last four names cosmic-session
        # spawns (main.rs:335-351). workspaces is built --no-default-features
        # --features force-shm-screencopy (no wgpu, wl_shm capture only);
        # files-applet drops the gvfs feature (no glib/gio on the image); idle
        # is featureless; greeter is --no-default-features + the locker patch
        # in ports/cosmic-greeter (idles instead of locking at boot — LeandrOS
        # has no logind lock trigger) and links the libpam shadow-auth shim.
        ("cosmic-workspaces",      f"{m6_out}/cosmic-workspaces-{arch}"),
        ("cosmic-greeter",         f"{m6_out}/cosmic-greeter-{arch}"),
        ("cosmic-files-applet",    f"{m6_out}/cosmic-files-applet-{arch}"),
        ("cosmic-idle",            f"{m6_out}/cosmic-idle-{arch}"),
    ]
    for name, src in m6_session_bins:
        if os.path.exists(src):
            bin_files.append((name, src, 0o100755))

    # leandros-applet — a minimal dependency-free wl_shm xdg_toplevel panel applet.
    # cosmic-panel refuses to render its bar with no applet content (render()
    # early-returns while actual_size<=20), and the real cosmic applets pull
    # tokio+zbus+system services (timedate1/logind/upower) absent on LeandrOS. This
    # tiny client draws one solid block so the panel has real content and commits
    # frame 0. It is spawned by cosmic-panel via the desktop file staged into
    # /usr/share/applications (m6-session-data/shared) whose stem matches the panel
    # config's center applet name (com.system76.CosmicAppletTime). Pure-Rust wayland
    # backend => only ld-musl at runtime.
    m7w_applet = os.path.expanduser(
        f"~/code/leandros-artifacts/m7w-applet/out/leandros-applet-{arch}")
    if os.path.exists(m7w_applet):
        bin_files.append(("leandros-applet", m7w_applet, 0o100755))

    # wl-globals — M9 Stage 0a instrument. Dumps the wl_registry of EVERY
    # wayland-* socket in $XDG_RUNTIME_DIR and exits, ignoring the environment
    # (so it reaches cosmic-comp, not the panel's embedded server that
    # leandros-applet is handed via WAYLAND_SOCKET). Measurement only; nothing
    # in the session depends on it.
    m9_wlglobals = os.path.expanduser(
        f"~/code/leandros-artifacts/m9-wlglobals/out/wl-globals-{arch}")
    if os.path.exists(m9_wlglobals):
        bin_files.append(("wl-globals", m9_wlglobals, 0o100755))

    # liprobe: runs libinput directly, with its log priority raised to DEBUG, so
    # the input path can be observed without patching COSMIC. It is what showed
    # libinput itself produces events (motion_abs, key, dispatch_err=0) while the
    # compositor acts on none of them, narrowing the break to smithay's drain or
    # cosmic-comp's routing. Source is in-repo at artifacts/m13-liprobe/.
    m13_liprobe = os.path.expanduser(
        f"~/code/leandros-artifacts/m13-liprobe/out/liprobe-{arch}")
    if os.path.exists(m13_liprobe):
        bin_files.append(("liprobe", m13_liprobe, 0o100755))

    # wlinput: a Wayland client that maps a real xdg_toplevel and counts every
    # wl_pointer / wl_keyboard / wl_touch event the compositor sends it. liprobe
    # instruments the layer BELOW cosmic-comp; this one instruments the layer
    # ABOVE it, so between them the compositor is bracketed and "cosmic-comp
    # never received input" and "cosmic-comp received it and routed it nowhere"
    # stop looking the same. Source is in-repo at artifacts/m14-wlinput/.
    m14_wlinput = os.path.expanduser(
        f"~/code/leandros-artifacts/m14-wlinput/out/wlinput-{arch}")
    if os.path.exists(m14_wlinput):
        bin_files.append(("wlinput", m14_wlinput, 0o100755))

    # The session launcher itself (a POSIX-sh script). The kernel execve()s ELF
    # only (no "#!"-shebang binfmt), so it is run as `sh /bin/start-cosmic-leandros`.
    m6_launcher = os.path.expanduser(
        "~/code/leandros-artifacts/m6-session-data/start-cosmic-leandros")
    if os.path.exists(m6_launcher):
        bin_files.append(("start-cosmic-leandros", m6_launcher, 0o100755))
    # m4-vkwl — the M4 driver: backgrounds start-cosmic-leandros with its log
    # redirected to a file, waits for the wayland-1 socket, then runs vkwl
    # against it. Same no-shebang rule: run as `brush /bin/m4-vkwl`.
    m4_drv = os.path.expanduser(
        "~/code/leandros-artifacts/m6-session-data/m4-vkwl")
    if os.path.exists(m4_drv):
        bin_files.append(("m4-vkwl", m4_drv, 0o100755))
    # m12-caps — the capability probe: brings the session up the same way and
    # then runs a fixed choreography (idle, pointer, click, three keybindings,
    # wl-globals, one window, two windows, move/resize/close, an application)
    # announcing each window as "M12: MARK <name> <secs>" so artifacts/
    # m12_caps.py can inject QMP input and photograph the scanout inside it.
    # Same no-shebang rule: run as `brush /bin/m12-caps`.
    m12_drv = os.path.expanduser(
        "~/code/leandros-artifacts/m6-session-data/m12-caps")
    if os.path.exists(m12_drv):
        bin_files.append(("m12-caps", m12_drv, 0o100755))
    # m12c-input — the follow-up that attributes m12-caps input null result to
    # a layer: /dev/input and /sys/class/input against what the libudev shim
    # claims, then evtest2 (raw evdev, no libinput in the path), then the same
    # session with RUST_LOG turned up on the smithay input backends.
    m12c_drv = os.path.expanduser(
        "~/code/leandros-artifacts/m6-session-data/m12c-input")
    if os.path.exists(m12c_drv):
        bin_files.append(("m12c-input", m12c_drv, 0o100755))
    # m14-input — the guest half of artifacts/m14_input.py: brings the session
    # up, dumps the COSMIC input config (an `input_devices` entry with
    # `state: Disabled` produces exactly the observed symptom), then runs
    # /bin/wlinput against cosmic-comp's socket so the same injection can be
    # counted BELOW the compositor ([EVSTAT]) and ABOVE it ([WLI]) in one run.
    # Same no-shebang rule: run as `brush /bin/m14-input`.
    m14_drv = os.path.expanduser(
        "~/code/leandros-artifacts/m6-session-data/m14-input")
    if os.path.exists(m14_drv):
        bin_files.append(("m14-input", m14_drv, 0o100755))
    # m15-iced — the guest half of artifacts/m15_iced.py. Brings the session up
    # with its output silenced to a file, then launches a raw wl_shm control and
    # cosmic-settings SIDE BY SIDE outside cosmic-session, so the toolkit app's
    # own stderr survives (launch_pad pipes child stderr and registers no
    # on_stderr handler, cosmic-session/src/comp.rs:122-134) and WAYLAND_DEBUG
    # can say whether it never commits or commits blank buffers.
    # Same no-shebang rule: run as `brush /bin/m15-iced`.
    m15_drv = os.path.expanduser(
        "~/code/leandros-artifacts/m6-session-data/m15-iced")
    if os.path.exists(m15_drv):
        bin_files.append(("m15-iced", m15_drv, 0o100755))
    # m15b-iced — the discriminator m15-iced pointed at: the same cosmic-settings
    # binary run twice one environment variable apart, COSMIC_SINGLE_INSTANCE
    # unset then =false, to separate "blocked in libcosmic's blocking D-Bus
    # single-instance probe" from "reaches iced and renders nothing".
    m15b_drv = os.path.expanduser(
        "~/code/leandros-artifacts/m6-session-data/m15b-iced")
    if os.path.exists(m15b_drv):
        bin_files.append(("m15b-iced", m15b_drv, 0o100755))
    # m17-census — the guest half of artifacts/m17_census.py. Brings the session
    # up the normal way but starts busd ITSELF, so that busd's stderr is a file
    # of its own: `busd::peers: unknown destination: <name>` is the census, and
    # dbus-run-session would otherwise hand busd the same fd every other
    # component writes to, where two writers keep independent offsets and
    # overwrite each other. Then re-runs each autostarted single-instance
    # component by hand, each with its own stderr, because the stderr byte count
    # separates "blocked in the D-Bus probe" from "ran".
    m17_drv = os.path.expanduser(
        "~/code/leandros-artifacts/m6-session-data/m17-census")
    if os.path.exists(m17_drv):
        bin_files.append(("m17-census", m17_drv, 0o100755))

    # m4-vkwl-a64 — the same driver with the compositor choice and every wait
    # made an argument, for aarch64 under TCG where "the session never came up"
    # and "the WSI chain does not work here" have to be told apart and a silent
    # timeout tells you neither. Staged exactly like m4-vkwl above; absent from
    # the artifact tree it is simply not packed.
    m4_drv_a64 = os.path.expanduser(
        "~/code/leandros-artifacts/m6-session-data/m4-vkwl-a64")
    if os.path.exists(m4_drv_a64):
        bin_files.append(("m4-vkwl-a64", m4_drv_a64, 0o100755))

    # /bin/sh -> brush (hardlinked; add_files_to_dir dedupes by host path). The
    # kernel has no shebang binfmt, so shell scripts (start-cosmic-leandros,
    # dbus-run-session) are executed as `sh <script>`; the proposed
    # dbus-run-session also uses `sh -c ...`. brush, invoked as sh, interprets the
    # script argument directly — no shebang or ENOEXEC-fallback dependency.
    _brush_p = f"../brush/target/{brush_target}/release/brush"
    if os.path.exists(_brush_p):
        bin_files.append(("sh", _brush_p, 0o100755))

    # libpipewire-0.3 stub (inert) -> /usr/lib, resolved by soname for the
    # settings-daemon's DT_NEEDED. Same soname trick as the GL/input libs.
    m6_pw_lib = os.path.expanduser(f"~/code/leandros-artifacts/pipewire-gap/lib/{arch}")
    for so in ("libpipewire-0.3.so.0",):
        sp = f"{m6_pw_lib}/{so}"
        if os.path.exists(sp):
            usr_lib_files.append((so, sp, 0o100755))

    # Cosmic icon theme -> /usr/share/icons/Cosmic/… (pruned set) and the default
    # wallpaper -> the exact hardcoded fallback path cosmic-bg expects
    # (/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg). Both are
    # soft-fail data (blank icons / black background if absent, never a crash);
    # they ride the shared /usr/share tree walk (m4_share_files/m4_share_dirs).
    m6_icons_src = os.path.expanduser("~/code/leandros-artifacts/m6-icons-pruned/share")
    if os.path.isdir(m6_icons_src):
        for dirpath, _dn, filenames in os.walk(m6_icons_src):
            rel = os.path.relpath(dirpath, m6_icons_src)   # e.g. "icons/Cosmic/scalable"
            sub = "" if rel == "." else "/" + rel
            image_dir = "/usr/share" + sub
            parts = ("usr/share" + sub).split("/")
            for i in range(2, len(parts) + 1):
                m4_share_dirs.add("/" + "/".join(parts[:i]))
            for fn in sorted(filenames):
                hp = os.path.join(dirpath, fn)
                if os.path.isfile(hp):
                    m4_share_files.append((image_dir, fn, hp))
    m6_shared_src = os.path.expanduser("~/code/leandros-artifacts/m6-session-data/shared")
    if os.path.isdir(m6_shared_src):
        for dirpath, _dn, filenames in os.walk(m6_shared_src):
            rel = os.path.relpath(dirpath, m6_shared_src)  # e.g. "usr/share/backgrounds/cosmic"
            if rel == ".":
                continue
            parts = rel.split("/")
            for i in range(2, len(parts) + 1):
                m4_share_dirs.add("/" + "/".join(parts[:i]))
            image_dir = "/" + rel
            for fn in sorted(filenames):
                if fn.endswith(".orig"):          # keep the placeholder off-image
                    continue
                hp = os.path.join(dirpath, fn)
                if os.path.isfile(hp):
                    m4_share_files.append((image_dir, fn, hp))

    # COSMIC system-default config tree -> /usr/share/cosmic/<component>/v<N>/<key>
    #
    # cosmic-config resolves a component's system defaults as
    #     system_path = xdg::BaseDirectories::with_prefix("cosmic")
    #                       .find_data_file("<name>/v<version>")
    # (libcosmic cosmic-config/src/lib.rs:203,236). find_data_file walks
    # XDG_DATA_HOME then XDG_DATA_DIRS and returns the first path that EXISTS,
    # so what it is really testing for is the DIRECTORY
    # /usr/share/cosmic/<name>/v<N>/. Each config key inside it is a BARE,
    # EXTENSIONLESS FILE whose entire content is one RON value: get_system_default
    # is `system_path.join(key)` -> read_to_string -> ron::from_str (:481-487).
    # If the directory is missing, system_path is None and EVERY key lookup
    # returns Error::NoConfigDirectory — which Error::is_err() reports as *not*
    # an error (:120-123), so callers swallow it quietly.
    #
    # Two of those quiet swallows are why the desktop had no working keyboard:
    #   * `defaults` (from cosmic-comp/data/keybindings.ron) IS the entire
    #     keybinding table. shortcuts::shortcuts() falls back to
    #     Shortcuts::default(), an EMPTY HashMap (cosmic-settings-daemon/config/
    #     src/shortcuts/mod.rs:35-38), so without this file cosmic-comp has no
    #     key bindings at all — not merely no system actions.
    #   * `system_actions` maps Action::System(..) to a command line. Handling is
    #     `if let Some(command) = ...system_actions.get(&system)` (cosmic-comp/
    #     src/input/actions.rs:1016-1021), so an empty map makes every system
    #     binding a silent no-op. cosmic-launcher, cosmic-app-library and
    #     cosmic-workspaces are all staged and spawned every boot, and the only
    #     thing that can raise them is such an action.
    # cosmic-panel's "Panel Entry Error: NoConfigDirectory" is the same cause on
    # com.system76.CosmicPanel{,.Panel,.Dock}: container_config.rs:116 reads the
    # `entries` key, then one Config per entry, then one get() per struct field.
    #
    # Sourced from the checked-out ../cosmic-epoch sibling (submodules pinned at
    # epoch-1.3.0), reproducing exactly what upstream's own install rules place
    # under $prefix/share/cosmic — better provenance than the unversioned
    # ~/code/leandros-artifacts staging dirs the rest of the session ship uses.
    cosmic_epoch = os.path.expanduser("~/code/cosmic-epoch")

    def _stage_cosmic_default(rel, hostpath):
        """rel is "<component>/v<N>/<key>"; register every ancestor directory."""
        image_dir = "/usr/share/cosmic/" + os.path.dirname(rel)
        parts = image_dir.lstrip("/").split("/")
        for i in range(2, len(parts) + 1):     # /usr is static (ino 15)
            m4_share_dirs.add("/" + "/".join(parts[:i]))
        m4_share_files.append((image_dir, os.path.basename(rel), hostpath))

    # Recursive installs. These source trees are already laid out as
    # <component>/v<N>/<key>, and upstream copies them wholesale
    # (`find ... -exec install -Dm0644`), so no renaming is involved.
    #   cosmic-panel/justfile:47-49            45 files (Panel + Dock + entries)
    #   cosmic-applets/justfile:16,40-41        2 files (CosmicAppList)
    #   cosmic-settings/justfile:8,36         211 files (themes + CosmicComp)
    #   cosmic-bg/data/justfile:4-7             2 files, under its own APPID
    for _src, _prefix in (
        (f"{cosmic_epoch}/cosmic-panel/data/default_schema", ""),
        (f"{cosmic_epoch}/cosmic-applets/cosmic-app-list/data/default_schema", ""),
        (f"{cosmic_epoch}/cosmic-settings/resources/default_schema", ""),
        (f"{cosmic_epoch}/cosmic-bg/data/v1", "com.system76.CosmicBackground/v1"),
    ):
        if not os.path.isdir(_src):
            continue
        for dirpath, _dn, filenames in os.walk(_src):
            _sub = os.path.relpath(dirpath, _src)
            if _sub == ".":
                _base = _prefix
            elif _prefix:
                _base = _prefix + "/" + _sub
            else:
                _base = _sub
            for fn in sorted(filenames):
                hp = os.path.join(dirpath, fn)
                if os.path.isfile(hp) and _base:
                    _stage_cosmic_default(f"{_base}/{fn}", hp)

    # The three files upstream installs one at a time, each RENAMED: the ".ron"
    # suffix is stripped, and keybindings.ron additionally becomes "defaults".
    #   cosmic-comp/Makefile:26-27,55-57
    #   cosmic-settings-daemon/Makefile:22,36
    for _src, _rel in (
        (f"{cosmic_epoch}/cosmic-comp/data/keybindings.ron",
         "com.system76.CosmicSettings.Shortcuts/v1/defaults"),
        (f"{cosmic_epoch}/cosmic-settings-daemon/data/system_actions.ron",
         "com.system76.CosmicSettings.Shortcuts/v1/system_actions"),
        (f"{cosmic_epoch}/cosmic-comp/data/tiling-exceptions.ron",
         "com.system76.CosmicSettings.WindowRules/v1/tiling_exception_defaults"),
    ):
        if os.path.isfile(_src):
            _stage_cosmic_default(_rel, _src)

    # 2. Dynamically calculate required blocks and image size
    # Each meta segment takes 512 blocks. We have 8 meta segments (4096 blocks).
    # Plus safety margin and blocks for directories.
    def content_size(path):
        return len(path) if isinstance(path, (bytes, bytearray)) else os.path.getsize(path)

    required_blocks = 4096 + 100
    # Count each distinct host path once: repeated paths become hardlinks
    # sharing a single inode and its data blocks, so charging the image for
    # every name would over-allocate by ~100x once coreutils is installed.
    _sized = set()
    for name, path, mode in (bin_files + lib_files + usr_lib_files + gbm_files + root_files + etc_files
                             + [(n, p, 0) for (_d, n, p) in m4_share_files]
                             + [(n, p, 0) for (_d, n, p) in m5_exec_files]
                             + [(n, c, 0) for (_d, n, c, _m) in sysfs_files]):
        if not isinstance(path, (bytes, bytearray)):
            if path in _sized:
                continue
            _sized.add(path)
        size = content_size(path)
        k = (size + BLOCK_SIZE - 1) // BLOCK_SIZE
        # Inode block + data blocks + potential direct nodes.
        # ADDRS_PER_DNODE is 1019 in this simplified F2FS: the node footer
        # sits at byte 4076 (5xu32 = 20 bytes), leaving 4076/4 = 1019 slots.
        # This MUST match servers/f2fs/src/lib.rs (NODE_FOOTER_OFF / 4);
        # a 1018 here shifted every block past ~7.9 MB when read back.
        required_blocks += k + 1 + (k + 1018) // 1019
    # Reserve two full segments beyond the statically-populated content for the
    # runtime node/data allocator curseg pointers (see below) so first writes
    # never land on blocks already occupied by pre-populated files/directories.
    required_blocks += 2 * BLOCKS_PER_SEG
        
    # Align to segments (512 blocks) and set a minimum size of 64MB (16384 blocks).
    # Double the segment count beyond what's needed for the pre-populated content
    # so the image ships with roughly 50% free space — tests (f2fstest et al.)
    # write new files at runtime and would otherwise exhaust a snugly-sized image.
    # (Measured 2026-07-21: 2x gives ~51-52% free on both arches even after the
    # coreutils/brush/xattr userland growth — the margin scales with content, so
    # this constant should not need revisiting as the userland grows.)
    segs = (required_blocks + 511) // 512
    total_segs = max(32, segs * 2)
    total_blocks = total_segs * BLOCKS_PER_SEG
    main_segs = total_segs - META_SEGS
    
    print(f"Sizing F2FS image dynamically to {total_blocks * BLOCK_SIZE // (1024 * 1024)} MB ({total_blocks} blocks, {total_segs} segments, {main_segs} main segments)...")
    
    image = bytearray(total_blocks * BLOCK_SIZE)
    write_superblock(image, 0, total_blocks, total_segs, main_segs)
    write_superblock(image, BLOCK_SIZE, total_blocks, total_segs, main_segs)
    
    # 3. Define statically allocated directory inodes and data blocks
    dir_nodes = {
        3: ("/"),
        4: ("/bin"),
        5: ("/old_root"),
        6: ("/dev"),
        7: ("/proc"),
        8: ("/tmp"),
        9: ("/etc"),
        10: ("/mnt"),
        11: ("/lib"),
        12: ("/root"),
        13: ("/home"),
        14: ("/home/leandro"),
        15: ("/usr"),
        16: ("/usr/lib"),
        17: ("/usr/lib/gbm"),
        # XDG runtime dir for the COSMIC session — the wayland-N socket (bound by
        # cosmic-comp) and the D-Bus session socket (bound by busd) both live in
        # /run/user/0. Pre-created because runtime `mkdir -p /run/user/0` on this
        # f2fs returns 0 without actually creating the deepest level when multiple
        # nested levels are new in one call (a second mkdir completes it) — so the
        # session must not depend on creating it at boot. /run/user/0 is 0700 root
        # per the XDG spec (libwayland warns otherwise).
        18: ("/run"),
        19: ("/run/user"),
        20: ("/run/user/0"),
        # /root's XDG base dirs, pre-created for the same reason as /run/user/0:
        # cosmic-comp/cosmic-* call create_dir_all() under $HOME (=/root) at
        # startup, and this f2fs's runtime mkdir does not reliably materialize a
        # freshly-created directory (the launcher's `mkdir -p` leaves broken
        # ?-type inodes — mode --------- — which turn a later create_dir_all into
        # an ENOTDIR panic). Shipping .config/.cache/.local as real dirs lets the
        # session's create_dir_all of a one-level-deeper path (e.g. .config/cosmic)
        # succeed. Root-owned 0700 to match /root.
        21: ("/root/.config"),
        22: ("/root/.cache"),
        23: ("/root/.local"),
    }

    # Per-directory mode/owner overrides; anything not listed here defaults
    # to 0755 root:root (matching what every directory got before ownership
    # was tracked at all).
    dir_owner = {
        12: (0o040700, 0, 0),        # /root
        14: (0o040700, 1000, 1000),  # /home/leandro
        20: (0o040700, 0, 0),        # /run/user/0 (XDG runtime dir, 0700 root)
        21: (0o040700, 0, 0),        # /root/.config
        22: (0o040700, 0, 0),        # /root/.cache
        23: (0o040700, 0, 0),        # /root/.local
    }

    # Subdirectories per parent, used both to emit "name -> child_ino" dentries
    # below and to compute each parent's link count (every child directory's
    # ".." entry adds one hardlink to its parent).
    subdirs = {
        3: [("bin", 4), ("old_root", 5), ("dev", 6), ("proc", 7), ("tmp", 8),
            ("etc", 9), ("mnt", 10), ("lib", 11), ("root", 12), ("home", 13),
            ("usr", 15), ("run", 18)],
        12: [(".config", 21), (".cache", 22), (".local", 23)],
        13: [("leandro", 14)],
        15: [("lib", 16)],
        16: [("gbm", 17)],
        18: [("user", 19)],
        19: [("0", 20)],
    }

    # ── Register the M4 /usr/share data tree as directory inodes ──────────────
    # Allocate a fresh ino/nid for every directory under /usr (usr/share,
    # usr/share/X11, ..., usr/share/X11/xkb/symbols, usr/share/libinput, ...) and
    # wire each into dir_nodes/subdirs so the standard inode/data-block
    # allocation and dentry-emit loops below handle them identically to the
    # static directories. Sorted order guarantees a parent is registered before
    # its children (a child path is its parent plus "/name", and "/" sorts
    # before any name char). /usr itself is static (ino 15). Files are attached
    # to these inos after packing (see m4_tree_files_by_ino below).
    _path_to_ino  = {p: ino for ino, p in dir_nodes.items()}
    _next_dir_ino = max(dir_nodes) + 1
    for d in sorted(m4_share_dirs):
        if d in _path_to_ino:
            continue
        parent_ino = _path_to_ino[os.path.dirname(d)]
        ino = _next_dir_ino
        _next_dir_ino += 1
        dir_nodes[ino] = d
        _path_to_ino[d] = ino
        subdirs.setdefault(parent_ino, []).append((os.path.basename(d), ino))
    m4_tree_files_by_ino = {}
    for image_dir, name, hostpath in m4_share_files:
        ino = _path_to_ino[image_dir]
        m4_tree_files_by_ino.setdefault(ino, []).append((name, hostpath, 0o100644))
    # The synthetic sysfs attributes ride the same packing table; they carry
    # their own mode because one of them (the "subsystem" link) is S_IFLNK and
    # the rest are 0444 like real sysfs.
    for image_dir, name, content, mode in sysfs_files:
        ino = _path_to_ino[image_dir]
        m4_tree_files_by_ino.setdefault(ino, []).append((name, content, mode))

    inode_blocks = {}
    data_blocks = {}
    
    next_blk = MAIN_BLKADDR
    for ino in sorted(dir_nodes.keys()):
        inode_blocks[ino] = next_blk
        next_blk += 1
        
    for ino in sorted(dir_nodes.keys()):
        data_blocks[ino] = next_blk
        next_blk += 1
        
    # 4. Process regular files, allocate data blocks and inodes
    file_entries = [] # (parent_ino, name, child_ino, file_type)
    next_nid = max(dir_nodes) + 1
    packed_by_path = {}  # host path -> (nid, inode block) for hardlinking
    nlink_by_path = {}   # host path -> current link count
    
    def add_files_to_dir(parent_ino, files):
        nonlocal next_nid, next_blk
        for name, path, mode in files:
            # A file whose mode says S_IFLNK gets a DT_LNK dentry; everything
            # else is DT_REG. The inode itself is built identically either way
            # (mode goes straight into i_mode below, and the target path is just
            # the file's data), which is what makes symlinks a three-line
            # addition here rather than a new code path.
            entry_ftype = DT_LNK if (mode & S_IFMT) == S_IFLNK else DT_REG
            # Hardlink: a host path already packed gets a second directory
            # entry pointing at the same nid rather than a second copy of the
            # content. This is what makes uutils/coreutils viable — its ~100
            # commands are one multicall binary dispatching on argv[0], and
            # duplicating a multi-MB binary per name would add gigabytes to
            # the image. The f2fs server's lookup path reads the nid out of
            # the dentry verbatim, so several names sharing one inode is
            # indistinguishable to it from one name; its unlink path drops
            # only the dentry and never touches the inode, so removing one
            # name cannot corrupt the others.
            key = None if isinstance(path, (bytes, bytearray)) else path
            if key is not None and key in packed_by_path:
                shared_nid, shared_inode_blk = packed_by_path[key]
                nlink_by_path[key] += 1
                w32(image, shared_inode_blk * BLOCK_SIZE + 12, nlink_by_path[key])
                file_entries.append((parent_ino, name.encode('utf-8'), shared_nid, entry_ftype))
                print(f"  Linked {name} -> nid {shared_nid} (hardlink, no extra content)")
                continue

            if isinstance(path, (bytes, bytearray)):
                data = bytes(path)
            else:
                with open(path, 'rb') as f:
                    data = f.read()
            size = len(data)
            k = (size + BLOCK_SIZE - 1) // BLOCK_SIZE
            
            # Allocate data blocks
            file_data_blks = [next_blk + j for j in range(k)]
            next_blk += k
            
            # Write data blocks to image
            for j, daddr in enumerate(file_data_blks):
                start = daddr * BLOCK_SIZE
                chunk = data[j*BLOCK_SIZE : (j+1)*BLOCK_SIZE]
                image[start : start + len(chunk)] = chunk
                
            # Build addresses for inode direct/indirect mapping
            direct_limit = 923
            direct_blks = file_data_blks[:direct_limit]
            rem_blks = file_data_blks[direct_limit:]
            i_nids = [0, 0, 0, 0, 0]
            dnode_info = []
            
            if rem_blks:
                # Direct node 0 (i_nid[0])
                dnode_nid = next_nid
                next_nid += 1
                dnode_blkaddr = next_blk
                next_blk += 1
                i_nids[0] = dnode_nid
                
                dnode_blks = rem_blks[:1019]
                rem_blks = rem_blks[1019:]
                
                dnode_bytes = bytearray(4096)
                for idx, addr in enumerate(dnode_blks):
                    w32(dnode_bytes, idx * 4, addr)
                w32(dnode_bytes, NODE_FOOTER_OFF + 0, dnode_nid)
                w32(dnode_bytes, NODE_FOOTER_OFF + 4, dnode_nid)
                w32(dnode_bytes, NODE_FOOTER_OFF + 8, 0)
                w32(dnode_bytes, NODE_FOOTER_OFF + 12, 0)
                w32(dnode_bytes, NODE_FOOTER_OFF + 16, 1)
                
                dnode_info.append((dnode_nid, dnode_blkaddr, dnode_bytes))
                
            if rem_blks:
                # Direct node 1 (i_nid[1])
                dnode_nid = next_nid
                next_nid += 1
                dnode_blkaddr = next_blk
                next_blk += 1
                i_nids[1] = dnode_nid
                
                dnode_blks = rem_blks[:1019]
                rem_blks = rem_blks[1019:]
                
                dnode_bytes = bytearray(4096)
                for idx, addr in enumerate(dnode_blks):
                    w32(dnode_bytes, idx * 4, addr)
                w32(dnode_bytes, NODE_FOOTER_OFF + 0, dnode_nid)
                w32(dnode_bytes, NODE_FOOTER_OFF + 4, dnode_nid)
                w32(dnode_bytes, NODE_FOOTER_OFF + 8, 0)
                w32(dnode_bytes, NODE_FOOTER_OFF + 12, 0)
                w32(dnode_bytes, NODE_FOOTER_OFF + 16, 1)
                
                dnode_info.append((dnode_nid, dnode_blkaddr, dnode_bytes))
                
            if rem_blks:
                # Indirect node 0 (i_nid[2])
                ind_nid = next_nid
                next_nid += 1
                ind_blkaddr = next_blk
                next_blk += 1
                i_nids[2] = ind_nid
                
                ind_bytes = bytearray(4096)
                chunk_idx = 0
                while rem_blks:
                    chunk_blks = rem_blks[:1019]
                    rem_blks = rem_blks[1019:]
                    
                    dn_nid = next_nid
                    next_nid += 1
                    dn_blkaddr = next_blk
                    next_blk += 1
                    
                    dn_bytes = bytearray(4096)
                    for idx, addr in enumerate(chunk_blks):
                        w32(dn_bytes, idx * 4, addr)
                    w32(dn_bytes, NODE_FOOTER_OFF + 0, dn_nid)
                    w32(dn_bytes, NODE_FOOTER_OFF + 4, dn_nid)
                    w32(dn_bytes, NODE_FOOTER_OFF + 8, 0)
                    w32(dn_bytes, NODE_FOOTER_OFF + 12, 0)
                    w32(dn_bytes, NODE_FOOTER_OFF + 16, 1)
                    
                    dnode_info.append((dn_nid, dn_blkaddr, dn_bytes))
                    w32(ind_bytes, chunk_idx * 4, dn_nid)
                    chunk_idx += 1
                    
                w32(ind_bytes, NODE_FOOTER_OFF + 0, ind_nid)
                w32(ind_bytes, NODE_FOOTER_OFF + 4, ind_nid)
                w32(ind_bytes, NODE_FOOTER_OFF + 8, 0)
                w32(ind_bytes, NODE_FOOTER_OFF + 12, 0)
                w32(ind_bytes, NODE_FOOTER_OFF + 16, 1)
                
                dnode_info.append((ind_nid, ind_blkaddr, ind_bytes))
                
            if rem_blks:
                raise ValueError(f"File {name} is too large (> 4 GB), indirect blocks not supported!")
                
            # Allocate inode block
            file_inode_blk = next_blk
            next_blk += 1
            
            # Write inode block to image
            inode_bytes = bytearray(4096)
            w16(inode_bytes, 0, mode)
            w8 (inode_bytes, 2, 0)
            w8 (inode_bytes, 3, 0)
            w32(inode_bytes, 12, 1) # links
            w64(inode_bytes, 16, size)
            for idx, addr in enumerate(direct_blks):
                w32(inode_bytes, 364 + idx * 4, addr)
            for idx, nid in enumerate(i_nids):
                w32(inode_bytes, 4056 + idx * 4, nid)
            file_nid = next_nid
            next_nid += 1
            w32(inode_bytes, NODE_FOOTER_OFF + 0, file_nid)
            w32(inode_bytes, NODE_FOOTER_OFF + 4, file_nid)
            w32(inode_bytes, NODE_FOOTER_OFF + 8, 0)
            w32(inode_bytes, NODE_FOOTER_OFF + 12, 0)
            w32(inode_bytes, NODE_FOOTER_OFF + 16, 1)
            
            # Write blocks to image
            image[file_inode_blk * BLOCK_SIZE : (file_inode_blk + 1) * BLOCK_SIZE] = inode_bytes
            write_nat_entry(image, file_nid, file_inode_blk)
            
            for dn_nid, dn_blk, dn_bytes in dnode_info:
                image[dn_blk * BLOCK_SIZE : (dn_blk + 1) * BLOCK_SIZE] = dn_bytes
                write_nat_entry(image, dn_nid, dn_blk)
                
            if key is not None:
                packed_by_path[key] = (file_nid, file_inode_blk)
                nlink_by_path[key] = 1

            file_entries.append((parent_ino, name.encode('utf-8'), file_nid, entry_ftype))
            print(f"  Packed {name} (size: {size} bytes, inode blk: {file_inode_blk}, nid: {file_nid})")
            
    print("Packing binaries into /bin...")
    add_files_to_dir(4, bin_files)
    print("Packing libraries into /lib...")
    add_files_to_dir(11, lib_files)
    print("Packing libraries into /usr/lib...")
    add_files_to_dir(16, usr_lib_files)
    print("Packing GBM backends into /usr/lib/gbm...")
    add_files_to_dir(17, gbm_files)
    print("Packing files into /...")
    add_files_to_dir(3, root_files)
    print("Packing files into /etc...")
    add_files_to_dir(9, etc_files)
    if m4_tree_files_by_ino:
        print("Packing /usr/share data tree (XKB keymaps + libinput quirks)...")
        for ino in sorted(m4_tree_files_by_ino):
            add_files_to_dir(ino, m4_tree_files_by_ino[ino])
    if m5_exec_files:
        print("Packing M5 session executables (busd, dbus-run-session) 0755...")
        m5_exec_by_ino = {}
        for image_dir, name, hostpath in m5_exec_files:
            ino = _path_to_ino[image_dir]
            m5_exec_by_ino.setdefault(ino, []).append((name, hostpath, 0o100755))
        for ino in sorted(m5_exec_by_ino):
            add_files_to_dir(ino, m5_exec_by_ino[ino])

    # 5. Write directory blocks and inodes
    # Build dentry blocks list for each directory
    dentry_entries = {ino: [] for ino in dir_nodes.keys()}

    # Add . and .. to all directories (root is its own parent).
    parent_of = {3: 3}
    for parent_ino, children in subdirs.items():
        for _name, child_ino in children:
            parent_of[child_ino] = parent_ino
    for ino in dir_nodes.keys():
        dentry_entries[ino].append((b'.', ino, DT_DIR))
        dentry_entries[ino].append((b'..', parent_of[ino], DT_DIR))

    # Add subdirectory entries to their parents.
    for parent_ino, children in subdirs.items():
        for name, child_ino in children:
            dentry_entries[parent_ino].append((name.encode('utf-8'), child_ino, DT_DIR))

    # Add regular files to their directories
    for parent_ino, name, child_ino, ftype in file_entries:
        dentry_entries[parent_ino].append((name, child_ino, ftype))
        
    # Write directory data blocks and directory inodes
    for ino, name in dir_nodes.items():
        db_blocks = build_dentry_blocks(dentry_entries[ino])
        # Each directory has one statically pre-allocated data block; a
        # directory that outgrows it (/bin, once the coreutils names land)
        # takes its extra blocks from the same bump allocator the files used.
        db_addrs = [data_blocks[ino]]
        for _ in range(len(db_blocks) - 1):
            db_addrs.append(next_blk)
            next_blk += 1
        for db_addr, db_bytes in zip(db_addrs, db_blocks):
            image[db_addr * BLOCK_SIZE : (db_addr + 1) * BLOCK_SIZE] = db_bytes

        in_addr = inode_blocks[ino]
        # Inodes point to their data block. Link count is 2 (self + parent's
        # entry) plus one per child directory (each child's ".." is another
        # hardlink to this inode).
        mode, uid, gid = dir_owner.get(ino, (0o040755, 0, 0))
        links = 2 + len(subdirs.get(ino, []))

        inode_bytes = build_inode_block(mode, links, len(db_addrs) * BLOCK_SIZE, db_addrs, ino)
        w32(inode_bytes, 4, uid)  # i_uid — see servers/f2fs/src/lib.rs INO_UID
        w32(inode_bytes, 8, gid)  # i_gid — see servers/f2fs/src/lib.rs INO_GID
        image[in_addr * BLOCK_SIZE : (in_addr + 1) * BLOCK_SIZE] = inode_bytes

        write_nat_entry(image, ino, in_addr)
        
    # 6. SIT (Segment Info Table) Updates
    MAIN_START_SEG = MAIN_BLKADDR // BLOCKS_PER_SEG
    valid_blocks_per_seg = [0] * main_segs
    valid_map_per_seg = [bytearray(64) for _ in range(main_segs)]
    
    # Count blocks allocated in main area
    allocated_main_blocks = next_blk - MAIN_BLKADDR
    for b in range(allocated_main_blocks):
        seg = b // BLOCKS_PER_SEG
        blk = b % BLOCKS_PER_SEG
        valid_blocks_per_seg[seg] += 1
        valid_map_per_seg[seg][blk // 8] |= (1 << (blk % 8))
        
    # All meta segments fully used
    for seg in range(MAIN_START_SEG):
        o = (SIT_BLKADDR + seg // SIT_PER_BLK) * BLOCK_SIZE + (seg % SIT_PER_BLK) * SIT_ENTRY_SIZE
        w16(image, o, BLOCKS_PER_SEG)
        for b in range(64): image[o + 2 + b] = 0xFF
        
    # Main segments
    for seg in range(main_segs):
        o = (SIT_BLKADDR + (MAIN_START_SEG + seg) // SIT_PER_BLK) * BLOCK_SIZE + ((MAIN_START_SEG + seg) % SIT_PER_BLK) * SIT_ENTRY_SIZE
        w16(image, o, valid_blocks_per_seg[seg])
        image[o + 2 : o + 66] = valid_map_per_seg[seg]
        
    # 7. Checkpoint updates
    valid_blocks_count = next_blk - MAIN_BLKADDR
    valid_nodes_count = next_nid - 3
    used_segs = (valid_blocks_count + BLOCKS_PER_SEG - 1) // BLOCKS_PER_SEG
    free_segs = main_segs - used_segs

    # The runtime allocator (servers/f2fs/src/lib.rs) starts handing out new
    # node/data blocks from these curseg positions and bumps them one block at
    # a time — it never consults the SIT valid-map to skip already-used blocks.
    # They must therefore start on fresh, fully-empty segments *past* every
    # block written above (not segment 0/1, which hold real directories and
    # files), and node/data need distinct segments so their independent
    # cursors can't collide with each other.
    first_free_seg = used_segs
    cur_node_segno, cur_data_segno = first_free_seg, first_free_seg + 1
    assert cur_data_segno < main_segs, "not enough reserved segments for runtime node/data logs"

    write_checkpoint(image, CP_BLKADDR, 1, valid_blocks_count, valid_nodes_count, free_segs, next_nid, main_segs,
                      cur_node_segno, 0, cur_data_segno, 0)
    write_checkpoint(image, CP_BLKADDR + BLOCKS_PER_SEG, 0, valid_blocks_count, valid_nodes_count, free_segs, next_nid, main_segs,
                      cur_node_segno, 0, cur_data_segno, 0)
    
    # Write output file
    with open(out_file, 'wb') as f:
        f.write(image)
        
    print(f"🎉 Created populated F2FS image at {out_file} (size: {len(image)} bytes)")
    print(f"   Allocated {valid_blocks_count} blocks starting at MAIN_BLKADDR {MAIN_BLKADDR}")

if __name__ == '__main__':
    main()
