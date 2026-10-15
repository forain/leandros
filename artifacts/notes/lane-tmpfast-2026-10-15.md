# lane/tmpfast — 2026-10-15

## Problem

a9c4743d grew the tmpfs pool (`MAX_TMP_FILES`) 512 -> 4096 slots, which the
laptop's i915 dma-buf path and Firefox's /dev/shm + memfd use need. Every tmpfs
lookup (`tmp_find`) walked all 4096 `TmpFileEntry`s, about 2.5 KiB each because
a 2 KiB xattr arena sat inline. A path walk calls it once per component
(`tmp_resolve_links`, `tmp_check_traversal`). `tmp_nlink` also scanned the pool
on every stat/fstat (`tmp_alias_count`), and free-slot search, getdents and
rmdir each did their own scan.

The pool stays at 4096.

## Design (servers/vfs/src/tmppool.rs)

The pool is a `TmpPool` struct that owns its bookkeeping:

| structure | replaces |
|---|---|
| name index: `[u16; 8192]` open addressing, linear probing, backward-shift delete, holding only *named* slots (`in_use && !ephemeral`) | `tmp_find` full scan, O(1) for a hit or a miss |
| `phash[i]`: hash of slot i's parent path (0 if unnamed) | getdents prefix test on every slot |
| in-use bitmap `[u64; 64]` | free-slot `position(!in_use)`, census and descendant walks |
| `nalias[o]`: in-use slots whose `link_to == o+1` | `tmp_alias_count` scan (every stat/fstat via `tmp_nlink`) |
| `xattr[i]`: side table of 2 KiB arenas | inline `TmpFileEntry::xattr` (hot entry ~2.5 KiB -> ~450 B) |

`path`, `in_use`, `ephemeral` and `link_to` are private to the module, and code
outside it cannot construct an entry. The only way to change them is through
`claim`, `release`, `set_path`, `make_nameless` and `set_link_to`, so the
compiler finds any site that would bypass the index. `TmpPool` derefs to
`[TmpFileEntry]`, so every other field is read and written as before. Slots are
still allocated lowest-free-first, so getdents cursors (slot index + 2) keep
their old semantics.

`TmpPool::verify()` recomputes every derived structure and returns the number
of inconsistencies:
- the VFS dump hook prints it as `tmpfsck=`;
- `LEANDROS_TMPFSCK=1 ./scripts/build-all.sh` compiles a `verify()` call after
  every mutation, which logs `[VFS] tmpfsck FAILED after <op>`. The normal
  build compiles it out; check with `strings kernel | grep tmpfsck`.

Separate commit: on a tmpfs miss, `stat_common` used to fall through to the
mount table. After the pivot_root, F2FS is "/", so every missed /tmp, /dev/shm
or /run/user stat became an IPC round trip to F2FS. It could also report an
on-disk file that `open()` refuses (open already answers ENOENT from the pool).
A miss now returns ENOENT, unless a filesystem is mounted inside a tmpfs
namespace (`mounted_inside_tmpfs`).

## Numbers

`fsbench tmpbench <nbg> <iters>` (new) reports ns/op. nbg is the number of
background files in /tmp/tb/bg. aarch64 is HVF; x86_64 is TCG on the Mac.

aarch64, iters 3000, values are nbg 0 / nbg 2000:

| op | before (d7f04ea2) | after |
|---|---|---|
| stat missing /tmp name | 40976 / 47730 | 4306 / 4350 |
| stat 5 dirs deep | 24252 / 23976 | 3203 / 3235 |
| create+close+unlink | 60987 / 66927 | 14389 / 14250 |
| memfd_create+fstat+close | 84986 / 90495 | 13818 / 13867 |
| getdents pass (bg dir) | 30933 / 104400 | 10733 / 89633 |
| (stat missing rootfs name, for scale) | n/a | 8633 / 8544 |
| brush 3000x `test -e /tmp/nope` | 0.152-0.155 s | 0.042-0.045 s |

x86_64, iters 1000 (readdir 10000), values are nbg 0 / nbg 2000:

| op | before | after |
|---|---|---|
| stat missing /tmp name | 168678 / 681713 | 23482 / 23857 |
| stat 5 dirs deep | 39747 / 55254 | 15859 / 15872 |
| create+close+unlink | 197191 / 202961 | 80096 / 78867 |
| memfd_create+fstat+close | 229794 / 307252 | 76563 / 73447 |
| getdents pass | 154500 / 591900 | 52620 / 505100 |
| brush 3000x `test -e /tmp/nope` | 0.82-1.16 s | 0.35 s |

On x86 the first version of `children_from` tested all 4096 parent hashes and
regressed the empty-directory pass to 235 us. Walking from the in-use bitmap
fixed it (commit "walk only in-use slots"). The nbg 2000 getdents pass is now
bounded by writing 2000 dirents, not by the pool.

Under a live COSMIC + Firefox session on aarch64 (ffsession `--post`), a stat
miss costs 4.5 us and create/unlink churn 22 us.

## Tests (rebased on main 8e2c1dfb)

- `runtests --suite regress /bin/permtest` (which includes memtest, vfstest and
  chroottest):
  - aarch64: all green;
  - x86_64: all green.
- sshsmoke (new in regress on main) failed 5/5 on both arches during the
  combined run with "No route to host" to 192.168.105.4. The two VMs (and
  another lane's) shared socket_vmnet. Rerun alone with `LEANDROS_NET=user`
  it passes 5/5 on both arches.
- x86_64 scmtest `tcp_connect_refused` failed once before the rebase
  (synchronous ECONNREFUSED on a loopback connect, a TCP timing case). It
  passed 4/4 in a rerun. It does not touch tmpfs.
- `LEANDROS_TMPFSCK=1` aarch64 build: full regress + permtest green, and a
  COSMIC + Firefox session showed no `tmpfsck FAILED` line.
- COSMIC + Firefox (`ffsession.py aarch64 --url file:///tmp/fftest.html`)
  rendered the page on every build, with no pool-FULL or panic.
- Host randomized test: 400k ops of claim, release, set_path, make_nameless
  and set_link_to, checked against a linear-scan model for find,
  children_from, used_from, free_slot and alias counts. The harness was
  ad hoc and is not committed.

## Open items

- x86_64 KVM, the real laptop and the real PC are not exercised. The change
  is arch-neutral.
- `tmp_has_descendants` (rmdir, directory rename) and the directory-rename
  rewrite still walk every in-use slot. They are cheap now that the walk
  skips free slots and the entry is ~450 B, and they are not on a hot path.
- The xattr side table is still a dense 8 MiB BSS array. Lazily allocated
  arenas would save almost all of it.
- `tmp_open_fd_mask` (taken on every unlink) walks every fd table. That is
  independent of the pool size and was not touched.
