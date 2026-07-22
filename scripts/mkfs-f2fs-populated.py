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
        "waittest", "sigchldtest", "scmtest", "epolltest", "idletest", "drmsmoke", "evtest2",
        "mount", "umount", "fstab", "lsblk", "lspci", "lsusb", "ping", "xattr",
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
               "libwayland-client.so.0", "libwayland-server.so.0", "libffi.so.8"):
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
    for so in ("libxkbcommon.so.0", "libdisplay-info.so.3", "libseat.so.1",
               "libudev.so.1", "libinput.so.10", "libpixman-1.so.0",
               "libmtdev.so.1", "libevdev.so.2"):
        p = f"{m4_lib_dir}/{so}"
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

    # Synthetic sysfs skeleton for anvil's drm-rs. DrmNode::from_path ->
    # is_device_drm() stat()s /sys/dev/char/<major>:<minor>/device/drm, and
    # node_with_type(Primary) read_dir()s that directory looking for a "card0"
    # entry (whose /dev/dri/card0 must then exist — it does). Provide the empty
    # directory tree (no files needed); the minor-number range gives the node
    # type, so the mere existence of the drm dir + a card0 child suffices.
    if os.path.isdir(m4_share_src) or os.path.exists(anvil_bin):
        for d in ("/sys", "/sys/dev", "/sys/dev/char",
                  "/sys/dev/char/226:0", "/sys/dev/char/226:0/device",
                  "/sys/dev/char/226:0/device/drm",
                  "/sys/dev/char/226:0/device/drm/card0"):
            m4_share_dirs.add(d)

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
                             + [(n, p, 0) for (_d, n, p) in m4_share_files]):
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
    }

    # Per-directory mode/owner overrides; anything not listed here defaults
    # to 0755 root:root (matching what every directory got before ownership
    # was tracked at all).
    dir_owner = {
        12: (0o040700, 0, 0),        # /root
        14: (0o040700, 1000, 1000),  # /home/leandro
    }

    # Subdirectories per parent, used both to emit "name -> child_ino" dentries
    # below and to compute each parent's link count (every child directory's
    # ".." entry adds one hardlink to its parent).
    subdirs = {
        3: [("bin", 4), ("old_root", 5), ("dev", 6), ("proc", 7), ("tmp", 8),
            ("etc", 9), ("mnt", 10), ("lib", 11), ("root", 12), ("home", 13),
            ("usr", 15)],
        13: [("leandro", 14)],
        15: [("lib", 16)],
        16: [("gbm", 17)],
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
                file_entries.append((parent_ino, name.encode('utf-8'), shared_nid, DT_REG))
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

            file_entries.append((parent_ino, name.encode('utf-8'), file_nid, DT_REG))
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
