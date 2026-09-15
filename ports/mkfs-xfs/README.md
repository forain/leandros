# mkfs.xfs (LeandrOS)

An XFS v5 formatter written from scratch in Rust. Not a port: xfsprogs is C, the
repo is Rust-only, so this reimplements the parts of `mkfs.xfs` that an
installer needs, reading the on-disk format out of xfsprogs 7.1.1
(`libxfs/xfs_format.h`, `libxfs/xfs_ag.c`, `libxfs/xfs_sb.c`, `mkfs/xfs_mkfs.c`,
`libxfs/rdwr.c`) and confirming every field against images written by the real
`mkfs.xfs` 7.1.1.

It exists so AerynOS' [disks-rs](https://github.com/AerynOS/disks-rs) runs
**unmodified** on LeandrOS. disks-rs shells out to

```
mkfs.xfs -m uuid=<UUID> -L ROOT -n parent=1 -i exchange=1 -m autofsck=repair -f <device>
```

and that exact line is the primary test case.

## What it writes

A v5 filesystem with the feature set `mkfs.xfs` 7.x turns on by default:
CRCs, ftype, finobt, sparse inodes, rmapbt, reflink, bigtime, inobtcount and
nrext64 — plus exchange-range and parent pointers when asked for. Sector size
512, block size 4096, inode size 512, internal log. Geometry (AG count and size,
log size, `imaxpct`) is computed exactly as xfsprogs does, so for any given
device size the superblock comes out byte-identical to the real tool's.

`-m autofsck=<v>` is implemented: it is a root-namespace shortform xattr
`xfs:autofsck` on the root inode, which Linux exposes as
`trusted.xfs:autofsck`.

Layout, per AG: block 0 carries the superblock copy, AGF, AGI and AGFL (one
sector each); blocks 1..6 carry the bnobt, cntbt, inobt, finobt, rmapbt and
refcountbt roots. AG 0 additionally holds the 64-inode chunk with the root
directory (an empty shortform directory) and the realtime bitmap/summary
inodes; the log lives in AG `agcount/2`. This follows libxfs'
`xfs_ag_init_headers()` — the growfs path — rather than mkfs' transactional
path, so the AGFL is left empty instead of pre-filled.

Only blocks that must carry content are written and "zero this range" first
checks whether the range is already zero, so formatting a 120 GiB sparse image
materialises about 192 KiB.

## What it does not do

Multi-device or stripe geometry, external logs, realtime subvolumes, quotas,
protofiles, v4 filesystems, and any block/sector/inode size other than
4096/512/512. Options selecting those are accepted and reported as ignored
rather than being made fatal, so an installer's command line never fails on an
option we do not honour. `-m crc=0` is the one hard error: this tool only
writes v5.

## Building

```
./build.sh            # both arches
./build.sh aarch64
```

Static musl via the repo's zig linker wrappers, the pinned nightly, and
`-C relocation-model=static` (mandatory — x86_64-unknown-linux-musl otherwise
emits a static-PIE that the LeandrOS ELF loader maps at vaddr 0). `build.sh`
asserts ET_EXEC and the absence of `PT_INTERP`. Binaries land in
`out/<arch>/mkfs.xfs`.

## Testing

`cargo test --target <host triple>` checks geometry, superblock fields, the
root inode bytes, the free-space extents and the log record against values
taken from real mkfs.xfs output. End-to-end verification needs Linux:

```
mkfs.xfs -f img
docker run --rm -v $PWD:/w alpine:edge sh -c 'apk add -q xfsprogs xfsprogs-extra && xfs_repair -n /w/img'
```
