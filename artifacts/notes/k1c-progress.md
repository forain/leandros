# K1-C progress (AF_UNIX scaling + VFS socket nodes + tmpfs mounts)

Brief: wayland_cosmic_plan.md §K1 last bullet. Reuse vfs::export_fd/import_fd/
drop_transfer. Baselines: scmtest 12/12, vfstest/polltest/forktest/sigtest/
memtest/waittest/boot-to-login green BOTH arches.

## FD-SPACE (verified)
- VFS MAX_FDS=64 → fds [0,64).
- SOCK_FD_BASE=0x100. TTY_FD_BASE=0x200 (DORMANT: TTY_OPEN never wired from
  kernel — nothing routes to it; safe to move). EPOLL_FD_BASE=0x400, MAX_EPOLL=32.
- Kernel socket routing: `f>=SOCK_FD_BASE && f<EPOLL_FD_BASE` (3 sites) + bare
  `f>=SOCK_FD_BASE` (route-to-net) sites. Do NOT need to change these if socket
  window stays < 0x400.
- DECISION: MAX_SOCKS 16→512 → SOCK window [0x100,0x300) (< EPOLL 0x400 ✓,
  disjoint from VFS ✓). Move dormant TTY_FD_BASE 0x200→0x1000 (1 line, clears
  the socket window). No kernel routing edits needed. Add SOCK_FD_END=0x300.
- MAX_CONNS 32→256 (test needs ~96). MAX_BOUND 16→512.

## STACK HAZARDS at MAX_SOCKS=512 (fixed-array design kept; de-stack 3 fns)
- handle_fork_dup: `let parent_socks=t.socks` copies 512*SockEntry (~24KB stack)
  → rewrite via parent_pos/child_pos index loop; `ends` array → Vec.
- handle_close_all: `[usize::MAX;512]`+`[None;512]` stack arrays → Vec.
- get_or_create + close_all: `= ProcSockTable::empty()` (24KB temp) → reset() in place.
- Keep ProcSockTable Copy derive (no implicit big copies remain).

## AF_UNIX VFS SOCKET NODES (design)
- BoundPath gains `sock_id:u64` (monotonic, NEXT_SOCK_ID AtomicU64 start 1) +
  `is_abstract:bool`. Every bind (abstract or pathname) gets a sock_id.
- Abstract (sun_path[0]==0): keep BOUND_PATHS byte-match (unchanged). No VFS node.
- Pathname: net copies sun_path bytes → vfs::unix_bind_node(pid,&path,sock_id).
  On -17(EEXIST)→net returns -98(EADDRINUSE). No BOUND_PATHS byte-scan for pathname.
- connect: pathname → vfs::unix_resolve_node(pid,&path)->i64 sock_id or -errno
  (-2 ENOENT, -111 ECONNREFUSED if node exists but not S_IFSOCK / no live Bound).
  abstract → byte-match BOUND_PATHS → sock_id. Then find live BoundPath w/ sock_id
  → bound_idx; set connector → UnixPendingAccept{conn_idx, sock_id}.
- accept: UnixListening{bound_idx} → sock_id = BOUND_PATHS[bound_idx].sock_id;
  match pending by sock_id (ABA-proof, fixes multi-listener routing).
- UnixPendingAccept{conn_idx} → {conn_idx, sock_id}. 5 sites (273,830,857,986,1877).
- handle_close/close_all: free BOUND_PATHS slot when a UnixListening closes
  (was leaking). Leaves VFS node (Linux: socket file persists past close).
- unlink removes node (existing handle_unlink) → rebindable; live conns untouched.
- force_bind_unix (pipewire): give it a sock_id + is_abstract=false; keep as
  byte-match listener (no VFS node — internal).

## net→vfs calls (lock order: vfs locks BEFORE any net lock; copy path to kbuf first)
- pub fn unix_bind_node(pid,path:&[u8],sock_id:u64)->i32
- pub fn unix_resolve_node(pid,path:&[u8])->i64

## VFS TMPFS MULTI-ROOT
- TMPFS_ROOTS=[b"/tmp",b"/dev/shm",b"/run/user/0"]. is_tmpfs_root(), tmpfs_root_of().
- is_tmp_path → tmpfs_root_of().is_some(). tmp_parent/tmp_dir_exists: root check
  via is_tmpfs_root. ~7 `== b"/tmp"` root guards → is_tmpfs_root().
- should_lookup_ramfs: add `/run/user` prefix (/dev/ already routes). 
- tmp_resolve_links: `comp_start=4 // past /tmp` → root.len() (generalize).
- RAMFS_DIRS: add /dev/shm,/run,/run/user,/run/user/0. stat RAMFS_DIRS branch:
  per-dir mode override (/dev/shm→01777, /run/user/0→0700, /run,/run/user→0755).
- TmpFileEntry: +is_sock:bool +sock_id:u64. empty(). tmp_ifmt +S_IFSOCK(0o140000).
  stat_common tmpfs-file ifmt (5624) + handle_fstat tmpfs arm + getdents d_type
  (DT_SOCK=12) add is_sock.
- MAP_SHARED under /dev/shm: FREE (VMO keyed on TmpFile idx; open routes to
  TmpFile via generalized is_tmp_path). Verify only.

## QUEUED-FD CAP (K1-A handoff)
- QUEUED_FD_CAP=1024/direction. handle_sendmsg: after conn lock, before write,
  if (queued fds on dir)+nfd > cap → drop batch, return -109 (ETOOMANYREFS).

## STATUS: all edits done, builds clean (build2/build3 EXIT 0). Running tests.

## EDITS APPLIED
- net/src/lib.rs: caps (512/256/512), SOCK_FD_END, QUEUED_FD_CAP=1024,
  ETOOMANYREFS=-109; BoundPath +sock_id +is_abstract; NEXT_SOCK_ID atomic;
  UnixPendingAccept{+sock_id}; ProcSockTable::reset(); free_bound_idx();
  handle_bind abstract/pathname split (vfs::unix_bind_node); handle_connect
  vfs::unix_resolve_node + live-BoundPath check; handle_accept match by sock_id;
  sendmsg queued-fd cap; handle_close/close_all free bound + destack (Vec);
  handle_fork_dup destack (index loop); force_bind_unix fields.
- vfs/src/lib.rs: TmpFileEntry +is_sock +sock_id; tmp_ifmt/stat/fstat/getdents
  S_IFSOCK/DT_SOCK; TMPFS_ROOTS[/tmp,/dev/shm,/run/user/0]; is_tmpfs_root/
  tmpfs_root_of; is_tmp_path/tmp_parent/tmp_dir_exists generalized; 7 root
  guards → is_tmpfs_root; should_lookup_ramfs +/run/user; RAMFS_DIRS +4 dirs;
  ramfs_dir_mode (/dev/shm 1777, /run/user/0 700); tmp_resolve_links root-len;
  handle_open guard; unix_bind_node/unix_resolve_node pub fns; hardlink copies
  is_sock/sock_id.
- tty/src/lib.rs: TTY_FD_BASE 0x200→0x1000 (dormant, clears socket window).
- scmtest: +7 tests (socket_node_roundtrip, socket_node_devshm, unlink_rebind,
  many_socketpairs_and_listeners[64sp+32listen concurrent], tmpfs_mounts_exist,
  devshm_shared_mmap[2-proc by name], queued_fd_cap).

## RUN: driver.py start <arch>; login root root; scmrun.py "scmtest" 90
