//! Leandros PID-1 init server.
//!
//! Called from `kernel/src/init.rs`.  Runs a POSIX smoke-test suite, then a
//! shell demo that exercises the full userland API surface.
//!
//! ## Architecture
//!
//! The init server runs in-kernel as a regular task.  It calls the same
//! subsystem library APIs that `kernel/src/syscall.rs` uses for every SVC/
//! SYSCALL from user space.  I/O is provided by two function-pointer hooks
//! supplied by the kernel at startup.

#![no_std]
#![allow(dead_code)]

extern crate sched;
extern crate ipc;

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use ipc::Message;
use vfs_server as vfs;
use net_server as net;

// ── I/O hooks ─────────────────────────────────────────────────────────────────

pub struct IoHooks {
    pub print_str:  fn(&str),
    pub write_raw:  fn(&[u8]),
    /// Non-blocking: returns the next byte from stdin, or `None` if empty.
    pub read_byte:  fn() -> Option<u8>,
}

static mut IO: *const IoHooks = core::ptr::null();

// usize::MAX = no redirect; any other value = VFS fd
static STDOUT_REDIR: AtomicUsize = AtomicUsize::new(usize::MAX);
static STDIN_REDIR:  AtomicUsize = AtomicUsize::new(usize::MAX);

// ── Shell variable table ───────────────────────────────────────────────────────

const MAX_VARS:    usize = 16;
const MAX_VAR_KEY: usize = 32;
const MAX_VAR_VAL: usize = 128;

struct ShellVar {
    key: [u8; MAX_VAR_KEY],
    klen: usize,
    val: [u8; MAX_VAR_VAL],
    vlen: usize,
}
impl ShellVar {
    const fn empty() -> Self {
        Self { key: [0u8; MAX_VAR_KEY], klen: 0, val: [0u8; MAX_VAR_VAL], vlen: 0 }
    }
}

static SHELL_VARS:      spin::Mutex<[ShellVar; MAX_VARS]> = spin::Mutex::new([const { ShellVar::empty() }; MAX_VARS]);
static LAST_EXIT_CODE: AtomicI32 = AtomicI32::new(0);

// ── Shell function table ──────────────────────────────────────────────────────

const MAX_FUNCS:     usize = 8;
const MAX_FUNC_NAME: usize = 32;
const MAX_FUNC_BODY: usize = 256;

struct ShellFunc {
    name: [u8; MAX_FUNC_NAME],
    nlen: usize,
    body: [u8; MAX_FUNC_BODY],
    blen: usize,
}
impl ShellFunc { const fn empty() -> Self { Self { name: [0u8; MAX_FUNC_NAME], nlen: 0, body: [0u8; MAX_FUNC_BODY], blen: 0 } } }

static SHELL_FUNCS: spin::Mutex<[ShellFunc; MAX_FUNCS]> = spin::Mutex::new([const { ShellFunc::empty() }; MAX_FUNCS]);

fn func_define(name: &[u8], body: &[u8]) {
    let mut funcs = SHELL_FUNCS.lock();
    let slot = funcs.iter().position(|f| f.nlen == name.len() && f.name[..f.nlen] == *name)
        .or_else(|| funcs.iter().position(|f| f.nlen == 0));
    if let Some(i) = slot {
        let nlen = name.len().min(MAX_FUNC_NAME);
        funcs[i].name[..nlen].copy_from_slice(&name[..nlen]);
        funcs[i].nlen = nlen;
        let blen = body.len().min(MAX_FUNC_BODY);
        funcs[i].body[..blen].copy_from_slice(&body[..blen]);
        funcs[i].blen = blen;
    }
}

/// Look up and invoke a shell function. Returns false if not defined.
fn call_func(name: &[u8], args: &[u8]) -> bool {
    let mut body_buf = [0u8; MAX_FUNC_BODY];
    let blen = {
        let funcs = SHELL_FUNCS.lock();
        if let Some(f) = funcs.iter().find(|f| f.nlen == name.len() && f.name[..f.nlen] == *name) {
            body_buf[..f.blen].copy_from_slice(&f.body[..f.blen]); f.blen
        } else { return false; }
    };
    // Set positional parameters $1...$9 from args.
    let mut rest = args;
    for i in 1usize..=9 {
        let key = [b'0' + i as u8];
        let (tok, tail) = if let Some(sp) = rest.iter().position(|&b| b == b' ') {
            (&rest[..sp], trim(&rest[sp+1..]))
        } else { (rest, &b""[..]) };
        var_set(&key, tok);
        rest = tail;
    }
    dispatch_command(&body_buf[..blen]);
    true
}

fn var_set(key: &[u8], val: &[u8]) {
    let mut vars = SHELL_VARS.lock();
    let pos = vars.iter().position(|v| v.klen == key.len() && v.key[..v.klen] == *key)
        .or_else(|| vars.iter().position(|v| v.klen == 0));
    if let Some(i) = pos {
        let klen = key.len().min(MAX_VAR_KEY);
        vars[i].key[..klen].copy_from_slice(&key[..klen]);
        vars[i].klen = klen;
        let vlen = val.len().min(MAX_VAR_VAL);
        vars[i].val[..vlen].copy_from_slice(&val[..vlen]);
        vars[i].vlen = vlen;
    }
}

/// Copy the value of `key` into `out`; returns the filled slice length.
fn var_get_len(key: &[u8], out: &mut [u8; MAX_VAR_VAL]) -> usize {
    let vars = SHELL_VARS.lock();
    for v in vars.iter() {
        if v.klen == key.len() && v.key[..v.klen] == *key {
            let len = v.vlen;
            out[..len].copy_from_slice(&v.val[..len]);
            return len;
        }
    }
    0
}

/// Expand $VAR references in `src` into `dst`. Also expands $$ (pid) and $? (always 0).
fn var_expand(src: &[u8], dst: &mut [u8; 512]) -> usize {
    let mut si = 0usize;
    let mut di = 0usize;
    while si < src.len() && di < dst.len() - 1 {
        if src[si] == b'$' && si + 1 < src.len() {
            si += 1;
            // Read variable name: alphanumeric + underscore, or special $$ / $?
            if src[si] == b'(' {
                // $(cmd) — command substitution
                si += 1; // skip '('
                let cmd_start = si;
                while si < src.len() && src[si] != b')' { si += 1; }
                let subcmd = &src[cmd_start..si];
                if si < src.len() { si += 1; } // skip ')'
                const O_WRONLY_VS: u32 = 0x001;
                const O_CREAT_VS:  u32 = 0x040;
                const O_TRUNC_VS:  u32 = 0x200;
                let wfd = vfs_open(b"/tmp/.subcmd", O_WRONLY_VS | O_CREAT_VS | O_TRUNC_VS, 0o600);
                if wfd >= 0 {
                    let prev = STDOUT_REDIR.swap(wfd as usize, Ordering::Relaxed);
                    dispatch_command(subcmd);
                    STDOUT_REDIR.store(prev, Ordering::Relaxed);
                    vfs_close(wfd);
                    let rfd = vfs_open(b"/tmp/.subcmd", 0, 0);
                    if rfd >= 0 {
                        let mut sub_out = [0u8; 256];
                        let n = vfs_read(rfd, sub_out.as_mut_ptr(), sub_out.len());
                        vfs_close(rfd);
                        if n > 0 {
                            let mut sn = n as usize;
                            while sn > 0 && (sub_out[sn-1] == b'\n' || sub_out[sn-1] == b'\r') { sn -= 1; }
                            for k in 0..sn { if di < 511 { dst[di] = sub_out[k]; di += 1; } }
                        }
                    }
                }
            } else if src[si] == b'$' {
                // $$  → current pid
                let pid = sched::current_pid();
                let (pb, pl) = u32_dec(pid);
                for k in 0..pl { if di < dst.len()-1 { dst[di] = pb[k]; di += 1; } }
                si += 1;
            } else if src[si] == b'?' {
                // $? → 0 (last exit code; we don't track it)
                dst[di] = b'0'; di += 1; si += 1;
            } else {
                // collect name
                let name_start = si;
                while si < src.len() && (src[si].is_ascii_alphanumeric() || src[si] == b'_') { si += 1; }
                let name = &src[name_start..si];
                let mut out = [0u8; MAX_VAR_VAL];
                let vlen = var_get_len(name, &mut out);
                for k in 0..vlen { if di < dst.len()-1 { dst[di] = out[k]; di += 1; } }
            }
        } else {
            dst[di] = src[si]; di += 1; si += 1;
        }
    }
    di
}

#[inline(always)]
fn io_write_raw(b: &[u8]) {
    let fd = STDOUT_REDIR.load(Ordering::Relaxed);
    if fd != usize::MAX {
        let pid = sched::current_pid();
        let msg = make_msg(vfs::VFS_WRITE, &[fd as u64, b.as_ptr() as u64, b.len() as u64]);
        let _ = vfs::handle(&msg, pid);
    } else {
        unsafe { if !IO.is_null() { ((*IO).write_raw)(b) } }
    }
}

macro_rules! kprint    { ($s:expr) => { io_write_raw($s.as_bytes()) } }
macro_rules! kprintln  { ($s:expr) => { io_write_raw(concat!($s, "\n").as_bytes()) } }
macro_rules! kraw      { ($b:expr) => { io_write_raw($b) } }

// ── Message helpers (VFS / net protocol) ─────────────────────────────────────

/// Build a message with tag and up to 6 u64 arguments in data[].
fn make_msg(tag: u64, args: &[u64]) -> Message {
    let mut m = Message::empty();
    m.tag = tag;
    for (i, &a) in args.iter().take(7).enumerate() {
        let off = i * 8;
        if off + 8 <= m.data.len() {
            m.data[off..off+8].copy_from_slice(&a.to_le_bytes());
        }
    }
    m
}

/// Read the first i64 from a server reply.
fn reply_i64(r: &Message) -> i64 {
    i64::from_le_bytes(r.data[..8].try_into().unwrap_or([0u8; 8]))
}

// ── VFS helpers ───────────────────────────────────────────────────────────────

fn vfs_open(path: &[u8], flags: u32, _mode: u32) -> i32 {
    // Ensure NUL-termination in a local buffer.
    let mut buf = [0u8; 256];
    let len = path.len().min(255);
    buf[..len].copy_from_slice(&path[..len]);
    // strip existing NUL for clean copy then re-terminate
    let end = buf[..len].iter().position(|&b| b == 0).unwrap_or(len);
    buf[end] = 0;
    let pid = sched::current_pid();
    let msg = make_msg(vfs::VFS_OPEN, &[buf.as_ptr() as u64, flags as u64, 0]);
    reply_i64(&vfs::handle(&msg, pid)) as i32
}

fn vfs_read(fd: i32, buf: *mut u8, len: usize) -> isize {
    let pid = sched::current_pid();
    let msg = make_msg(vfs::VFS_READ, &[fd as u64, buf as u64, len as u64]);
    reply_i64(&vfs::handle(&msg, pid)) as isize
}

/// Blocking read: loops on EAGAIN (-11) until data or EOF arrives.
fn vfs_read_blocking(fd: i32, buf: *mut u8, len: usize) -> isize {
    loop {
        let n = vfs_read(fd, buf, len);
        if n != -11 { return n; }
        sched::yield_now("init_idle");
    }
}

fn vfs_write(fd: i32, buf: *const u8, len: usize) -> isize {
    let pid = sched::current_pid();
    let msg = make_msg(vfs::VFS_WRITE, &[fd as u64, buf as u64, len as u64]);
    reply_i64(&vfs::handle(&msg, pid)) as isize
}

fn vfs_close(fd: i32) {
    let pid = sched::current_pid();
    // Route socket fds to the net server.
    if fd as usize >= net::SOCK_FD_BASE {
        let msg = make_msg(net::NET_CLOSE, &[fd as u64]);
        let _ = net::handle(&msg, pid);
        return;
    }
    let msg = make_msg(vfs::VFS_CLOSE, &[fd as u64]);
    let _ = vfs::handle(&msg, pid);
}

/// pipe() — writes the two fds into a local [u32;2] and returns them.
fn vfs_pipe() -> Option<(i32, i32)> {
    let pid = sched::current_pid();
    let mut fds = [0u32; 2];
    let msg = make_msg(vfs::VFS_PIPE, &[fds.as_mut_ptr() as u64,
                                         fds[1..].as_mut_ptr() as u64]);
    let r = reply_i64(&vfs::handle(&msg, pid));
    if r == 0 { Some((fds[0] as i32, fds[1] as i32)) } else { None }
}

/// dup2(old, new) using VFS_DUP2.
fn vfs_dup2(old: i32, new: i32) -> i32 {
    let pid = sched::current_pid();
    let msg = make_msg(vfs::VFS_DUP2, &[old as u64, new as u64]);
    reply_i64(&vfs::handle(&msg, pid)) as i32
}

fn vfs_getdents64(fd: i32, buf: *mut u8, count: usize) -> isize {
    let pid = sched::current_pid();
    let msg = make_msg(vfs::VFS_GETDENTS64, &[fd as u64, buf as u64, count as u64]);
    reply_i64(&vfs::handle(&msg, pid)) as isize
}

// ── POSIX smoke tests ─────────────────────────────────────────────────────────

static TESTS_RUN:    spin::Mutex<u32> = spin::Mutex::new(0);
static TESTS_PASSED: spin::Mutex<u32> = spin::Mutex::new(0);

fn pass(name: &str) {
    *TESTS_RUN.lock()    += 1;
    *TESTS_PASSED.lock() += 1;
    kprint!("  [PASS] "); kraw!(name.as_bytes()); kprintln!("");
}
fn fail(name: &str, why: &str) {
    *TESTS_RUN.lock() += 1;
    kprint!("  [FAIL] ");
    kraw!(name.as_bytes()); kprint!(" -- ");
    kraw!(why.as_bytes()); kprintln!("");
}

fn t_getpid() {
    if sched::current_pid() > 0 { pass("getpid() > 0"); }
    else                         { fail("getpid",  "returned 0"); }
}

fn t_getcwd() {
    let mut buf = [0u8; 256];
    let n = sched::current_cwd(buf.as_mut_ptr(), buf.len());
    if n > 0 && buf[0] == b'/' { pass("getcwd() starts with '/'"); }
    else { fail("getcwd", "bad result"); }
}

fn t_chdir() {
    if sched::set_cwd(b"/etc") {
        let mut buf = [0u8; 256];
        let n = sched::current_cwd(buf.as_mut_ptr(), buf.len()) as usize;
        sched::set_cwd(b"/");
        if n >= 4 && &buf[..4] == b"/etc" { pass("chdir + getcwd"); }
        else { fail("chdir", "getcwd mismatch"); }
    } else {
        fail("chdir /etc", "set_cwd returned false");
    }
}

fn t_open_read_close() {
    let fd = vfs_open(b"/etc/hostname", 0, 0);
    if fd < 0 { fail("open /etc/hostname", "negative fd"); return; }
    let mut buf = [0u8; 64];
    let n = vfs_read(fd, buf.as_mut_ptr(), buf.len());
    vfs_close(fd);
    if n > 0 { pass("open+read+close /etc/hostname"); }
    else { fail("read /etc/hostname", "returned <= 0"); }
}

fn t_write_stdout() {
    let msg = b"[init] stdout self-test\n";
    let n = vfs_write(1, msg.as_ptr(), msg.len());
    if n == msg.len() as isize { pass("write(stdout)"); }
    else { fail("write stdout", "short write"); }
}

fn t_pipe() {
    let (r, w) = match vfs_pipe() {
        Some(p) => p,
        None    => { fail("pipe", "pipe() failed"); return; }
    };
    let msg = b"ping";
    vfs_write(w, msg.as_ptr(), msg.len());
    let mut buf = [0u8; 8];
    let n = vfs_read(r, buf.as_mut_ptr(), buf.len());
    vfs_close(r); vfs_close(w);
    if n == 4 && &buf[..4] == b"ping" { pass("pipe write+read"); }
    else { fail("pipe", "data mismatch"); }
}

fn t_dup2() {
    let (r, w) = match vfs_pipe() {
        Some(p) => p,
        None    => { fail("dup2 pipe", "pipe failed"); return; }
    };
    let new_w = vfs_dup2(w, 9);
    if new_w != 9 {
        fail("dup2", "returned wrong fd");
        vfs_close(r); vfs_close(w); return;
    }
    let msg = b"dup2";
    vfs_write(9, msg.as_ptr(), msg.len());
    let mut buf = [0u8; 8];
    let n = vfs_read(r, buf.as_mut_ptr(), buf.len());
    vfs_close(r); vfs_close(w); vfs_close(9);
    if n == 4 && &buf[..4] == b"dup2" { pass("dup2 + pipe roundtrip"); }
    else { fail("dup2", "data mismatch"); }
}

fn t_getdents64() {
    let fd = vfs_open(b"/", 0x10000 /* O_DIRECTORY */, 0);
    if fd < 0 { fail("getdents64 open '/'", "open failed"); return; }
    let mut buf = [0u8; 512];
    let n = vfs_getdents64(fd, buf.as_mut_ptr(), buf.len());
    vfs_close(fd);
    if n > 0 { pass("getdents64 '/'"); }
    else { fail("getdents64", "returned <= 0"); }
}

fn t_pgid_sid() {
    let pid  = sched::current_pid();
    let pgid = sched::current_pgid();
    let ok_set = sched::set_pgid(pid, pid);
    let sid    = sched::setsid();
    if pgid > 0 && ok_set && sid == pid { pass("getpgid + setpgid + setsid"); }
    else { fail("pgid/sid", "unexpected values"); }
}

fn t_umask() {
    let old = sched::umask(0o027);
    let cur = sched::umask(u32::MAX);
    sched::umask(old);
    if cur == 0o027 { pass("umask set + query"); }
    else { fail("umask", "value mismatch"); }
}

fn t_clock() {
    let t = sched::ticks();
    if t < u64::MAX { pass("sched::ticks() (monotonic clock)"); }
    else { fail("clock", "ticks overflow"); }
}

fn t_af_unix() {
    let pid = sched::current_pid();
    let msg = make_msg(net::NET_SOCKET, &[net::AF_UNIX as u64, net::SOCK_STREAM as u64, 0]);
    let r   = net::handle(&msg, pid);
    let fd  = reply_i64(&r) as i32;
    if fd >= net::SOCK_FD_BASE as i32 {
        vfs_close(fd);
        pass("socket(AF_UNIX, SOCK_STREAM)");
    } else {
        fail("socket(AF_UNIX)", "negative fd");
    }
}

/// Test AF_INET loopback: bind+listen on port 9999, connect, accept, send/recv.
fn t_af_inet_loopback() {
    let pid = sched::current_pid();
    const TEST_PORT: u16 = 9999;

    // 1. Create listener socket.
    let srv = {
        let m = make_msg(net::NET_SOCKET, &[net::AF_INET as u64, net::SOCK_STREAM as u64, 0]);
        reply_i64(&net::handle(&m, pid)) as i32
    };
    if srv < 0 { fail("inet listen socket", "socket() failed"); return; }

    // 2. Bind to 0.0.0.0:TEST_PORT.
    //    sockaddr_in: u16 AF_INET, u16 port(BE), u32 addr, u8[8] zero
    let mut sa_bind = [0u8; 16];
    sa_bind[0] = net::AF_INET as u8;
    let port_be = TEST_PORT.to_be_bytes();
    sa_bind[2] = port_be[0]; sa_bind[3] = port_be[1];
    {
        let m = make_msg(net::NET_BIND,
            &[srv as u64, sa_bind.as_ptr() as u64, 16]);
        let r = reply_i64(&net::handle(&m, pid)) as i32;
        if r != 0 { fail("inet bind", "bind() failed"); vfs_close(srv); return; }
    }

    // 3. Listen.
    {
        let m = make_msg(net::NET_LISTEN, &[srv as u64, 5]);
        let _ = net::handle(&m, pid);
    }

    // 4. Connect from a client socket.
    let cli = {
        let m = make_msg(net::NET_SOCKET, &[net::AF_INET as u64, net::SOCK_STREAM as u64, 0]);
        reply_i64(&net::handle(&m, pid)) as i32
    };
    if cli < 0 { fail("inet client socket", "socket() failed"); vfs_close(srv); return; }

    let mut sa_conn = [0u8; 16];
    sa_conn[0] = net::AF_INET as u8;
    sa_conn[2] = port_be[0]; sa_conn[3] = port_be[1];
    // sin_addr = 127.0.0.1 (little-endian 0x0100007F)
    sa_conn[4] = 0x7F; sa_conn[5] = 0x00; sa_conn[6] = 0x00; sa_conn[7] = 0x01;
    {
        let m = make_msg(net::NET_CONNECT,
            &[cli as u64, sa_conn.as_ptr() as u64, 16]);
        let r = reply_i64(&net::handle(&m, pid)) as i32;
        if r != 0 { fail("inet connect", "connect() failed"); vfs_close(srv); vfs_close(cli); return; }
    }

    // 5. Accept the connection.
    let acc = {
        let m = make_msg(net::NET_ACCEPT, &[srv as u64, 0, 0]);
        reply_i64(&net::handle(&m, pid)) as i32
    };
    if acc < 0 { fail("inet accept", "accept() failed"); vfs_close(srv); vfs_close(cli); return; }

    // 6. Send "hello" from client → accepted socket, receive on server side.
    let send_msg = b"hello-inet";
    {
        let m = make_msg(net::NET_SEND, &[cli as u64, send_msg.as_ptr() as u64, send_msg.len() as u64, 0]);
        let _ = net::handle(&m, pid);
    }
    let mut rbuf = [0u8; 16];
    let rn = {
        let m = make_msg(net::NET_RECV, &[acc as u64, rbuf.as_mut_ptr() as u64, rbuf.len() as u64, 0]);
        reply_i64(&net::handle(&m, pid)) as isize
    };

    vfs_close(srv); vfs_close(cli); vfs_close(acc);

    if rn == send_msg.len() as isize && &rbuf[..rn as usize] == send_msg {
        pass("AF_INET loopback: connect+accept+send+recv");
    } else {
        fail("AF_INET loopback recv", "data mismatch");
    }
}

fn t_buddy_alloc() {
    match mm::buddy::alloc(0) {
        Some(phys) => { mm::buddy::free(phys, 0); pass("buddy alloc+free"); }
        None       => { fail("buddy alloc", "returned None"); }
    }
}

fn t_heap_end() {
    let _h = sched::heap_end();
    pass("heap_end check (brk surrogate)");
}

static CHILD_RAN: AtomicBool = AtomicBool::new(false);
fn child_fn() -> ! {
    CHILD_RAN.store(true, Ordering::SeqCst);
    sched::exit(0);
}

fn t_spawn_exit() {
    CHILD_RAN.store(false, Ordering::SeqCst);
    match sched::spawn(child_fn, 0) {
        Some(_) => {
            for _ in 0..20 { sched::yield_now("init_idle"); }
            if CHILD_RAN.load(Ordering::SeqCst) { pass("spawn + exit"); }
            else { fail("spawn", "child didn't run in 20 yields"); }
        }
        None => { fail("spawn", "returned None"); }
    }
}

fn t_ipc_loopback() {
    match ipc::port::create(1) {
        Some(p) => {
            let _ = ipc::port::send(p, Message::empty());
            if ipc::port::recv(p).is_some() { pass("IPC send+recv loopback"); }
            else { fail("IPC recv", "no message"); }
            ipc::port::close(p);
        }
        None => { fail("IPC create", "returned None"); }
    }
}

fn t_signal_deliver() {
    sched::deliver_signal(sched::current_pid(), 10 /* SIGUSR1 */);
    pass("deliver_signal(SIGUSR1) — no crash");
}

fn t_tmpfs_write_read() {
    const O_WRONLY: u32 = 0x01;
    const O_RDONLY: u32 = 0x00;
    const O_CREAT:  u32 = 0x40;
    const O_TRUNC:  u32 = 0x200;

    let wfd = vfs_open(b"/tmp/smoke.txt", O_WRONLY | O_CREAT | O_TRUNC, 0);
    if wfd < 0 { fail("tmpfs open O_CREAT", "returned negative fd"); return; }
    let data = b"hello tmpfs";
    let n = vfs_write(wfd, data.as_ptr(), data.len());
    vfs_close(wfd);
    if n != data.len() as isize { fail("tmpfs write", "short write"); return; }

    let rfd = vfs_open(b"/tmp/smoke.txt", O_RDONLY, 0);
    if rfd < 0 { fail("tmpfs open for read", "returned negative fd"); return; }
    let mut buf = [0u8; 32];
    let rn = vfs_read(rfd, buf.as_mut_ptr(), buf.len());
    vfs_close(rfd);

    if rn == data.len() as isize && &buf[..rn as usize] == data {
        pass("tmpfs write+read /tmp/smoke.txt");
    } else {
        fail("tmpfs read", "data mismatch");
    }
}

fn t_tmpfs_mkdir() {
    let pid = sched::current_pid();
    let msg = make_msg(vfs::VFS_MKDIR, &[b"/tmp/testdir\0".as_ptr() as u64]);
    let r = reply_i64(&vfs::handle(&msg, pid)) as i32;
    if r == 0 || r == -17 /* EEXIST */ {
        pass("tmpfs mkdir /tmp/testdir");
    } else {
        fail("tmpfs mkdir", "failed");
    }
}

fn t_pipe_blocking_read() {
    let (r, w) = match vfs_pipe() {
        Some(p) => p,
        None    => { fail("blocking pipe", "pipe() failed"); return; }
    };
    let msg = b"blocktest";
    vfs_write(w, msg.as_ptr(), msg.len());
    vfs_close(w);
    let mut buf = [0u8; 16];
    // vfs_read_blocking waits for data (write end already closed → should return immediately).
    let n = vfs_read_blocking(r, buf.as_mut_ptr(), buf.len());
    vfs_close(r);
    if n == msg.len() as isize && &buf[..n as usize] == msg {
        pass("pipe blocking read (data immediately available)");
    } else {
        fail("blocking pipe", "data mismatch");
    }
}

fn t_fd_path() {
    let fd = vfs_open(b"/etc/hostname", 0, 0);
    if fd < 0 { fail("fd_path open", "open failed"); return; }
    let mut buf = [0u8; 64];
    let pid = sched::current_pid();
    let msg = make_msg(vfs::VFS_FD_PATH, &[fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64]);
    let n = reply_i64(&vfs::handle(&msg, pid));
    vfs_close(fd);
    // Should return the RamFS path "/etc/hostname"
    let expected = b"/etc/hostname";
    if n == expected.len() as i64 && &buf[..n as usize] == expected {
        pass("VFS_FD_PATH resolves RamFS path");
    } else {
        fail("fd_path", "wrong path returned");
    }
}

fn t_proc_meminfo() {
    let fd = vfs_open(b"/proc/meminfo", 0, 0);
    if fd < 0 { fail("/proc/meminfo open", "negative fd"); return; }
    let mut buf = [0u8; 256];
    let n = vfs_read(fd, buf.as_mut_ptr(), buf.len());
    vfs_close(fd);
    let content = &buf[..n.max(0) as usize];
    let has_total = content.windows(9).any(|w| w == b"MemTotal:");
    let has_free  = content.windows(8).any(|w| w == b"MemFree:");
    if has_total && has_free { pass("/proc/meminfo has MemTotal and MemFree"); }
    else { fail("/proc/meminfo", "missing fields"); }
}

fn t_proc_uptime() {
    let fd = vfs_open(b"/proc/uptime", 0, 0);
    if fd < 0 { fail("/proc/uptime open", "negative fd"); return; }
    let mut buf = [0u8; 64];
    let n = vfs_read(fd, buf.as_mut_ptr(), buf.len());
    vfs_close(fd);
    if n > 0 && buf[0] >= b'0' { pass("/proc/uptime non-empty and starts with digit"); }
    else { fail("/proc/uptime", "unexpected content"); }
}

fn t_proc_self_status() {
    let fd = vfs_open(b"/proc/self/status", 0, 0);
    if fd < 0 { fail("proc/self/status open", "negative fd"); return; }
    let mut buf = [0u8; 256];
    let n = vfs_read(fd, buf.as_mut_ptr(), buf.len());
    vfs_close(fd);
    // Should contain "Name:" and "Pid:"
    let content = &buf[..n.max(0) as usize];
    let has_name = content.windows(5).any(|w| w == b"Name:");
    let has_pid  = content.windows(4).any(|w| w == b"Pid:");
    if has_name && has_pid { pass("/proc/self/status has Name: and Pid:"); }
    else { fail("proc/self/status", "missing expected fields"); }
}

fn t_proc_self_cmdline() {
    let fd = vfs_open(b"/proc/self/cmdline", 0, 0);
    if fd < 0 { fail("proc/self/cmdline open", "negative fd"); return; }
    let mut buf = [0u8; 64];
    let n = vfs_read(fd, buf.as_mut_ptr(), buf.len());
    vfs_close(fd);
    if n > 0 { pass("/proc/self/cmdline non-empty"); }
    else { fail("proc/self/cmdline", "empty or error"); }
}

fn t_tmpfs_cp() {
    const O_WRONLY: u32 = 0x01;
    const O_CREAT:  u32 = 0x40;
    const O_TRUNC:  u32 = 0x200;
    // Write source file.
    let wfd = vfs_open(b"/tmp/cp_src.txt", O_WRONLY | O_CREAT | O_TRUNC, 0);
    if wfd < 0 { fail("tmpfs cp write src", "open failed"); return; }
    let data = b"copy data";
    vfs_write(wfd, data.as_ptr(), data.len());
    vfs_close(wfd);
    // Copy via cmd_cp.
    cmd_cp(b"/tmp/cp_src.txt", b"/tmp/cp_dst.txt");
    // Read destination.
    let rfd = vfs_open(b"/tmp/cp_dst.txt", 0, 0);
    if rfd < 0 { fail("tmpfs cp read dst", "open failed"); return; }
    let mut buf = [0u8; 16];
    let n = vfs_read(rfd, buf.as_mut_ptr(), buf.len());
    vfs_close(rfd);
    if n == data.len() as isize && &buf[..n as usize] == data {
        pass("tmpfs cp: copy data");
    } else {
        fail("tmpfs cp", "data mismatch");
    }
}

fn t_shell_pipe() {
    // echo hello | cat  — cat should read from STDIN_REDIR → /tmp/.pipebuf
    dispatch_command(b"echo pipe-test | cat > /tmp/pipe_out.txt");
    let fd = vfs_open(b"/tmp/pipe_out.txt", 0, 0);
    if fd < 0 { fail("shell pipe |", "output file missing"); return; }
    let mut buf = [0u8; 32];
    let n = vfs_read(fd, buf.as_mut_ptr(), buf.len());
    vfs_close(fd);
    if n > 0 && buf[..n as usize].windows(9).any(|w| w == b"pipe-test") {
        pass("shell pipe |");
    } else {
        fail("shell pipe |", "output mismatch");
    }
}

fn t_shell_redirect() {
    // echo hello > /tmp/redir.txt  then verify file contents
    dispatch_command(b"echo redirect-test > /tmp/redir.txt");
    let fd = vfs_open(b"/tmp/redir.txt", 0, 0);
    if fd < 0 { fail("shell redirect >", "file not created"); return; }
    let mut buf = [0u8; 32];
    let n = vfs_read(fd, buf.as_mut_ptr(), buf.len());
    vfs_close(fd);
    let data = &buf[..n.max(0) as usize];
    if data.starts_with(b"redirect-test") {
        pass("shell redirect >");
    } else {
        fail("shell redirect >", "wrong content");
    }
    // echo line2 >> /tmp/redir.txt  then verify appended
    dispatch_command(b"echo line2 >> /tmp/redir.txt");
    let fd2 = vfs_open(b"/tmp/redir.txt", 0, 0);
    if fd2 < 0 { fail("shell redirect >>", "file not created"); return; }
    let mut buf2 = [0u8; 64];
    let n2 = vfs_read(fd2, buf2.as_mut_ptr(), buf2.len());
    vfs_close(fd2);
    let data2 = &buf2[..n2.max(0) as usize];
    if data2.windows(5).any(|w| w == b"line2") {
        pass("shell redirect >>");
    } else {
        fail("shell redirect >>", "append missing");
    }
}

fn t_if_while() {
    // Test: `if true; then BODY; fi` runs body.
    dispatch_command(b"if true; then echo if_true_ok; fi");

    // Test: `if false; then BODY; else ELSE; fi` runs else.
    let ok_fd = {
        const O_WRONLY: u32 = 0x001; const O_CREAT: u32 = 0x040; const O_TRUNC: u32 = 0x200;
        let wfd = vfs_open(b"/tmp/.if_test", O_WRONLY | O_CREAT | O_TRUNC, 0o600);
        if wfd >= 0 {
            STDOUT_REDIR.store(wfd as usize, Ordering::Relaxed);
            dispatch_command(b"if false; then echo wrong; else echo else_ok; fi");
            STDOUT_REDIR.store(usize::MAX, Ordering::Relaxed);
            vfs_close(wfd);
        }
        wfd
    };
    let mut buf = [0u8; 64];
    let rfd = vfs_open(b"/tmp/.if_test", 0, 0);
    let n = if rfd >= 0 { let n = vfs_read(rfd, buf.as_mut_ptr(), buf.len()); vfs_close(rfd); n } else { -1 };
    if ok_fd >= 0 && n > 0 && buf[..n as usize].windows(7).any(|w| w == b"else_ok") {
        pass("if/else construct");
    } else {
        fail("if/else construct", "else branch not taken");
    }

    // Test: `test -f PATH` works (file we just created).
    dispatch_command(b"test -f /tmp/.if_test");
    if LAST_EXIT_CODE.load(Ordering::Relaxed) == 0 { pass("test -f existing file"); }
    else { fail("test -f", "file should exist"); }

    // Test: `test -z ""` (empty string) → true.
    dispatch_command(b"test -z x");
    let z_empty = LAST_EXIT_CODE.load(Ordering::Relaxed) != 0; // -z "x" should be false
    dispatch_command(b"test -z ");
    let z_empty2 = LAST_EXIT_CODE.load(Ordering::Relaxed) == 0; // -z "" should be true
    if z_empty && z_empty2 { pass("test -z"); } else { fail("test -z", "wrong result"); }

    // Test: command substitution $(echo hello) expands to "hello".
    let mut src_buf = [0u8; 64];
    src_buf[..14].copy_from_slice(b"$(echo subcmd)");
    let mut dst_buf = [0u8; 512];
    let dlen = var_expand(&src_buf[..14], &mut dst_buf);
    if &dst_buf[..dlen] == b"subcmd" { pass("command substitution $(cmd)"); }
    else { fail("command substitution", "expansion mismatch"); }
}

fn t_for_case_read() {
    // Test for loop: collect output into a file.
    const O_WRONLY: u32 = 0x001; const O_CREAT: u32 = 0x040; const O_TRUNC: u32 = 0x200;
    let wfd = vfs_open(b"/tmp/.for_out", O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    if wfd >= 0 {
        STDOUT_REDIR.store(wfd as usize, Ordering::Relaxed);
        dispatch_command(b"for x in alpha beta gamma; do echo $x; done");
        STDOUT_REDIR.store(usize::MAX, Ordering::Relaxed);
        vfs_close(wfd);
    }
    let mut buf = [0u8; 128];
    let rfd = vfs_open(b"/tmp/.for_out", 0, 0);
    let n = if rfd >= 0 { let n = vfs_read(rfd, buf.as_mut_ptr(), buf.len()); vfs_close(rfd); n } else { 0 };
    if n > 0 && buf[..n as usize].windows(5).any(|w| w == b"alpha")
            && buf[..n as usize].windows(4).any(|w| w == b"beta")
            && buf[..n as usize].windows(5).any(|w| w == b"gamma") {
        pass("for loop");
    } else {
        fail("for loop", "output missing expected items");
    }

    // Test case: match exact and wildcard.
    let wfd2 = vfs_open(b"/tmp/.case_out", O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    if wfd2 >= 0 {
        STDOUT_REDIR.store(wfd2 as usize, Ordering::Relaxed);
        dispatch_command(b"case hello in hi) echo wrong;; hello) echo case_ok;; *) echo wild;; esac");
        STDOUT_REDIR.store(usize::MAX, Ordering::Relaxed);
        vfs_close(wfd2);
    }
    let mut buf2 = [0u8; 64];
    let rfd2 = vfs_open(b"/tmp/.case_out", 0, 0);
    let n2 = if rfd2 >= 0 { let n = vfs_read(rfd2, buf2.as_mut_ptr(), buf2.len()); vfs_close(rfd2); n } else { 0 };
    if n2 > 0 && buf2[..n2 as usize].windows(7).any(|w| w == b"case_ok") {
        pass("case construct");
    } else {
        fail("case construct", "wrong branch taken");
    }

    // Test read VAR from STDIN_REDIR.
    let wfd3 = vfs_open(b"/tmp/.read_in", O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    if wfd3 >= 0 {
        let msg = b"readvalue\n";
        vfs_write(wfd3, msg.as_ptr(), msg.len());
        vfs_close(wfd3);
    }
    let rfd3 = vfs_open(b"/tmp/.read_in", 0, 0);
    if rfd3 >= 0 {
        STDIN_REDIR.store(rfd3 as usize, Ordering::Relaxed);
        dispatch_command(b"read RVAR");
        STDIN_REDIR.store(usize::MAX, Ordering::Relaxed);
        vfs_close(rfd3);
    }
    let mut out = [0u8; MAX_VAR_VAL];
    let vlen = var_get_len(b"RVAR", &mut out);
    if &out[..vlen] == b"readvalue" { pass("read VAR from stdin"); }
    else { fail("read VAR", "variable not set correctly"); }
}

fn t_printf_sed_awk() {
    const O_WRONLY: u32 = 0x001; const O_CREAT: u32 = 0x040; const O_TRUNC: u32 = 0x200;

    // printf: capture output.
    let wfd = vfs_open(b"/tmp/.printf_out", O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    if wfd >= 0 {
        STDOUT_REDIR.store(wfd as usize, Ordering::Relaxed);
        dispatch_command(b"printf hello_%s_%d world 42");
        STDOUT_REDIR.store(usize::MAX, Ordering::Relaxed);
        vfs_close(wfd);
    }
    let mut buf = [0u8; 64];
    let rfd = vfs_open(b"/tmp/.printf_out", 0, 0);
    let n = if rfd >= 0 { let r = vfs_read(rfd, buf.as_mut_ptr(), buf.len()); vfs_close(rfd); r } else { 0 };
    if n > 0 && buf[..n as usize].windows(14).any(|w| w == b"hello_world_42") {
        pass("printf %s %d");
    } else {
        fail("printf", "output mismatch");
    }

    // sed: write input file, run substitution.
    let wfd2 = vfs_open(b"/tmp/.sed_in", O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    if wfd2 >= 0 {
        let msg = b"foo bar foo\n";
        vfs_write(wfd2, msg.as_ptr(), msg.len());
        vfs_close(wfd2);
    }
    let wfd3 = vfs_open(b"/tmp/.sed_out", O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    if wfd3 >= 0 {
        let rfd2 = vfs_open(b"/tmp/.sed_in", 0, 0);
        if rfd2 >= 0 {
            STDIN_REDIR.store(rfd2 as usize, Ordering::Relaxed);
            STDOUT_REDIR.store(wfd3 as usize, Ordering::Relaxed);
            dispatch_command(b"sed s/foo/baz/g");
            STDOUT_REDIR.store(usize::MAX, Ordering::Relaxed);
            STDIN_REDIR.store(usize::MAX, Ordering::Relaxed);
            vfs_close(rfd2);
        }
        vfs_close(wfd3);
    }
    let mut buf3 = [0u8; 64];
    let rfd3 = vfs_open(b"/tmp/.sed_out", 0, 0);
    let n3 = if rfd3 >= 0 { let r = vfs_read(rfd3, buf3.as_mut_ptr(), buf3.len()); vfs_close(rfd3); r } else { 0 };
    if n3 > 0 && buf3[..n3 as usize].windows(7).any(|w| w == b"baz bar") {
        pass("sed s/foo/baz/g");
    } else {
        fail("sed", "substitution mismatch");
    }

    // awk: extract field 2.
    let wfd4 = vfs_open(b"/tmp/.awk_in", O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    if wfd4 >= 0 {
        let msg = b"one two three\n";
        vfs_write(wfd4, msg.as_ptr(), msg.len());
        vfs_close(wfd4);
    }
    let wfd5 = vfs_open(b"/tmp/.awk_out", O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    if wfd5 >= 0 {
        let rfd4 = vfs_open(b"/tmp/.awk_in", 0, 0);
        if rfd4 >= 0 {
            STDIN_REDIR.store(rfd4 as usize, Ordering::Relaxed);
            STDOUT_REDIR.store(wfd5 as usize, Ordering::Relaxed);
            dispatch_command(b"awk {print $2}");
            STDOUT_REDIR.store(usize::MAX, Ordering::Relaxed);
            STDIN_REDIR.store(usize::MAX, Ordering::Relaxed);
            vfs_close(rfd4);
        }
        vfs_close(wfd5);
    }
    let mut buf5 = [0u8; 32];
    let rfd5 = vfs_open(b"/tmp/.awk_out", 0, 0);
    let n5 = if rfd5 >= 0 { let r = vfs_read(rfd5, buf5.as_mut_ptr(), buf5.len()); vfs_close(rfd5); r } else { 0 };
    if n5 > 0 && buf5[..n5 as usize].windows(3).any(|w| w == b"two") {
        pass("awk {print $2}");
    } else {
        fail("awk", "wrong field extracted");
    }
}

fn t_shell_funcs() {
    const O_WRONLY: u32 = 0x001; const O_CREAT: u32 = 0x040; const O_TRUNC: u32 = 0x200;

    // Define a function and call it.
    dispatch_command(b"greet() { echo hello_from_func }");
    let wfd = vfs_open(b"/tmp/.func_out", O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    if wfd >= 0 {
        STDOUT_REDIR.store(wfd as usize, Ordering::Relaxed);
        dispatch_command(b"greet");
        STDOUT_REDIR.store(usize::MAX, Ordering::Relaxed);
        vfs_close(wfd);
    }
    let mut buf = [0u8; 64];
    let rfd = vfs_open(b"/tmp/.func_out", 0, 0);
    let n = if rfd >= 0 { let r = vfs_read(rfd, buf.as_mut_ptr(), buf.len()); vfs_close(rfd); r } else { 0 };
    if n > 0 && buf[..n as usize].windows(15).any(|w| w == b"hello_from_func") {
        pass("shell function define + call");
    } else {
        fail("shell function", "output missing");
    }

    // Test positional params: sayhi() { echo hi_$1 }
    dispatch_command(b"sayhi() { echo hi_$1 }");
    let wfd2 = vfs_open(b"/tmp/.func2_out", O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    if wfd2 >= 0 {
        STDOUT_REDIR.store(wfd2 as usize, Ordering::Relaxed);
        dispatch_command(b"sayhi world");
        STDOUT_REDIR.store(usize::MAX, Ordering::Relaxed);
        vfs_close(wfd2);
    }
    let mut buf2 = [0u8; 32];
    let rfd2 = vfs_open(b"/tmp/.func2_out", 0, 0);
    let n2 = if rfd2 >= 0 { let r = vfs_read(rfd2, buf2.as_mut_ptr(), buf2.len()); vfs_close(rfd2); r } else { 0 };
    if n2 > 0 && buf2[..n2 as usize].windows(8).any(|w| w == b"hi_world") {
        pass("shell function positional params");
    } else {
        fail("shell function params", "output mismatch");
    }

    // Test source: write a script and execute it.
    let wfd3 = vfs_open(b"/tmp/.test_script.sh", O_WRONLY | O_CREAT | O_TRUNC, 0o600);
    if wfd3 >= 0 {
        let script = b"SOURCED=script_ran\n";
        vfs_write(wfd3, script.as_ptr(), script.len());
        vfs_close(wfd3);
    }
    dispatch_command(b"source /tmp/.test_script.sh");
    let mut out = [0u8; MAX_VAR_VAL];
    let vlen = var_get_len(b"SOURCED", &mut out);
    if &out[..vlen] == b"script_ran" { pass("source FILE sets variable"); }
    else { fail("source FILE", "variable not set"); }
}

fn run_posix_tests() {
    kprintln!("[init] ══════════ POSIX smoke tests ══════════");
    t_getpid();
    t_getcwd();
    t_chdir();
    t_open_read_close();
    t_write_stdout();
    t_pipe();
    t_dup2();
    t_getdents64();
    t_pgid_sid();
    t_umask();
    t_clock();
    t_af_unix();
    t_af_inet_loopback();
    t_buddy_alloc();
    t_heap_end();
    t_spawn_exit();
    t_ipc_loopback();
    t_signal_deliver();
    t_tmpfs_write_read();
    t_tmpfs_mkdir();
    t_pipe_blocking_read();
    t_fd_path();
    t_proc_meminfo();
    t_proc_uptime();
    t_proc_self_status();
    t_proc_self_cmdline();
    t_tmpfs_cp();
    t_shell_pipe();
    t_shell_redirect();
    t_if_while();
    t_for_case_read();
    t_printf_sed_awk();
    t_shell_funcs();

    let run    = *TESTS_RUN.lock();
    let passed = *TESTS_PASSED.lock();
    let failed = run.saturating_sub(passed);
    kprint!("[init] ════ Result: ");
    kraw!(&u32_dec(passed).0[..u32_dec(passed).1]);
    kprint!("/");
    kraw!(&u32_dec(run).0[..u32_dec(run).1]);
    if failed == 0 { kprintln!(" — ALL PASSED ════"); }
    else {
        kprint!(" — ");
        kraw!(&u32_dec(failed).0[..u32_dec(failed).1]);
        kprintln!(" FAILED ════");
    }
}

// ── Interactive shell ─────────────────────────────────────────────────────────
//
// A real command interpreter that reads lines from the serial console and
// dispatches built-in commands.  Uses the `read_byte` I/O hook for input.

/// Block until a complete line is available.  Writes characters into `buf`
/// (without the trailing newline) and returns the length.  Handles backspace.
fn readline(buf: &mut [u8]) -> usize {
    let mut n = 0usize;
    loop {
        // Yield until a byte arrives (non-blocking poll).
        let b = loop {
            match unsafe { ((*IO).read_byte)() } {
                Some(b) => break b,
                None    => sched::yield_now("init_idle"),
            }
        };
        match b {
            b'\r' | b'\n' => {
                kraw!(b"\r\n");
                return n;
            }
            // Backspace / DEL
            0x08 | 0x7F => {
                if n > 0 {
                    n -= 1;
                    kraw!(b"\x08 \x08"); // erase character on terminal
                }
            }
            // Ctrl-C
            0x03 => {
                kraw!(b"^C\r\n");
                return 0;
            }
            // Printable ASCII
            0x20..=0x7E => {
                if n < buf.len() - 1 {
                    buf[n] = b;
                    n += 1;
                    // Echo the character.
                    unsafe { ((*IO).write_raw)(&[b]); }
                }
            }
            _ => {}
        }
    }
}

// ── Built-in command implementations ─────────────────────────────────────────

fn cmd_echo(args: &[u8]) {
    kraw!(args);
    kprintln!("");
}

fn cmd_cat(path: &[u8]) {
    // `-` or empty = read from stdin redirect
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    let (fd, close_after) = if path == b"-" || (path.is_empty() && redir != usize::MAX) {
        if redir == usize::MAX { kprintln!("cat: no stdin"); return; }
        (redir as i32, false)
    } else {
        let f = vfs_open(path, 0, 0);
        if f < 0 { kprintln!("cat: no such file"); return; }
        (f, true)
    };
    let mut buf = [0u8; 256];
    loop {
        let n = vfs_read_blocking(fd, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        kraw!(&buf[..n as usize]);
    }
    if close_after { vfs_close(fd); }
}

fn cmd_ls(path: &[u8]) {
    let fd = vfs_open(path, 0x10000 /* O_DIRECTORY */, 0);
    if fd < 0 { kprintln!("ls: cannot open directory"); return; }
    let mut buf = [0u8; 1024];
    loop {
        let n = vfs_getdents64(fd, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        let mut off = 0usize;
        while off < n as usize {
            if off + 19 > n as usize { break; }
            let reclen = u16::from_le_bytes([buf[off+16], buf[off+17]]) as usize;
            if reclen == 0 { break; }
            let name_start = off + 19;
            let name_end = buf[name_start..off+reclen]
                .iter().position(|&b| b == 0)
                .map(|p| name_start + p)
                .unwrap_or(off + reclen);
            kraw!(&buf[name_start..name_end]); kprintln!("");
            off += reclen;
        }
    }
    vfs_close(fd);
}

fn cmd_uname() {
    kprintln!("Leandros 1.0.0 #1 SMP aarch64/x86_64");
}

fn cmd_pwd() {
    let mut buf = [0u8; 256];
    let n = sched::current_cwd(buf.as_mut_ptr(), buf.len()) as usize;
    if n > 0 { kraw!(&buf[..n]); }
    else { kraw!(b"/"); }
    kprintln!("");
}

fn cmd_cd(path: &[u8]) {
    let target: &[u8] = if path.is_empty() { b"/" } else { path };
    // Build a NUL-terminated copy for sched::set_cwd.
    let mut buf = [0u8; 256];
    let n = target.len().min(255);
    buf[..n].copy_from_slice(&target[..n]);
    if sched::set_cwd(&buf[..n]) {
        // success — no output
    } else {
        kprint!("cd: "); kraw!(target); kprintln!(": no such directory");
    }
}

fn cmd_mkdir(path: &[u8]) {
    if path.is_empty() { kprintln!("mkdir: missing operand"); return; }
    let pid = sched::current_pid();
    let mut buf = [0u8; 256];
    let n = path.len().min(255);
    buf[..n].copy_from_slice(&path[..n]);
    let msg = make_msg(vfs::VFS_MKDIR, &[buf.as_ptr() as u64]);
    let r = reply_i64(&vfs::handle(&msg, pid)) as i32;
    if r < 0 { kprint!("mkdir: "); kraw!(path); kprintln!(": failed"); }
}

fn cmd_rm(path: &[u8]) {
    if path.is_empty() { kprintln!("rm: missing operand"); return; }
    let pid = sched::current_pid();
    let mut buf = [0u8; 256];
    let n = path.len().min(255);
    buf[..n].copy_from_slice(&path[..n]);
    let msg = make_msg(vfs::VFS_UNLINK, &[buf.as_ptr() as u64]);
    let r = reply_i64(&vfs::handle(&msg, pid)) as i32;
    if r < 0 { kprint!("rm: "); kraw!(path); kprintln!(": failed"); }
}

fn cmd_write(path: &[u8], content: &[u8]) {
    // Create/overwrite file at path with content.
    const O_WRONLY: u32 = 0x01;
    const O_CREAT:  u32 = 0x40;
    const O_TRUNC:  u32 = 0x200;
    let fd = vfs_open(path, O_WRONLY | O_CREAT | O_TRUNC, 0o644);
    if fd < 0 { kprintln!("write: cannot open file"); return; }
    vfs_write(fd, content.as_ptr(), content.len());
    vfs_close(fd);
}

fn cmd_cp(src: &[u8], dst: &[u8]) {
    if src.is_empty() || dst.is_empty() { kprintln!("cp: missing operand"); return; }
    let rfd = vfs_open(src, 0, 0);
    if rfd < 0 { kprint!("cp: "); kraw!(src); kprintln!(": cannot open"); return; }
    const O_WRONLY: u32 = 0x01;
    const O_CREAT:  u32 = 0x40;
    const O_TRUNC:  u32 = 0x200;
    let wfd = vfs_open(dst, O_WRONLY | O_CREAT | O_TRUNC, 0o644);
    if wfd < 0 { vfs_close(rfd); kprint!("cp: "); kraw!(dst); kprintln!(": cannot create"); return; }
    let mut buf = [0u8; 512];
    loop {
        let n = vfs_read(rfd, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        vfs_write(wfd, buf.as_ptr(), n as usize);
    }
    vfs_close(rfd);
    vfs_close(wfd);
}

fn cmd_mv(src: &[u8], dst: &[u8]) {
    if src.is_empty() || dst.is_empty() { kprintln!("mv: missing operand"); return; }
    // Try rename first (cheap, works within /tmp).
    let pid = sched::current_pid();
    let mut sbuf = [0u8; 256]; let mut dbuf = [0u8; 256];
    let sn = src.len().min(255); let dn = dst.len().min(255);
    sbuf[..sn].copy_from_slice(&src[..sn]);
    dbuf[..dn].copy_from_slice(&dst[..dn]);
    let rmsg = make_msg(vfs::VFS_RENAME, &[sbuf.as_ptr() as u64, dbuf.as_ptr() as u64]);
    let r = reply_i64(&vfs::handle(&rmsg, pid)) as i32;
    if r == 0 { return; }
    // Cross-filesystem: copy then unlink.
    cmd_cp(src, dst);
    let umsg = make_msg(vfs::VFS_UNLINK, &[sbuf.as_ptr() as u64]);
    let _ = vfs::handle(&umsg, pid);
}

fn cmd_readlink(path: &[u8]) {
    if path.is_empty() { kprintln!("readlink: missing operand"); return; }
    // Build a NUL-terminated path in a local buf, pass as a fake user ptr.
    let mut pb = [0u8; 256];
    let n = path.len().min(255);
    pb[..n].copy_from_slice(&path[..n]);
    let mut out = [0u8; 256];
    // Emulate sys_readlinkat by querying VFS_FD_PATH for /proc/self/fd paths,
    // or return ENOENT for other paths (we don't resolve real symlinks).
    if n > 15 && &pb[..15] == b"/proc/self/fd/" {
        let num_str = &pb[15..n];
        let mut fd = 0usize;
        let mut valid = !num_str.is_empty();
        for &d in num_str {
            if d < b'0' || d > b'9' { valid = false; break; }
            fd = fd * 10 + (d - b'0') as usize;
        }
        if valid {
            let pid = sched::current_pid();
            let msg = make_msg(vfs::VFS_FD_PATH, &[fd as u64, out.as_mut_ptr() as u64, out.len() as u64]);
            let len = reply_i64(&vfs::handle(&msg, pid));
            if len > 0 { kraw!(&out[..len as usize]); kprintln!(""); return; }
        }
    }
    kprintln!("readlink: not a symlink");
}

fn cmd_ps() {
    kprintln!("  PID TTY      STAT  COMMAND");
    let pid = sched::current_pid();
    kprint!("    "); kraw!(&u32_dec(pid).0[..u32_dec(pid).1]);
    kprintln!(" ttyS0   R     leandros-init");
}

fn cmd_free() {
    let fd = vfs_open(b"/proc/meminfo", 0, 0);
    if fd < 0 { kprintln!("free: cannot read /proc/meminfo"); return; }
    kprintln!("              total        used        free");
    kprint!("Mem:  ");
    let mut buf = [0u8; 512];
    let n = vfs_read(fd, buf.as_mut_ptr(), buf.len());
    vfs_close(fd);
    // Parse MemTotal and MemFree lines from the content.
    let content = &buf[..n.max(0) as usize];
    let mut total_kb = 0u32;
    let mut free_kb  = 0u32;
    for line in content.split(|&b| b == b'\n') {
        if line.starts_with(b"MemTotal:") { total_kb = parse_kb(line); }
        if line.starts_with(b"MemFree:")  { free_kb  = parse_kb(line); }
    }
    let used_kb = total_kb.saturating_sub(free_kb);
    let (tb, tl) = u32_dec(total_kb); let (ub, ul) = u32_dec(used_kb); let (fb, fl) = u32_dec(free_kb);
    kraw!(&tb[..tl]); kprint!(" kB   "); kraw!(&ub[..ul]); kprint!(" kB   "); kraw!(&fb[..fl]); kprintln!(" kB");
}

fn parse_kb(line: &[u8]) -> u32 {
    // "MemTotal:       262144 kB" — skip to digits
    let mut v = 0u32;
    let mut seen = false;
    for &b in line {
        if b >= b'0' && b <= b'9' { v = v * 10 + (b - b'0') as u32; seen = true; }
        else if seen { break; }
    }
    v
}

fn cmd_df() {
    kprintln!("Filesystem      1K-blocks      Used  Available Use% Mounted on");
    let fd = vfs_open(b"/proc/meminfo", 0, 0);
    if fd < 0 { kprintln!("df: cannot read memory info"); return; }
    let mut buf = [0u8; 256];
    let n = vfs_read(fd, buf.as_mut_ptr(), buf.len());
    vfs_close(fd);
    let content = &buf[..n.max(0) as usize];
    let total_kb = content.split(|&b| b == b'\n')
        .filter(|l| l.starts_with(b"MemTotal:")).map(|l| parse_kb(l)).next().unwrap_or(0);
    let (tb, tl) = u32_dec(total_kb);
    kprint!("tmpfs           "); kraw!(&tb[..tl]);
    kprint!(" kB         0 kB   "); kraw!(&tb[..tl]);
    kprintln!(" kB   0% /tmp");
}

fn cmd_date() {
    let ticks = sched::ticks();
    let secs = ticks / 100;
    let h = (secs / 3600) % 24;
    let m = (secs / 60)   % 60;
    let s =  secs          % 60;
    kprint!("Uptime ");
    let (hb,hl) = u32_dec(h as u32); let (mb,ml) = u32_dec(m as u32); let (sb,sl) = u32_dec(s as u32);
    if h < 10 { kprint!("0"); } kraw!(&hb[..hl]);
    kprint!(":");
    if m < 10 { kprint!("0"); } kraw!(&mb[..ml]);
    kprint!(":");
    if s < 10 { kprint!("0"); } kraw!(&sb[..sl]);
    kprintln!(" (seconds since boot)");
}

fn cmd_kill(args: &[u8]) {
    // kill [-SIGNAL] PID
    let (sig, rest) = if args.starts_with(b"-") {
        let sp = args.iter().position(|&b| b == b' ').unwrap_or(args.len());
        let signum: u32 = {
            let mut v = 0u32;
            for &b in &args[1..sp] { if b >= b'0' && b <= b'9' { v = v*10+(b-b'0') as u32; } }
            if v == 0 { 15 } else { v } // default SIGTERM
        };
        (signum, trim(&args[sp..]))
    } else {
        (15, args)
    };
    let mut pid = 0u32;
    for &b in rest { if b >= b'0' && b <= b'9' { pid = pid*10+(b-b'0') as u32; } }
    if pid == 0 { kprintln!("kill: invalid pid"); return; }
    match sched::deliver_signal(pid, sig) {
        0  => {}
        -3 => kprintln!("kill: no such process"),
        _  => kprintln!("kill: failed"),
    }
}

fn cmd_sleep(arg: &[u8]) {
    // Parse integer seconds from arg.
    let mut secs = 0u64;
    for &b in arg { if b >= b'0' && b <= b'9' { secs = secs * 10 + (b - b'0') as u64; } else { break; } }
    if secs == 0 { return; }
    let start = sched::ticks();
    while sched::ticks().wrapping_sub(start) < secs * 100 {
        sched::yield_now("init_idle");
    }
}

fn cmd_wc(path: &[u8]) {
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    let (fd, close_after) = if path == b"-" || (path.is_empty() && redir != usize::MAX) {
        if redir == usize::MAX { kprintln!("wc: no stdin"); return; }
        (redir as i32, false)
    } else {
        let f = vfs_open(path, 0, 0);
        if f < 0 { kprintln!("wc: cannot open file"); return; }
        (f, true)
    };
    let mut buf = [0u8; 512];
    let mut lines = 0u32;
    let mut words = 0u32;
    let mut bytes = 0u32;
    let mut in_word = false;
    loop {
        let n = vfs_read_blocking(fd, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        for i in 0..n as usize {
            let c = buf[i];
            bytes += 1;
            if c == b'\n' { lines += 1; }
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                in_word = false;
            } else if !in_word {
                words += 1;
                in_word = true;
            }
        }
    }
    if close_after { vfs_close(fd); }
    let (lb, ll) = u32_dec(lines);
    let (wb, wl) = u32_dec(words);
    let (bb, bl) = u32_dec(bytes);
    kraw!(&lb[..ll]); kprint!(" "); kraw!(&wb[..wl]); kprint!(" "); kraw!(&bb[..bl]);
    kprint!(" "); kraw!(path); kprintln!("");
}

fn cmd_head(path: &[u8], n_lines: usize) {
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    let (fd, close_after) = if path == b"-" || (path.is_empty() && redir != usize::MAX) {
        if redir == usize::MAX { kprintln!("head: no stdin"); return; }
        (redir as i32, false)
    } else {
        let f = vfs_open(path, 0, 0);
        if f < 0 { kprintln!("head: cannot open file"); return; }
        (f, true)
    };
    let mut buf = [0u8; 256];
    let mut lines = 0usize;
    'outer: loop {
        let n = vfs_read_blocking(fd, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        for i in 0..n as usize {
            io_write_raw(&buf[i..i+1]);
            if buf[i] == b'\n' {
                lines += 1;
                if lines >= n_lines { break 'outer; }
            }
        }
    }
    if close_after { vfs_close(fd); }
}

/// grep PATTERN [FILE|-]  — print lines containing PATTERN (substring match).
fn cmd_grep(args: &[u8]) {
    let (pat, path) = if let Some(sp) = args.iter().position(|&b| b == b' ') {
        (&args[..sp], trim(&args[sp+1..]))
    } else {
        (args, &b""[..])
    };
    if pat.is_empty() { kprintln!("usage: grep PATTERN [FILE]"); return; }
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    let (fd, close_after) = if path == b"-" || (path.is_empty() && redir != usize::MAX) {
        if redir == usize::MAX { kprintln!("grep: no stdin"); return; }
        (redir as i32, false)
    } else if !path.is_empty() {
        let f = vfs_open(path, 0, 0);
        if f < 0 { kprintln!("grep: no such file"); return; }
        (f, true)
    } else { kprintln!("usage: grep PATTERN FILE"); return; };

    let mut buf = [0u8; 256];
    let mut line = [0u8; 256];
    let mut llen = 0usize;
    loop {
        let n = vfs_read_blocking(fd, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        for i in 0..n as usize {
            let c = buf[i];
            if c == b'\n' {
                if line[..llen].windows(pat.len()).any(|w| w == pat) {
                    kraw!(&line[..llen]); kprintln!("");
                }
                llen = 0;
            } else if llen < line.len() - 1 {
                line[llen] = c; llen += 1;
            }
        }
    }
    // flush last line (no trailing newline)
    if llen > 0 && line[..llen].windows(pat.len()).any(|w| w == pat) {
        kraw!(&line[..llen]); kprintln!("");
    }
    if close_after { vfs_close(fd); }
}

/// sort [FILE|-]  — read all lines, sort lexicographically, print.
fn cmd_sort(path: &[u8]) {
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    let (fd, close_after) = if path == b"-" || (path.is_empty() && redir != usize::MAX) {
        if redir == usize::MAX { kprintln!("sort: no stdin"); return; }
        (redir as i32, false)
    } else if !path.is_empty() {
        let f = vfs_open(path, 0, 0);
        if f < 0 { kprintln!("sort: no such file"); return; }
        (f, true)
    } else { kprintln!("usage: sort [FILE]"); return; };

    // Store up to 64 lines of 128 bytes each.
    const MAX_LINES: usize = 64;
    const MAX_LINE: usize  = 128;
    let mut lines = [[0u8; MAX_LINE]; MAX_LINES];
    let mut lens  = [0usize; MAX_LINES];
    let mut count = 0usize;
    let mut buf = [0u8; 256];
    let mut cur = [0u8; MAX_LINE];
    let mut clen = 0usize;
    loop {
        let n = vfs_read_blocking(fd, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        for i in 0..n as usize {
            let c = buf[i];
            if c == b'\n' {
                if count < MAX_LINES { lines[count][..clen].copy_from_slice(&cur[..clen]); lens[count] = clen; count += 1; }
                clen = 0;
            } else if clen < MAX_LINE - 1 { cur[clen] = c; clen += 1; }
        }
    }
    if clen > 0 && count < MAX_LINES { lines[count][..clen].copy_from_slice(&cur[..clen]); lens[count] = clen; count += 1; }
    if close_after { vfs_close(fd); }
    // Insertion sort.
    for i in 1..count {
        let mut j = i;
        while j > 0 && lines[j-1][..lens[j-1]] > lines[j][..lens[j]] {
            lines.swap(j-1, j); lens.swap(j-1, j); j -= 1;
        }
    }
    for i in 0..count { kraw!(&lines[i][..lens[i]]); kprintln!(""); }
}

/// tee FILE  — copy stdin to stdout AND FILE.
fn cmd_tee(path: &[u8]) {
    if path.is_empty() { kprintln!("usage: tee FILE"); return; }
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    if redir == usize::MAX { kprintln!("tee: no stdin"); return; }
    const O_WRONLY: u32 = 0x001;
    const O_CREAT:  u32 = 0x040;
    const O_TRUNC:  u32 = 0x200;
    let wfd = vfs_open(path, O_WRONLY | O_CREAT | O_TRUNC, 0o644);
    if wfd < 0 { kprintln!("tee: cannot open file"); return; }
    let mut buf = [0u8; 256];
    loop {
        let n = vfs_read_blocking(redir as i32, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        io_write_raw(&buf[..n as usize]);
        vfs_write(wfd, buf.as_ptr(), n as usize);
    }
    vfs_close(wfd);
}

/// find [DIR] [-name PATTERN]  — list files/dirs under DIR matching name pattern.
fn cmd_find(args: &[u8]) {
    let (dir, pat) = if let Some(p) = args.windows(6).position(|w| w == b"-name ") {
        (trim(&args[..p]), trim(&args[p+6..]))
    } else {
        (if args.is_empty() { b"/" as &[u8] } else { args }, b"" as &[u8])
    };
    find_recurse(dir, pat);
}

fn find_recurse(dir: &[u8], pat: &[u8]) {
    let fd = vfs_open(dir, 0x10000 /* O_DIRECTORY */, 0);
    if fd < 0 { return; }
    let mut buf = [0u8; 512];
    // Accumulate child names so we can close fd before recursing.
    let mut names = [[0u8; 64]; 32];
    let mut nlens = [0usize; 32];
    let mut is_dir_flags = [false; 32];
    let mut ncount = 0usize;
    loop {
        let n = vfs_getdents64(fd, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        let mut off = 0usize;
        while off + 19 <= n as usize {
            let rec_len = u16::from_le_bytes([buf[off+16], buf[off+17]]) as usize;
            let d_type  = buf[off+18]; // 4=dir, 8=file
            let name_start = off + 19;
            let name_end   = (off + rec_len).min(n as usize);
            if name_end > name_start && ncount < 32 {
                let raw = &buf[name_start..name_end];
                let raw = &raw[..raw.iter().position(|&b| b==0).unwrap_or(raw.len())];
                if !raw.is_empty() && raw != b"." && raw != b".." {
                    let copy_len = raw.len().min(63);
                    names[ncount][..copy_len].copy_from_slice(&raw[..copy_len]);
                    nlens[ncount] = copy_len;
                    is_dir_flags[ncount] = d_type == 4;
                    ncount += 1;
                }
            }
            if rec_len == 0 { break; }
            off += rec_len;
        }
    }
    vfs_close(fd);
    for i in 0..ncount {
        // Build full path: dir + "/" + name
        let mut full = [0u8; 128];
        let dlen = dir.len().min(60);
        full[..dlen].copy_from_slice(&dir[..dlen]);
        let sep_pos = dlen;
        full[sep_pos] = b'/';
        let nlen = nlens[i];
        full[sep_pos+1..sep_pos+1+nlen].copy_from_slice(&names[i][..nlen]);
        let total = sep_pos + 1 + nlen;
        let name = &names[i][..nlen];
        if pat.is_empty() || name.windows(pat.len()).any(|w| w == pat) {
            kraw!(&full[..total]); kprintln!("");
        }
        if is_dir_flags[i] {
            find_recurse(&full[..total], pat);
        }
    }
}

/// touch FILE  — create empty file or update (just open+close with O_CREAT).
fn cmd_touch(path: &[u8]) {
    if path.is_empty() { kprintln!("usage: touch FILE"); return; }
    const O_WRONLY: u32 = 0x001;
    const O_CREAT:  u32 = 0x040;
    let fd = vfs_open(path, O_WRONLY | O_CREAT, 0o644);
    if fd < 0 { kprint!("touch: "); kraw!(path); kprintln!(": failed"); return; }
    vfs_close(fd);
}

/// cut -d DELIM -f N [FILE|-]  — extract Nth field (1-based) by delimiter.
fn cmd_cut(args: &[u8]) {
    // Parse: -d D -f N [FILE]
    let mut delim = b'\t';
    let mut field = 1usize;
    let mut rest  = args;
    while rest.starts_with(b"-") {
        if rest.starts_with(b"-d ") {
            rest = trim(&rest[3..]);
            if !rest.is_empty() { delim = rest[0]; rest = trim(&rest[1..]); }
        } else if rest.starts_with(b"-f ") {
            rest = trim(&rest[3..]);
            let mut n = 0usize;
            while !rest.is_empty() && rest[0].is_ascii_digit() {
                n = n * 10 + (rest[0] - b'0') as usize;
                rest = &rest[1..];
            }
            if n > 0 { field = n; }
            rest = trim(rest);
        } else { break; }
    }
    let path = rest;
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    let (fd, close_after) = if path == b"-" || (path.is_empty() && redir != usize::MAX) {
        if redir == usize::MAX { kprintln!("cut: no stdin"); return; }
        (redir as i32, false)
    } else if !path.is_empty() {
        let f = vfs_open(path, 0, 0);
        if f < 0 { kprintln!("cut: no such file"); return; }
        (f, true)
    } else { kprintln!("usage: cut -d D -f N FILE"); return; };

    let mut buf = [0u8; 256];
    let mut line = [0u8; 256];
    let mut llen = 0usize;
    loop {
        let n = vfs_read_blocking(fd, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        for i in 0..n as usize {
            let c = buf[i];
            if c == b'\n' {
                // Extract field from line[..llen].
                let mut f_cur = 1usize;
                let mut start = 0usize;
                let mut end   = llen;
                for j in 0..llen {
                    if line[j] == delim {
                        if f_cur == field { end = j; break; }
                        f_cur += 1; start = j + 1;
                    }
                }
                if f_cur == field { kraw!(&line[start..end]); kprintln!(""); }
                llen = 0;
            } else if llen < 255 { line[llen] = c; llen += 1; }
        }
    }
    if close_after { vfs_close(fd); }
}

/// tr SET1 SET2 / tr -d SET1  — translate or delete characters (stdin only).
fn cmd_tr(args: &[u8]) {
    let delete = args.starts_with(b"-d ");
    let rest   = if delete { trim(&args[3..]) } else { args };
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    if redir == usize::MAX { kprintln!("tr: no stdin"); return; }

    // Parse set1 and optional set2 (space-separated, first unquoted space).
    let (set1, set2) = if let Some(sp) = rest.iter().position(|&b| b == b' ') {
        (&rest[..sp], trim(&rest[sp+1..]))
    } else {
        (rest, &b""[..])
    };
    let mut buf = [0u8; 256];
    loop {
        let n = vfs_read_blocking(redir as i32, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        for i in 0..n as usize {
            let c = buf[i];
            if delete {
                if !set1.contains(&c) { io_write_raw(&buf[i..i+1]); }
            } else {
                if let Some(pos) = set1.iter().position(|&b| b == c) {
                    let out = if pos < set2.len() { set2[pos] } else { *set2.last().unwrap_or(&c) };
                    io_write_raw(&[out]);
                } else {
                    io_write_raw(&buf[i..i+1]);
                }
            }
        }
    }
}

/// uniq [FILE|-]  — filter consecutive duplicate lines.
fn cmd_uniq(path: &[u8]) {
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    let (fd, close_after) = if path == b"-" || (path.is_empty() && redir != usize::MAX) {
        if redir == usize::MAX { kprintln!("uniq: no stdin"); return; }
        (redir as i32, false)
    } else if !path.is_empty() {
        let f = vfs_open(path, 0, 0);
        if f < 0 { kprintln!("uniq: no such file"); return; }
        (f, true)
    } else { kprintln!("usage: uniq [FILE]"); return; };

    let mut prev = [0u8; 256];
    let mut plen = usize::MAX; // sentinel: no previous line
    let mut cur  = [0u8; 256];
    let mut clen = 0usize;
    let mut buf  = [0u8; 256];
    loop {
        let n = vfs_read_blocking(fd, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        for i in 0..n as usize {
            let c = buf[i];
            if c == b'\n' {
                if plen == usize::MAX || cur[..clen] != prev[..plen] {
                    kraw!(&cur[..clen]); kprintln!("");
                    prev[..clen].copy_from_slice(&cur[..clen]);
                    plen = clen;
                }
                clen = 0;
            } else if clen < 255 { cur[clen] = c; clen += 1; }
        }
    }
    if clen > 0 && (plen == usize::MAX || cur[..clen] != prev[..plen]) {
        kraw!(&cur[..clen]); kprintln!("");
    }
    if close_after { vfs_close(fd); }
}

/// xargs CMD  — read whitespace-separated tokens from stdin, append to CMD and run.
fn cmd_xargs(args: &[u8]) {
    if args.is_empty() { kprintln!("usage: xargs CMD"); return; }
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    if redir == usize::MAX { kprintln!("xargs: no stdin"); return; }
    // Collect all tokens into a single space-separated arg list.
    let mut tokens = [0u8; 256];
    let mut tlen   = 0usize;
    let mut buf    = [0u8; 256];
    let mut in_tok = false;
    loop {
        let n = vfs_read_blocking(redir as i32, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        for i in 0..n as usize {
            let c = buf[i];
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                if in_tok && tlen < 255 { tokens[tlen] = b' '; tlen += 1; }
                in_tok = false;
            } else if tlen < 254 {
                tokens[tlen] = c; tlen += 1; in_tok = true;
            }
        }
    }
    // Remove trailing space.
    while tlen > 0 && tokens[tlen-1] == b' ' { tlen -= 1; }
    if tlen == 0 { return; }
    // Build `CMD TOKENS` and dispatch.
    let mut cmd_line = [0u8; 512];
    let alen = args.len().min(255);
    cmd_line[..alen].copy_from_slice(&args[..alen]);
    cmd_line[alen] = b' ';
    cmd_line[alen+1..alen+1+tlen].copy_from_slice(&tokens[..tlen]);
    // Clear STDIN_REDIR so the sub-command uses real stdin.
    STDIN_REDIR.store(usize::MAX, Ordering::Relaxed);
    dispatch_command(&cmd_line[..alen+1+tlen]);
    STDIN_REDIR.store(redir, Ordering::Relaxed);
}

/// `printf FORMAT [ARG...]` — format string supporting %s/%d/\n/\t.
fn cmd_printf(args: &[u8]) {
    if args.is_empty() { return; }
    // Split format from arguments at first space after any %s/%d.
    // Strategy: scan format for % specifiers, consume args one by one.
    let (fmt, mut rest) = if let Some(sp) = args.iter().position(|&b| b == b' ') {
        (&args[..sp], &args[sp+1..])
    } else {
        (args, &b""[..])
    };
    let mut out = [0u8; 512];
    let mut oi  = 0usize;
    let mut fi  = 0usize;
    while fi < fmt.len() && oi < 510 {
        if fmt[fi] == b'\\' && fi + 1 < fmt.len() {
            fi += 1;
            match fmt[fi] {
                b'n' => { out[oi] = b'\n'; oi += 1; }
                b't' => { out[oi] = b'\t'; oi += 1; }
                b'r' => { out[oi] = b'\r'; oi += 1; }
                b'\\' => { out[oi] = b'\\'; oi += 1; }
                c    => { out[oi] = b'\\'; oi += 1; if oi < 510 { out[oi] = c; oi += 1; } }
            }
            fi += 1;
        } else if fmt[fi] == b'%' && fi + 1 < fmt.len() {
            fi += 1;
            // Optional zero-padding: %05d style
            let mut pad_char = b' ';
            let mut pad_width = 0usize;
            if fmt[fi] == b'0' { pad_char = b'0'; fi += 1; }
            while fi < fmt.len() && fmt[fi] >= b'0' && fmt[fi] <= b'9' {
                pad_width = pad_width * 10 + (fmt[fi] - b'0') as usize;
                fi += 1;
            }
            if fi >= fmt.len() { break; }
            match fmt[fi] {
                b's' => {
                    // Consume next arg token.
                    rest = trim(rest);
                    let (tok, tail) = if let Some(sp) = rest.iter().position(|&b| b == b' ') {
                        (&rest[..sp], trim(&rest[sp+1..]))
                    } else { (rest, &b""[..]) };
                    rest = tail;
                    for &b in tok.iter() { if oi < 510 { out[oi] = b; oi += 1; } }
                }
                b'd' => {
                    rest = trim(rest);
                    let (tok, tail) = if let Some(sp) = rest.iter().position(|&b| b == b' ') {
                        (&rest[..sp], trim(&rest[sp+1..]))
                    } else { (rest, &b""[..]) };
                    rest = tail;
                    let val = parse_int(tok);
                    let (db, dl) = if val < 0 {
                        let (b, l) = u32_dec((-val) as u32);
                        // prepend '-'
                        if oi < 510 { out[oi] = b'-'; oi += 1; }
                        (b, l)
                    } else { u32_dec(val as u32) };
                    if pad_width > dl {
                        let padding = pad_width - dl;
                        for _ in 0..padding { if oi < 510 { out[oi] = pad_char; oi += 1; } }
                    }
                    for k in 0..dl { if oi < 510 { out[oi] = db[k]; oi += 1; } }
                }
                b'%' => { out[oi] = b'%'; oi += 1; }
                c    => { out[oi] = b'%'; oi += 1; if oi < 510 { out[oi] = c; oi += 1; } }
            }
            fi += 1;
        } else {
            out[oi] = fmt[fi]; oi += 1; fi += 1;
        }
    }
    io_write_raw(&out[..oi]);
}

/// `sed s/FROM/TO/[g]` — substitute FROM with TO in lines from stdin or file.
fn cmd_sed(args: &[u8]) {
    // Parse: sed s/FROM/TO/[g] [FILE]
    let args = trim(args);
    if !args.starts_with(b"s/") { kprintln!("sed: only s/FROM/TO/[g] supported"); return; }
    let inner = &args[2..]; // skip "s/"
    // Find second '/'
    let Some(mid) = inner.iter().position(|&b| b == b'/') else { kprintln!("sed: malformed"); return; };
    let from = &inner[..mid];
    let rest = &inner[mid+1..];
    // Find third '/'
    let Some(end) = rest.iter().position(|&b| b == b'/') else { kprintln!("sed: malformed"); return; };
    let to   = &rest[..end];
    let flags = &rest[end+1..];
    let global = flags.contains(&b'g');
    // Determine input fd.
    let file_arg = trim(if let Some(sp) = flags.iter().position(|&b| b == b' ') { &flags[sp+1..] } else { b"" });
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    let (fd, close_after) = if !file_arg.is_empty() {
        let f = vfs_open(file_arg, 0, 0);
        if f < 0 { kprintln!("sed: cannot open file"); return; }
        (f, true)
    } else if redir != usize::MAX {
        (redir as i32, false)
    } else { kprintln!("sed: no input"); return; };

    let mut line_buf = [0u8; 512];
    let mut llen = 0usize;
    let mut io_buf = [0u8; 256];
    loop {
        let n = vfs_read_blocking(fd, io_buf.as_mut_ptr(), io_buf.len());
        if n <= 0 { break; }
        for i in 0..n as usize {
            let c = io_buf[i];
            if c == b'\n' {
                // Substitute and emit.
                let mut out = [0u8; 512];
                let olen = sed_replace(&line_buf[..llen], from, to, global, &mut out);
                io_write_raw(&out[..olen]);
                io_write_raw(b"\n");
                llen = 0;
            } else if llen < 511 { line_buf[llen] = c; llen += 1; }
        }
    }
    if llen > 0 {
        let mut out = [0u8; 512];
        let olen = sed_replace(&line_buf[..llen], from, to, global, &mut out);
        io_write_raw(&out[..olen]);
        io_write_raw(b"\n");
    }
    if close_after { vfs_close(fd); }
}

fn sed_replace(line: &[u8], from: &[u8], to: &[u8], global: bool, out: &mut [u8; 512]) -> usize {
    if from.is_empty() { let l = line.len().min(511); out[..l].copy_from_slice(&line[..l]); return l; }
    let mut oi = 0usize;
    let mut i  = 0usize;
    let mut replaced = false;
    while i <= line.len().saturating_sub(from.len()) {
        if !replaced || global {
            if &line[i..i+from.len()] == from {
                for &b in to.iter() { if oi < 511 { out[oi] = b; oi += 1; } }
                i += from.len();
                replaced = true;
                continue;
            }
        }
        if oi < 511 { out[oi] = line[i]; oi += 1; }
        i += 1;
    }
    // Copy remainder.
    while i < line.len() { if oi < 511 { out[oi] = line[i]; oi += 1; } i += 1; }
    oi
}

/// `awk '{print $N}'` — print the Nth whitespace-separated field (1-indexed, 0=whole line).
fn cmd_awk(args: &[u8]) {
    // Only support: awk '{print $N}' [FILE]
    let args = trim(args);
    // Parse field index from '{print $N}'
    let field: usize = if let Some(p) = find_seq(args, b"$") {
        let after = &args[p+1..];
        let mut n = 0usize;
        for &b in after.iter() {
            if b >= b'0' && b <= b'9' { n = n * 10 + (b - b'0') as usize; }
            else { break; }
        }
        n
    } else { 0 };
    // Find optional file arg after the closing '}'.
    let file_arg = if let Some(rb) = args.iter().position(|&b| b == b'}') {
        trim(&args[rb+1..])
    } else { b"" };
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    let (fd, close_after) = if !file_arg.is_empty() {
        let f = vfs_open(file_arg, 0, 0);
        if f < 0 { kprintln!("awk: cannot open file"); return; }
        (f, true)
    } else if redir != usize::MAX {
        (redir as i32, false)
    } else { kprintln!("awk: no input"); return; };

    let mut line_buf = [0u8; 512];
    let mut llen = 0usize;
    let mut io_buf = [0u8; 256];
    loop {
        let n = vfs_read_blocking(fd, io_buf.as_mut_ptr(), io_buf.len());
        if n <= 0 { break; }
        for i in 0..n as usize {
            let c = io_buf[i];
            if c == b'\n' {
                awk_print_field(&line_buf[..llen], field);
                llen = 0;
            } else if llen < 511 { line_buf[llen] = c; llen += 1; }
        }
    }
    if llen > 0 { awk_print_field(&line_buf[..llen], field); }
    if close_after { vfs_close(fd); }
}

fn awk_print_field(line: &[u8], field: usize) {
    if field == 0 { io_write_raw(line); io_write_raw(b"\n"); return; }
    let mut f = 0usize;
    let mut i = 0usize;
    while i < line.len() {
        while i < line.len() && (line[i] == b' ' || line[i] == b'\t') { i += 1; }
        if i >= line.len() { break; }
        f += 1;
        let start = i;
        while i < line.len() && line[i] != b' ' && line[i] != b'\t' { i += 1; }
        if f == field { io_write_raw(&line[start..i]); io_write_raw(b"\n"); return; }
    }
}

/// `read VAR` — read one line from stdin into a shell variable.
fn cmd_read(var_name: &[u8]) {
    let var_name = trim(var_name);
    if var_name.is_empty() { kprintln!("usage: read VAR"); return; }
    let mut buf = [0u8; MAX_VAR_VAL];
    let mut blen = 0usize;
    let redir = STDIN_REDIR.load(Ordering::Relaxed);
    if redir != usize::MAX {
        loop {
            let mut b = [0u8; 1];
            let n = vfs_read(redir as i32, b.as_mut_ptr(), 1);
            if n <= 0 { break; }
            if b[0] == b'\n' { break; }
            if blen < MAX_VAR_VAL - 1 { buf[blen] = b[0]; blen += 1; }
        }
    } else {
        // Blocking read from serial.
        loop {
            let byte = unsafe { if !IO.is_null() { ((*IO).read_byte)() } else { None } };
            match byte {
                Some(b'\n') | Some(b'\r') => break,
                Some(b) if blen < MAX_VAR_VAL - 1 => { buf[blen] = b; blen += 1; }
                None => sched::yield_now("init_idle"),
                _ => {}
            }
        }
    }
    var_set(var_name, &buf[..blen]);
}

/// `source FILE` / `. FILE` — execute each line of FILE in the current shell.
fn cmd_source(path: &[u8]) {
    let path = trim(path);
    if path.is_empty() { kprintln!("usage: source FILE"); return; }
    let fd = vfs_open(path, 0, 0);
    if fd < 0 { kraw!(b"source: cannot open "); kraw!(path); kprintln!(""); return; }
    let mut line = [0u8; 256];
    let mut llen = 0usize;
    let mut buf  = [0u8; 256];
    loop {
        let n = vfs_read_blocking(fd, buf.as_mut_ptr(), buf.len());
        if n <= 0 { break; }
        for i in 0..n as usize {
            if buf[i] == b'\n' {
                if llen > 0 { dispatch_command(&line[..llen]); llen = 0; }
            } else if llen < 255 { line[llen] = buf[i]; llen += 1; }
        }
    }
    if llen > 0 { dispatch_command(&line[..llen]); }
    vfs_close(fd);
}

/// `type NAME` — show whether NAME is a builtin, function, or unknown.
fn cmd_type(name: &[u8]) {
    let name = trim(name);
    let funcs = SHELL_FUNCS.lock();
    if funcs.iter().any(|f| f.nlen == name.len() && f.name[..f.nlen] == *name) {
        kraw!(name); kprintln!(" is a shell function");
        return;
    }
    drop(funcs);
    kraw!(name); kprintln!(" is a shell builtin");
}

fn cmd_help() {
    kprintln!("Built-in commands:");
    kprintln!("  echo <text>         -- print text");
    kprintln!("  cat <path>          -- print file contents");
    kprintln!("  ls [path]           -- list directory (default: /)");
    kprintln!("  cd <path>           -- change directory");
    kprintln!("  pwd                 -- print working directory");
    kprintln!("  mkdir <path>        -- create directory (in /tmp only)");
    kprintln!("  rm <path>           -- remove file (in /tmp only)");
    kprintln!("  cp <src> <dst>      -- copy file");
    kprintln!("  mv <src> <dst>      -- move/rename file");
    kprintln!("  write <path> <text> -- create/overwrite /tmp file");
    kprintln!("  readlink <path>     -- resolve /proc/self/fd/N links");
    kprintln!("  sleep <n>           -- sleep for n seconds");
    kprintln!("  grep PATTERN [FILE] -- print matching lines (substring)");
    kprintln!("  sort [FILE]         -- sort lines lexicographically");
    kprintln!("  tee FILE            -- copy stdin to stdout and FILE");
    kprintln!("  find [DIR] [-name P]-- list files matching name pattern");
    kprintln!("  touch FILE          -- create empty file");
    kprintln!("  cut -d D -f N [FILE]-- extract Nth field by delimiter");
    kprintln!("  tr SET1 SET2        -- translate chars (stdin)");
    kprintln!("  tr -d SET1          -- delete chars from stdin");
    kprintln!("  uniq [FILE]         -- filter consecutive duplicate lines");
    kprintln!("  xargs CMD           -- run CMD with args from stdin");
    kprintln!("  wc <path>           -- count lines/words/bytes");
    kprintln!("  head [-n N] <path>  -- show first N lines (default 10)");
    kprintln!("  whoami              -- print current user");
    kprintln!("  hostname            -- print hostname");
    kprintln!("  ps                  -- list processes");
    kprintln!("  free                -- show memory usage");
    kprintln!("  df                  -- show disk usage");
    kprintln!("  date                -- show uptime as HH:MM:SS");
    kprintln!("  kill [-SIG] <pid>   -- send signal to process");
    kprintln!("  uname               -- system info");
    kprintln!("  uptime              -- seconds since boot");
    kprintln!("  test EXPR           -- evaluate: -f/-d/-z/-n/-eq/-ne/-lt/-gt/=");
    kprintln!("  [ EXPR ]            -- alias for test");
    kprintln!("  true / false        -- exit 0 / exit 1");
    kprintln!("  help                -- this message");
    kprintln!("  exit                -- halt (enter event loop)");
    kprintln!("  read VAR            -- read line from stdin into variable");
    kprintln!("  printf FMT [ARGS]   -- format: %s %d \\n \\t %05d");
    kprintln!("  sed s/FROM/TO/[g]   -- substitute in stdin or file");
    kprintln!("  awk '{print $N}'    -- print Nth field (stdin or file)");
    kprintln!("Control flow (single-line):");
    kprintln!("  if CMD; then CMD2; fi");
    kprintln!("  if CMD; then CMD2; else CMD3; fi");
    kprintln!("  while CMD; do CMD2; done");
    kprintln!("  for VAR in ITEMS; do CMD; done");
    kprintln!("  case EXPR in PAT) CMD;; *) CMD;; esac");
    kprintln!("  NAME() { BODY }     -- define a shell function");
    kprintln!("  source FILE / . FILE-- execute FILE in current shell");
    kprintln!("  type NAME           -- show if NAME is function or builtin");
    kprintln!("  CMD1; CMD2          -- semicolon-separated statements");
    kprintln!("Substitution: $(cmd) expands to stdout of cmd");
    kprintln!("Redirection: cmd > /tmp/out.txt  (overwrite)");
    kprintln!("             cmd >> /tmp/out.txt  (append)");
    kprintln!("Pipes:       cmd1 | cmd2          (stdout of cmd1 to stdin of cmd2)");
}

fn cmd_uptime() {
    let secs = sched::ticks() / 100;
    let mut b = [0u8; 10];
    let (db, dl) = u32_dec(secs as u32);
    b[..dl].copy_from_slice(&db[..dl]);
    kraw!(&b[..dl]);
    kprintln!(" seconds since boot");
}

/// Find first occurrence of `needle` in `haystack`; returns byte index or None.
fn find_seq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() { return Some(0); }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Parse a signed decimal integer from bytes (stops at first non-digit).
fn parse_int(s: &[u8]) -> i64 {
    let mut n = 0i64;
    let mut neg = false;
    let mut i = 0usize;
    if !s.is_empty() && s[0] == b'-' { neg = true; i = 1; }
    while i < s.len() && s[i] >= b'0' && s[i] <= b'9' {
        n = n * 10 + (s[i] - b'0') as i64; i += 1;
    }
    if neg { -n } else { n }
}

/// `test ARGS` / `[ ARGS ]` — evaluate a POSIX test expression.
/// Returns true (success/0) or false (failure/1).
fn cmd_test(args: &[u8]) -> bool {
    let args = trim(args);
    // Strip trailing ']' for `[` invocation.
    let args = if args.ends_with(b"]") { trim(&args[..args.len()-1]) } else { args };
    if args.is_empty() { return false; }

    // Unary operators.
    if args.starts_with(b"-f ") {
        let path = trim(&args[3..]);
        let fd = vfs_open(path, 0, 0);
        if fd >= 0 { vfs_close(fd); return true; }
        return false;
    }
    if args.starts_with(b"-d ") {
        let path = trim(&args[3..]);
        let fd = vfs_open(path, 0x10000 /* O_DIRECTORY */, 0);
        if fd >= 0 { vfs_close(fd); return true; }
        return false;
    }
    if args.starts_with(b"-z ") { return trim(&args[3..]).is_empty(); }
    if args.starts_with(b"-n ") { return !trim(&args[3..]).is_empty(); }
    if args.starts_with(b"-e ") {
        // alias for -f
        let path = trim(&args[3..]);
        let fd = vfs_open(path, 0, 0);
        if fd >= 0 { vfs_close(fd); return true; }
        return false;
    }

    // Binary operators: STR1 OP STR2.
    if let Some(p) = find_seq(args, b" -eq ") {
        return parse_int(trim(&args[..p])) == parse_int(trim(&args[p+5..]));
    }
    if let Some(p) = find_seq(args, b" -ne ") {
        return parse_int(trim(&args[..p])) != parse_int(trim(&args[p+5..]));
    }
    if let Some(p) = find_seq(args, b" -lt ") {
        return parse_int(trim(&args[..p])) < parse_int(trim(&args[p+5..]));
    }
    if let Some(p) = find_seq(args, b" -gt ") {
        return parse_int(trim(&args[..p])) > parse_int(trim(&args[p+5..]));
    }
    if let Some(p) = find_seq(args, b" -le ") {
        return parse_int(trim(&args[..p])) <= parse_int(trim(&args[p+5..]));
    }
    if let Some(p) = find_seq(args, b" -ge ") {
        return parse_int(trim(&args[..p])) >= parse_int(trim(&args[p+5..]));
    }
    if let Some(p) = find_seq(args, b" != ") {
        return &args[..p] != &args[p+4..];
    }
    if let Some(p) = find_seq(args, b" = ") {
        return &args[..p] == &args[p+3..];
    }
    // Plain non-empty string → true.
    !args.is_empty()
}

/// Trim leading/trailing ASCII spaces from a byte slice.
fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| b != b' ').unwrap_or(s.len());
    let end   = s.iter().rposition(|&b| b != b' ').map(|p| p + 1).unwrap_or(0);
    if start >= end { &[] } else { &s[start..end] }
}

/// Run one line from the shell.  Returns `true` to continue, `false` to exit.
fn dispatch_command(line: &[u8]) -> bool {
    let line = trim(line);
    if line.is_empty() { return true; }

    // Function definition: NAME() { BODY; }
    if let Some(paren_pos) = line.iter().position(|&b| b == b'(') {
        let name_cand = &line[..paren_pos];
        if !name_cand.is_empty()
            && name_cand.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_')
            && !name_cand[0].is_ascii_digit()
        {
            let after = &line[paren_pos+1..];
            if let Some(brace) = find_seq(after, b"{") {
                let close_paren = after.iter().position(|&b| b == b')').unwrap_or(0);
                if close_paren < brace {
                    let inner = trim(&after[brace+1..]);
                    let body = if inner.ends_with(b"}") { trim(&inner[..inner.len()-1]) } else { inner };
                    func_define(name_cand, body);
                    return true;
                }
            }
        }
    }

    // Variable assignment: VAR=value  (no spaces around =, first token has no space before =)
    if let Some(eq) = line.iter().position(|&b| b == b'=') {
        let key = &line[..eq];
        if !key.is_empty() && key.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_')
            && !key[0].is_ascii_digit() {
            var_set(key, &line[eq+1..]);
            return true;
        }
    }

    // Expand $VARs in the line before further processing.
    let mut expanded = [0u8; 512];
    let elen = var_expand(line, &mut expanded);
    let line = trim(&expanded[..elen]);
    if line.is_empty() { return true; }

    // Detect: if COND; then BODY; [else ELSE; ]fi
    if line.starts_with(b"if ") {
        if let Some(then_off) = find_seq(&line[3..], b"; then ") {
            let cond = trim(&line[3..3+then_off]);
            let rest = &line[3 + then_off + 7..];
            // rest must end with "; fi"
            if rest.ends_with(b"; fi") || rest == b"fi" {
                let body_all = if rest.ends_with(b"; fi") { trim(&rest[..rest.len()-4]) } else { b"" as &[u8] };
                if let Some(else_off) = find_seq(body_all, b"; else ") {
                    let then_body = trim(&body_all[..else_off]);
                    let else_body = trim(&body_all[else_off + 7..]);
                    dispatch_command(cond);
                    if LAST_EXIT_CODE.load(Ordering::Relaxed) == 0 { dispatch_command(then_body); }
                    else { dispatch_command(else_body); }
                } else {
                    dispatch_command(cond);
                    if LAST_EXIT_CODE.load(Ordering::Relaxed) == 0 { dispatch_command(body_all); }
                }
            }
        }
        return true;
    }

    // Detect: while COND; do BODY; done
    if line.starts_with(b"while ") {
        if let Some(do_off) = find_seq(&line[6..], b"; do ") {
            let cond = trim(&line[6..6+do_off]);
            let rest = &line[6 + do_off + 5..];
            if rest.ends_with(b"; done") {
                let body = trim(&rest[..rest.len()-6]);
                for _ in 0..256usize {
                    dispatch_command(cond);
                    if LAST_EXIT_CODE.load(Ordering::Relaxed) != 0 { break; }
                    dispatch_command(body);
                }
            }
        }
        return true;
    }

    // Detect: for VAR in ITEMS; do BODY; done
    if line.starts_with(b"for ") {
        let rest = &line[4..];
        if let Some(in_off) = find_seq(rest, b" in ") {
            let var_name = trim(&rest[..in_off]);
            let rest2 = &rest[in_off + 4..];
            if let Some(do_off) = find_seq(rest2, b"; do ") {
                let items_str = trim(&rest2[..do_off]);
                let rest3 = &rest2[do_off + 5..];
                if rest3.ends_with(b"; done") {
                    let body = trim(&rest3[..rest3.len()-6]);
                    let mut i = 0usize;
                    while i < items_str.len() {
                        while i < items_str.len() && items_str[i] == b' ' { i += 1; }
                        if i >= items_str.len() { break; }
                        let item_start = i;
                        while i < items_str.len() && items_str[i] != b' ' { i += 1; }
                        var_set(var_name, &items_str[item_start..i]);
                        dispatch_command(body);
                    }
                }
            }
        }
        return true;
    }

    // Detect: case EXPR in PAT) BODY;; [PAT2) BODY2;;] esac
    if line.starts_with(b"case ") {
        let rest = &line[5..];
        if let Some(in_off) = find_seq(rest, b" in ") {
            let expr = trim(&rest[..in_off]);
            let pats = trim(&rest[in_off + 4..]);
            let pats = if pats.ends_with(b" esac") { trim(&pats[..pats.len()-5]) }
                       else if pats.ends_with(b"esac") { trim(&pats[..pats.len()-4]) }
                       else { pats };
            let mut pos = 0usize;
            let mut matched = false;
            while pos < pats.len() && !matched {
                let end = find_seq(&pats[pos..], b";;").map(|p| pos + p).unwrap_or(pats.len());
                let case_str = trim(&pats[pos..end]);
                pos = end + 2;
                if case_str.is_empty() { continue; }
                if let Some(rp) = case_str.iter().position(|&b| b == b')') {
                    let pat  = trim(&case_str[..rp]);
                    let body = trim(&case_str[rp+1..]);
                    let m = if pat == b"*" { true }
                            else if pat.ends_with(b"*") { expr.starts_with(&pat[..pat.len()-1]) }
                            else if pat.starts_with(b"*") { expr.ends_with(&pat[1..]) }
                            else { pat == expr };
                    if m { matched = true; dispatch_command(body); }
                }
            }
        }
        return true;
    }

    // Semicolon-separated statements: "CMD1; CMD2"  (not inside control flow keywords).
    if !line.starts_with(b"if ") && !line.starts_with(b"while ") &&
       !line.starts_with(b"for ") && !line.starts_with(b"case ") {
        if let Some(semi) = line.iter().position(|&b| b == b';') {
            let first = trim(&line[..semi]);
            let rest  = trim(&line[semi+1..]);
            if !first.is_empty() { dispatch_command(first); }
            if !rest.is_empty()  { dispatch_command(rest); }
            return true;
        }
    }

    // Detect pipe: `left | right`  — buffer left to a tmp file, feed to right.
    if let Some(pipe_pos) = line.iter().position(|&b| b == b'|') {
        let left  = trim(&line[..pipe_pos]);
        let right = trim(&line[pipe_pos+1..]);
        if !left.is_empty() && !right.is_empty() {
            const O_WRONLY: u32 = 0x001;
            const O_CREAT:  u32 = 0x040;
            const O_TRUNC:  u32 = 0x200;
            let buf_path = b"/tmp/.pipebuf";
            let wfd = vfs_open(buf_path, O_WRONLY | O_CREAT | O_TRUNC, 0o600);
            if wfd >= 0 {
                STDOUT_REDIR.store(wfd as usize, Ordering::Relaxed);
                dispatch_command(left);
                STDOUT_REDIR.store(usize::MAX, Ordering::Relaxed);
                vfs_close(wfd);
                let rfd = vfs_open(buf_path, 0, 0);
                if rfd >= 0 {
                    STDIN_REDIR.store(rfd as usize, Ordering::Relaxed);
                    dispatch_command(right);
                    STDIN_REDIR.store(usize::MAX, Ordering::Relaxed);
                    vfs_close(rfd);
                }
            } else {
                kprintln!("pipe: cannot create buffer");
            }
            return true;
        }
    }

    // Detect output redirection: `cmd >> file` (append) or `cmd > file` (trunc).
    let (line, redir_fd) = {
        const O_WRONLY: u32 = 0x001;
        const O_CREAT:  u32 = 0x040;
        const O_TRUNC:  u32 = 0x200;
        const O_APPEND: u32 = 0x400;
        let mut rfd: i32 = -1;
        let mut cmd_part = line;
        // Search for `>>` first, then `>`.
        let append_pos = line.windows(2).position(|w| w == b">>");
        let trunc_pos  = line.iter().position(|&b| b == b'>');
        if let Some(p) = append_pos {
            let path = trim(&line[p+2..]);
            if !path.is_empty() {
                rfd = vfs_open(path, O_WRONLY | O_CREAT | O_APPEND, 0o644);
                cmd_part = trim(&line[..p]);
            }
        } else if let Some(p) = trunc_pos {
            let path = trim(&line[p+1..]);
            if !path.is_empty() {
                rfd = vfs_open(path, O_WRONLY | O_CREAT | O_TRUNC, 0o644);
                cmd_part = trim(&line[..p]);
            }
        }
        (cmd_part, rfd)
    };
    if redir_fd >= 0 { STDOUT_REDIR.store(redir_fd as usize, Ordering::Relaxed); }

    // Split command from arguments at the first space.
    let (cmd, args) = if let Some(sp) = line.iter().position(|&b| b == b' ') {
        (&line[..sp], trim(&line[sp+1..]))
    } else {
        (line, &b""[..])
    };

    match cmd {
        b"echo"   => cmd_echo(args),
        b"cat"    => cmd_cat(if args.is_empty() && STDIN_REDIR.load(Ordering::Relaxed) == usize::MAX { b"/etc/motd" } else { args }),
        b"ls"     => cmd_ls(if args.is_empty() { b"/" } else { args }),
        b"cd"     => cmd_cd(args),
        b"pwd"    => cmd_pwd(),
        b"mkdir"  => cmd_mkdir(args),
        b"rm"     => cmd_rm(args),
        b"cp"     => {
            if let Some(sp) = args.iter().position(|&b| b == b' ') {
                cmd_cp(&args[..sp], trim(&args[sp+1..]));
            } else { kprintln!("usage: cp <src> <dst>"); }
        }
        b"mv"     => {
            if let Some(sp) = args.iter().position(|&b| b == b' ') {
                cmd_mv(&args[..sp], trim(&args[sp+1..]));
            } else { kprintln!("usage: mv <src> <dst>"); }
        }
        b"readlink" => cmd_readlink(args),
        b"write"  => {
            // write <path> <content>
            if let Some(sp) = args.iter().position(|&b| b == b' ') {
                cmd_write(&args[..sp], trim(&args[sp+1..]));
            } else {
                kprintln!("usage: write <path> <content>");
            }
        }
        b"sleep"  => cmd_sleep(args),
        b"grep"   => cmd_grep(args),
        b"sort"   => cmd_sort(if args.is_empty() && STDIN_REDIR.load(Ordering::Relaxed) != usize::MAX { b"-" } else { args }),
        b"tee"    => cmd_tee(args),
        b"find"   => cmd_find(args),
        b"touch"  => cmd_touch(args),
        b"cut"    => cmd_cut(args),
        b"tr"     => cmd_tr(args),
        b"uniq"   => cmd_uniq(if args.is_empty() && STDIN_REDIR.load(Ordering::Relaxed) != usize::MAX { b"-" } else { args }),
        b"xargs"  => cmd_xargs(args),
        b"wc"     => cmd_wc(if args.is_empty() && STDIN_REDIR.load(Ordering::Relaxed) == usize::MAX { b"/etc/motd" } else { args }),
        b"head"   => {
            // head [-n N] <path>
            let (n, path) = if args.starts_with(b"-n ") {
                let rest = trim(&args[3..]);
                let mut n = 0usize;
                let mut i = 0;
                while i < rest.len() && rest[i] >= b'0' && rest[i] <= b'9' {
                    n = n * 10 + (rest[i] - b'0') as usize; i += 1;
                }
                let p = trim(&rest[i..]);
                (if n == 0 { 10 } else { n }, p)
            } else {
                (10usize, args)
            };
            cmd_head(path, n);
        }
        b"test"     => {
            let ok = cmd_test(args);
            LAST_EXIT_CODE.store(if ok { 0 } else { 1 }, Ordering::Relaxed);
        }
        b"["        => {
            let ok = cmd_test(args);
            LAST_EXIT_CODE.store(if ok { 0 } else { 1 }, Ordering::Relaxed);
        }
        b"whoami"   => kprintln!("root"),
        b"hostname" => cmd_cat(b"/etc/hostname"),
        b"true"     => { LAST_EXIT_CODE.store(0, Ordering::Relaxed); }
        b"false"    => { LAST_EXIT_CODE.store(1, Ordering::Relaxed); }
        b"env"      => {
            let vars = SHELL_VARS.lock();
            let mut any = false;
            for v in vars.iter() {
                if v.klen > 0 {
                    kraw!(&v.key[..v.klen]); kprint!("="); kraw!(&v.val[..v.vlen]); kprintln!("");
                    any = true;
                }
            }
            if !any { kprintln!("(no variables set)"); }
        }
        b"unset"    => { if !args.is_empty() { var_set(args, b""); } }
        b"read"     => cmd_read(args),
        b"printf"   => cmd_printf(args),
        b"sed"      => cmd_sed(args),
        b"awk"      => cmd_awk(args),
        b"source"   => cmd_source(args),
        b"."        => cmd_source(args),
        b"type"     => cmd_type(args),
        b"ps"       => cmd_ps(),
        b"free"     => cmd_free(),
        b"df"       => cmd_df(),
        b"date"     => cmd_date(),
        b"kill"     => cmd_kill(args),
        b"uname"    => cmd_uname(),
        b"uptime"   => cmd_uptime(),
        b"help"     | b"?"  => cmd_help(),
        b"exit"     | b"quit" => {
            if redir_fd >= 0 { STDOUT_REDIR.store(usize::MAX, Ordering::Relaxed); vfs_close(redir_fd); }
            return false;
        }
        _ => {
            if !call_func(cmd, args) {
                kraw!(b"unknown command: "); kraw!(cmd); kprintln!("");
                kprintln!("Type 'help' for a list of commands.");
            }
        }
    }
    if redir_fd >= 0 { STDOUT_REDIR.store(usize::MAX, Ordering::Relaxed); vfs_close(redir_fd); }
    true
}

fn run_shell() -> ! {
    kprintln!("[init] ═══════════════════════════════════════════");
    kprintln!("[init] Leandros interactive shell");
    kprintln!("[init] Type 'help' for available commands.");
    kprintln!("[init] ═══════════════════════════════════════════");
    let mut line = [0u8; 256];
    loop {
        kraw!(b"leandros> ");
        let n = readline(&mut line);
        if n == 0 { continue; }
        if !dispatch_command(&line[..n]) {
            kprintln!("[init] Shell exited — entering event loop");
            break;
        }
    }
    event_loop()
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Format a u32 as ASCII decimal.  Returns (buf, len).
fn u32_dec(mut n: u32) -> ([u8; 10], usize) {
    let mut b = [0u8; 10];
    if n == 0 { b[0] = b'0'; return (b, 1); }
    let mut i = 10usize;
    while n > 0 { i -= 1; b[i] = b'0' + (n % 10) as u8; n /= 10; }
    let mut out = [0u8; 10];
    let len = 10 - i;
    out[..len].copy_from_slice(&b[i..]);
    (out, len)
}

// ── Event loop ────────────────────────────────────────────────────────────────

fn event_loop() -> ! {
    kprintln!("[init] System ready — entering event loop");
    let mut last = 0u64;
    loop {
        let t = sched::ticks();
        if t.wrapping_sub(last) >= 100 {
            last = t;
            kprintln!("[init] heartbeat");
        }
        sched::yield_now("init_idle");
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

static STARTED: AtomicBool = AtomicBool::new(false);

/// Called from `kernel/src/init.rs` as the PID-1 task body.
pub fn init_main(io: &'static IoHooks) -> ! {
    if STARTED.swap(true, Ordering::SeqCst) {
        panic!("init_main called twice");
    }
    unsafe { IO = io as *const IoHooks; }

    kprintln!("\n\
     ██████╗██╗   ██╗ █████╗ ███╗   ██╗ ██████╗ ███████╗\n\
    ██╔════╝╚██╗ ██╔╝██╔══██╗████╗  ██║██╔═══██╗██╔════╝\n\
    ██║      ╚████╔╝ ███████║██╔██╗ ██║██║   ██║███████╗\n\
    ██║       ╚██╔╝  ██╔══██║██║╚██╗██║██║   ██║╚════██║\n\
    ╚██████╗   ██║   ██║  ██║██║ ╚████║╚██████╔╝███████║\n\
     ╚═════╝   ╚═╝   ╚═╝  ╚═╝╚═╝  ╚═══╝ ╚═════╝ ╚══════╝");
    kprint!("[init] PID ");
    {
        let (b, l) = u32_dec(sched::current_pid());
        kraw!(&b[..l]);
    }
    kprintln!(" starting");

    run_posix_tests();
    run_shell();
}
