# Lane stdioredir — 2026-10-01

**Outcome: FIXED (kernel side); item 1 itself is a brush bug, REPORTED with a tested patch.**
Branch `lane/stdioredir` @ `477c9aa` (base origin/main f489941). Machines: Mac (aarch64 HVF), linux desktop (x86_64 KVM).

## 1. `sh -c 'cmd >&2' 2>FILE` prints to the console, FILE empty — brush bug, not the kernel
- brush-core `openfiles.rs`, `impl TryFrom<OpenFile> for Stdio`: `OpenFile::Stdin|Stdout|Stderr => Stdio::inherit()`.
  `commands.rs` only skips the same-slot case, so the shell's *own* inherited stderr wired to the child's
  **stdout** (`>&2`) becomes `inherit()` of the child's fd 1 — i.e. stdout. Same for `2>&1` when the shell's
  stdout is its inherited one. Upstream origin/main has the identical code.
- Reproduced on the **macOS host** with stock brush (no LeandrOS involved):
  `brush -c '/bin/echo hi >&2' 2>f` → "hi" on stdout, `f` empty (`bash`: `f`="hi").
- Fix (tested on the macOS host, stock-vs-patched: `f` empty → "hi"; `2>&1`, `2>&1 >/dev/null`, `>&2 | cat` all correct):
  patch at `artifacts/notes/lane-stdioredir-brush-stdio-dup.patch` (applies to brush a8006e7; excerpt below). It dups the descriptor
  (`try_clone_to_owned`) instead of `inherit()`. **Not applied**: `~/code/brush` has someone's uncommitted
  work (jobs.rs/kill.rs/signal.rs/Cargo.toml) — needs an owner to commit into brush.
```diff
--- a/brush-core/src/openfiles.rs
+++ b/brush-core/src/openfiles.rs
@@ impl TryFrom<OpenFile> for Stdio {
+        #[cfg(unix)]
+        if matches!(open_file, OpenFile::Stdin(_) | OpenFile::Stdout(_) | OpenFile::Stderr(_)) {
+            return Ok(open_file.try_clone_to_owned()?.into());
+        }
         match open_file {
```

### Real kernel redirect bugs found on the way (fixed)
`VnodeKind::DevStdio { target_fd }` (what dup of a raw console fd 0-2, `/dev/console`, `/dev/tty` without a
ctty and `/dev/std*` produce) **followed its target slot live**, so a descriptor changed object when another fd
was later redirected:
- `cmd 2>&1 >FILE` with stdout on the console: stderr went into FILE.
- `/dev/console` (and `/dev/tty` with no ctty) after `< file`: writes went into the stdin file.
- `g=open("/dev/stdout"); dup2(pipe, 1); write(g)`: went to the pipe, not the old stdout.
Fix (servers/vfs): DevStdio is a fixed console object (`fd_is_console_stdio` → true, VFS read/write arms
no longer re-enter on the slot); `/dev/stdin|stdout|stderr` dup the object fd 0/1/2 holds at open time
(BSD /dev/fd semantics; raw console → console proxy).

## 2. scmtest `scm_import_emfile_single_release` — test fixed
Dups until EMFILE (cap 8192, static array). Now `hogs=507 errno=24` → PASS both arches.

## 3. /dev/zero 4 KiB reads — fixed
Whole request per read (cap Linux MAX_RW_COUNT), fault-tolerant `copy_raw` from the zero page with the fd
lock dropped. Also: lseek on /dev/null|zero|urandom returns 0 (uutils `dd of=/dev/null` died with
"failed to seek in output file: Invalid seek"). `dd if=/dev/zero bs=1M count=4`: before `0+4 records,
16384 bytes`; after `4+0 records, 4194304 bytes`.

## 4. `df /tmp` showed the root f2fs — fixed
/proc/mounts, /etc/mtab and /proc/self/mountinfo had only registered (port) mounts; the in-VFS tmpfs roots
(/tmp, /dev/shm, /run/user) are now listed (`tmpfs <root> tmpfs`, own minor each). `df /tmp` → `tmpfs ... /tmp`.

## 5. fallocate — implemented
Kernel `FALLOCATE => 0` replaced by `VFS_FALLOCATE` (0x50). tmpfs: mode 0 (allocates charged pages, ENOSPC
against budget, moves EOF), KEEP_SIZE (allocates, EOF unchanged), PUNCH_HOLE|KEEP_SIZE (zeroes range, frees
whole pages). f2fs: mode 0 grows i_size (no block reservation — f2fs allocates at write), KEEP_SIZE no-op,
punch EOPNOTSUPP. Other modes EOPNOTSUPP, len<=0 EINVAL, pipe ESPIPE, non-file ENODEV.

## Evidence
New tests: exectest +18 (9 redirect shapes × tmpfs/f2fs: `2>FILE`, nested `>&2` surviving exec, `>FILE 2>&1`,
`2>&1 >FILE` console+pipe, /dev/stdout snapshot, /dev/console vs `<file`, two pipelines); vfstest +4
(fallocate_tmpfs, fallocate_f2fs, dev_zero_big_read, mount_table_tmpfs). Marker-based checks: every exec'd
image writes loader diagnostics to fd 2.
- **Base kernel + new tests (aarch64/HVF)**: exectest FAIL 6 (2to1_then_file_console, dev_stdout_snapshot,
  dev_console_vs_stdin × 2 fs), vfstest FAIL 4 (all four new), scmtest PASS (test fix alone).
- **Fixed, aarch64/HVF**: vfstest 59 PASS/0, exectest 28/28, forktest 8, ptytest 15, scmtest 36, polltest 7 — all RC=0.
- **Fixed, x86_64/KVM (desktop)**: identical — vfstest 59, exectest 28/28, forktest 8, ptytest 15, scmtest 36, polltest 7, RC=0.
- COSMIC smoke aarch64/HVF (virgl, Mac GPU QEMU): greeter → leandro login → panel+wallpaper → Super+T cosmic-term → `ls -la /bin` rendered; /run/user/1000 sockets present; no panic.
- COSMIC smoke x86_64/KVM (desktop, virgl, `gpu-session-loop.py 1 x86virgl virgl`, APPS=1 ROUNDS=2: greeter login, 2× Super+T cosmic-term + `ls -la /bin` + Super+Q): session alive at the end (panel clock 18:18:06 + wallpaper + dock), 0 host amdgpu fault lines, no guest panic. Its greetd.log ALIVE probe returned nothing (empty list, not an error line). First attempt failed to launch: `-vnc :47` port busy — use a free LEANDROS_VNC_PORT.

## Shared files touched
kernel/src/syscall.rs (FALLOCATE dispatch + `sys_fallocate`, ~13 lines), servers/vfs/src/lib.rs,
servers/f2fs/src/lib.rs, userland/{exectest,vfstest,scmtest}.

## Open
- brush patch above needs applying in ~/code/brush (owner) + upstream report (reubeno/brush).
- `/proc/self/fd/N` of a console proxy still reads `/dev/stdout` etc. (Linux: the tty path) — unchanged.
- DevVt (/dev/ttyN) VFS read/write arms still delegate to fd 0's slot; unreachable in practice (kernel fast path claims them).
- No `grep` binary in the image (uutils has none) — test harnesses must grep on the host.
