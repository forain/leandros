//! mqtest -- POSIX message queues (lane posixmq): mq_open/mq_unlink/
//! mq_timedsend/mq_timedreceive/mq_notify/mq_getsetattr on raw syscalls,
//! priorities, O_NONBLOCK, timeouts, poll readiness, IPC-namespace isolation
//! and the mqueue filesystem (mount, readdir, QSIZE read, unlink).
//! Exits with the number of failed checks.

#![no_std]
#![no_main]

#[path = "../../muslrt/rt.rs"]
mod rt;

extern "C" {
    pub fn fork() -> i32;
    pub fn waitpid(pid: i32, stat: *mut i32, options: i32) -> i32;
    pub fn _exit(status: i32) -> !;
    pub fn exit(status: i32) -> !;
}

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const READ: i64 = 0; pub const WRITE: i64 = 1; pub const CLOSE: i64 = 3;
    pub const PPOLL: i64 = 271; pub const OPENAT: i64 = 257; pub const GETDENTS64: i64 = 217;
    pub const MKDIRAT: i64 = 258; pub const UNLINKAT: i64 = 263; pub const MOUNT: i64 = 165;
    pub const UNSHARE: i64 = 272; pub const CLOCK_GETTIME: i64 = 228;
    pub const SIGPROCMASK: i64 = 14; pub const SIGTIMEDWAIT: i64 = 128;
    pub const MQ_OPEN: i64 = 240; pub const MQ_UNLINK: i64 = 241; pub const MQ_TIMEDSEND: i64 = 242;
    pub const MQ_TIMEDRECEIVE: i64 = 243; pub const MQ_NOTIFY: i64 = 244; pub const MQ_GETSETATTR: i64 = 245;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const READ: i64 = 63; pub const WRITE: i64 = 64; pub const CLOSE: i64 = 57;
    pub const PPOLL: i64 = 73; pub const OPENAT: i64 = 56; pub const GETDENTS64: i64 = 61;
    pub const MKDIRAT: i64 = 34; pub const UNLINKAT: i64 = 35; pub const MOUNT: i64 = 40;
    pub const UNSHARE: i64 = 97; pub const CLOCK_GETTIME: i64 = 113;
    pub const SIGPROCMASK: i64 = 135; pub const SIGTIMEDWAIT: i64 = 137;
    pub const MQ_OPEN: i64 = 180; pub const MQ_UNLINK: i64 = 181; pub const MQ_TIMEDSEND: i64 = 182;
    pub const MQ_TIMEDRECEIVE: i64 = 183; pub const MQ_NOTIFY: i64 = 184; pub const MQ_GETSETATTR: i64 = 185;
}

const O_RDONLY: i64 = 0; const O_RDWR: i64 = 2; const O_CREAT: i64 = 0o100; const O_EXCL: i64 = 0o200;
const O_NONBLOCK: i64 = 0o4000; const O_DIRECTORY: i64 = 0o200000;
const AT_FDCWD: i64 = -100;
const EAGAIN: i64 = -11; const ENOENT: i64 = -2; const ETIMEDOUT: i64 = -110;
const EMSGSIZE: i64 = -90; const EEXIST: i64 = -17; const EBUSY: i64 = -16;

static mut FAILS: i32 = 0;

unsafe fn sc(n: i64, a: [i64; 6]) -> i64 { rt::raw_syscall(n, a) }
unsafe fn puts(s: &[u8]) { sc(nr::WRITE, [1, s.as_ptr() as i64, s.len() as i64, 0, 0, 0]); }
unsafe fn putn(mut v: i64) {
    if v < 0 { puts(b"-"); v = -v; }
    let mut b = [0u8; 20]; let mut i = 20;
    if v == 0 { puts(b"0"); return; }
    while v > 0 { i -= 1; b[i] = b'0' + (v % 10) as u8; v /= 10; }
    puts(&b[i..]);
}
unsafe fn check(what: &[u8], got: i64, want: i64) -> bool {
    let ok = got == want;
    puts(if ok { b"[PASS] " } else { b"[FAIL] " });
    puts(what);
    if !ok { puts(b" got="); putn(got); puts(b" want="); putn(want); FAILS += 1; }
    puts(b"\n");
    ok
}
unsafe fn truth(what: &[u8], cond: bool) { check(what, cond as i64, 1); }

fn cs(s: &[u8], buf: &mut [u8; 96]) -> i64 { buf[..s.len()].copy_from_slice(s); buf[s.len()] = 0; buf.as_ptr() as i64 }

unsafe fn mq_open(name: &[u8], oflag: i64, attr: Option<(i64, i64)>) -> i64 {
    let mut nb = [0u8; 96];
    let p = cs(name, &mut nb);
    let a = [0i64, attr.map_or(0, |a| a.0), attr.map_or(0, |a| a.1), 0, 0, 0, 0, 0];
    sc(nr::MQ_OPEN, [p, oflag, 0o644, if attr.is_some() { a.as_ptr() as i64 } else { 0 }, 0, 0])
}
unsafe fn mq_unlink(name: &[u8]) -> i64 { let mut nb = [0u8; 96]; let p = cs(name, &mut nb); sc(nr::MQ_UNLINK, [p, 0, 0, 0, 0, 0]) }
unsafe fn send(fd: i64, m: &[u8], prio: i64, ts: i64) -> i64 { sc(nr::MQ_TIMEDSEND, [fd, m.as_ptr() as i64, m.len() as i64, prio, ts, 0]) }
unsafe fn recv(fd: i64, buf: &mut [u8], prio: &mut u32, ts: i64) -> i64 {
    sc(nr::MQ_TIMEDRECEIVE, [fd, buf.as_mut_ptr() as i64, buf.len() as i64, prio as *mut u32 as i64, ts, 0])
}
unsafe fn now_ns() -> i64 { let mut t = [0i64; 2]; sc(nr::CLOCK_GETTIME, [0, t.as_mut_ptr() as i64, 0, 0, 0, 0]); t[0] * 1_000_000_000 + t[1] }
unsafe fn close(fd: i64) { sc(nr::CLOSE, [fd, 0, 0, 0, 0, 0]); }

unsafe fn t_priority_order() {
    mq_unlink(b"/mqt_prio");
    let fd = mq_open(b"/mqt_prio", O_CREAT | O_RDWR, Some((4, 64)));
    truth(b"mq_open O_CREAT returns an mqd", fd >= 0);
    check(b"O_EXCL on existing queue is EEXIST", mq_open(b"/mqt_prio", O_CREAT | O_EXCL | O_RDWR, None), EEXIST);
    check(b"send prio 1", send(fd, b"low", 1, 0), 0);
    check(b"send prio 5", send(fd, b"high", 5, 0), 0);
    check(b"send prio 3", send(fd, b"mid", 3, 0), 0);
    check(b"send prio 5 again (FIFO within prio)", send(fd, b"high2", 5, 0), 0);
    let mut attr = [0i64; 8];
    check(b"mq_getattr", sc(nr::MQ_GETSETATTR, [fd, 0, attr.as_mut_ptr() as i64, 0, 0, 0]), 0);
    check(b"attr maxmsg", attr[1], 4); check(b"attr msgsize", attr[2], 64); check(b"attr curmsgs", attr[3], 4);
    let mut buf = [0u8; 64]; let mut pr = 99u32;
    check(b"recv #1 len", recv(fd, &mut buf, &mut pr, 0), 4);
    check(b"recv #1 prio", pr as i64, 5); truth(b"recv #1 is 'high'", &buf[..4] == b"high");
    check(b"recv #2 len", recv(fd, &mut buf, &mut pr, 0), 5);
    check(b"recv #2 prio", pr as i64, 5); truth(b"recv #2 is 'high2'", &buf[..5] == b"high2");
    check(b"recv #3 prio-3", recv(fd, &mut buf, &mut pr, 0), 3); check(b"prio 3", pr as i64, 3);
    check(b"recv #4 prio-1", recv(fd, &mut buf, &mut pr, 0), 3); check(b"prio 1", pr as i64, 1);
    check(b"send 65 bytes into msgsize 64 is EMSGSIZE", send(fd, &[0u8; 65], 0, 0), EMSGSIZE);
    let mut small = [0u8; 8];
    check(b"recv into too-small buffer is EMSGSIZE", recv(fd, &mut small, &mut pr, 0), EMSGSIZE);
    close(fd);
    check(b"mq_unlink", mq_unlink(b"/mqt_prio"), 0);
    check(b"mq_unlink again is ENOENT", mq_unlink(b"/mqt_prio"), ENOENT);
    check(b"open after unlink is ENOENT", mq_open(b"/mqt_prio", O_RDWR, None), ENOENT);
}

unsafe fn t_nonblock_and_timeout() {
    mq_unlink(b"/mqt_nb");
    let fd = mq_open(b"/mqt_nb", O_CREAT | O_RDWR | O_NONBLOCK, Some((2, 16)));
    let mut buf = [0u8; 16]; let mut pr = 0u32;
    check(b"O_NONBLOCK receive on empty is EAGAIN", recv(fd, &mut buf, &mut pr, 0), EAGAIN);
    check(b"send 1", send(fd, b"a", 0, 0), 0); check(b"send 2", send(fd, b"b", 0, 0), 0);
    check(b"O_NONBLOCK send on full is EAGAIN", send(fd, b"c", 0, 0), EAGAIN);
    // mq_setattr clears O_NONBLOCK; a timed receive then blocks until the timeout.
    let mut a = [0i64; 8];
    sc(nr::MQ_GETSETATTR, [fd, a.as_ptr() as i64, 0, 0, 0, 0]);
    check(b"drain 1", recv(fd, &mut buf, &mut pr, 0), 1); check(b"drain 2", recv(fd, &mut buf, &mut pr, 0), 1);
    let t0 = now_ns();
    let ts = [(t0 + 150_000_000) / 1_000_000_000, (t0 + 150_000_000) % 1_000_000_000];
    check(b"timed receive on empty is ETIMEDOUT", recv(fd, &mut buf, &mut pr, ts.as_ptr() as i64), ETIMEDOUT);
    let dt = now_ns() - t0;
    truth(b"timed receive waited ~150ms", dt >= 140_000_000 && dt < 2_000_000_000);
    send(fd, b"a", 0, 0); send(fd, b"b", 0, 0);
    let t1 = now_ns();
    let ts2 = [(t1 + 120_000_000) / 1_000_000_000, (t1 + 120_000_000) % 1_000_000_000];
    check(b"timed send on full is ETIMEDOUT", send(fd, b"c", 0, ts2.as_ptr() as i64), ETIMEDOUT);
    close(fd); mq_unlink(b"/mqt_nb");
}

unsafe fn t_blocking_wakeup() {
    mq_unlink(b"/mqt_blk");
    let fd = mq_open(b"/mqt_blk", O_CREAT | O_RDWR, Some((2, 16)));
    let pid = fork();
    if pid == 0 {
        // child: sleep a little (timed receive on a private empty queue), then send
        let q = mq_open(b"/mqt_blk", O_RDWR, None);
        let mut buf = [0u8; 4]; let mut pr = 0u32;
        let t0 = now_ns();
        let ts = [(t0 + 60_000_000) / 1_000_000_000, (t0 + 60_000_000) % 1_000_000_000];
        let nobody = mq_open(b"/mqt_blk_empty", O_CREAT | O_RDWR, Some((1, 4)));
        recv(nobody, &mut buf, &mut pr, ts.as_ptr() as i64);
        send(q, b"wake", 7, 0);
        _exit(0);
    }
    let mut buf = [0u8; 16]; let mut pr = 0u32;
    let t0 = now_ns();
    let ts = [(t0 + 5_000_000_000) / 1_000_000_000, (t0 + 5_000_000_000) % 1_000_000_000];
    check(b"blocking receive is woken by a sender", recv(fd, &mut buf, &mut pr, ts.as_ptr() as i64), 4);
    check(b"woken message priority", pr as i64, 7);
    truth(b"woken well before the 5s timeout", now_ns() - t0 < 3_000_000_000);
    let mut st = 0; waitpid(pid, &mut st, 0);
    mq_unlink(b"/mqt_blk_empty");
    close(fd); mq_unlink(b"/mqt_blk");
}

#[repr(C)] struct PollFd { fd: i32, events: i16, revents: i16 }

unsafe fn t_poll() {
    mq_unlink(b"/mqt_poll");
    let fd = mq_open(b"/mqt_poll", O_CREAT | O_RDWR | O_NONBLOCK, Some((1, 8)));
    let mut p = PollFd { fd: fd as i32, events: 1 | 4, revents: 0 };
    let ts = [0i64; 2];
    sc(nr::PPOLL, [&mut p as *mut PollFd as i64, 1, ts.as_ptr() as i64, 0, 0, 0]);
    check(b"empty queue polls POLLOUT only", p.revents as i64, 4);
    send(fd, b"x", 0, 0);
    p.revents = 0;
    sc(nr::PPOLL, [&mut p as *mut PollFd as i64, 1, ts.as_ptr() as i64, 0, 0, 0]);
    check(b"full non-empty queue polls POLLIN only", p.revents as i64, 1);
    close(fd); mq_unlink(b"/mqt_poll");
}

unsafe fn t_notify() {
    mq_unlink(b"/mqt_nf");
    let fd = mq_open(b"/mqt_nf", O_CREAT | O_RDWR | O_NONBLOCK, Some((4, 16)));
    let usr1 = 10i64;
    let mask: u64 = 1 << (usr1 - 1);
    sc(nr::SIGPROCMASK, [0, &mask as *const u64 as i64, 0, 8, 0, 0]); // SIG_BLOCK
    let sev: [i64; 8] = [0x1234, usr1 | (0i64 << 32), 0, 0, 0, 0, 0, 0]; // SIGEV_SIGNAL
    check(b"mq_notify register", sc(nr::MQ_NOTIFY, [fd, sev.as_ptr() as i64, 0, 0, 0, 0]), 0);
    check(b"second mq_notify is EBUSY", sc(nr::MQ_NOTIFY, [fd, sev.as_ptr() as i64, 0, 0, 0, 0]), EBUSY);
    let ts = [0i64; 2]; let mut info = [0u8; 128];
    check(b"no signal before a message", sc(nr::SIGTIMEDWAIT, [&mask as *const u64 as i64, info.as_mut_ptr() as i64, ts.as_ptr() as i64, 8, 0, 0]), -11);
    send(fd, b"n", 0, 0);
    check(b"SIGEV_SIGNAL delivered on first message", sc(nr::SIGTIMEDWAIT, [&mask as *const u64 as i64, info.as_mut_ptr() as i64, ts.as_ptr() as i64, 8, 0, 0]), usr1);
    send(fd, b"m", 0, 0);
    check(b"notification is one-shot", sc(nr::SIGTIMEDWAIT, [&mask as *const u64 as i64, info.as_mut_ptr() as i64, ts.as_ptr() as i64, 8, 0, 0]), -11);
    close(fd); mq_unlink(b"/mqt_nf");
}

unsafe fn t_ipcns() {
    mq_unlink(b"/mqt_ns");
    let fd = mq_open(b"/mqt_ns", O_CREAT | O_RDWR, Some((2, 8)));
    let pid = fork();
    if pid == 0 {
        let seen_before = mq_open(b"/mqt_ns", O_RDWR, None) >= 0;
        if !seen_before { _exit(10); }
        if sc(nr::UNSHARE, [0x0800_0000, 0, 0, 0, 0, 0]) != 0 { _exit(11); }
        if mq_open(b"/mqt_ns", O_RDWR, None) != ENOENT { _exit(12); } // must not see the parent's queue
        let own = mq_open(b"/mqt_ns", O_CREAT | O_EXCL | O_RDWR, Some((1, 4)));
        if own < 0 { _exit(13); }
        _exit(0);
    }
    let mut st = 0; waitpid(pid, &mut st, 0);
    check(b"child: same ns sees queue, new ipc ns does not", ((st >> 8) & 0xff) as i64, 0);
    let mut attr = [0i64; 8];
    sc(nr::MQ_GETSETATTR, [fd, 0, attr.as_mut_ptr() as i64, 0, 0, 0]);
    check(b"parent queue untouched by the child's namesake", attr[1], 2);
    close(fd); mq_unlink(b"/mqt_ns");
}

unsafe fn list_dir(path: &[u8], want: &[u8]) -> bool {
    let mut nb = [0u8; 96]; let p = cs(path, &mut nb);
    let d = sc(nr::OPENAT, [AT_FDCWD, p, O_RDONLY | O_DIRECTORY, 0, 0, 0]);
    if d < 0 { return false; }
    let mut buf = [0u8; 1024]; let mut found = false;
    loop {
        let n = sc(nr::GETDENTS64, [d, buf.as_mut_ptr() as i64, 1024, 0, 0, 0]);
        if n <= 0 { break; }
        let mut off = 0usize;
        while off < n as usize {
            let reclen = u16::from_le_bytes([buf[off + 16], buf[off + 17]]) as usize;
            let nm = &buf[off + 19..off + reclen];
            let l = nm.iter().position(|&c| c == 0).unwrap_or(nm.len());
            if &nm[..l] == want { found = true; }
            off += reclen;
        }
    }
    close(d);
    found
}

unsafe fn t_fs() {
    mq_unlink(b"/mqt_fs");
    let fd = mq_open(b"/mqt_fs", O_CREAT | O_RDWR, Some((4, 16)));
    send(fd, b"hello", 0, 0); send(fd, b"abc", 0, 0);
    truth(b"readdir /dev/mqueue lists the queue", list_dir(b"/dev/mqueue", b"mqt_fs"));
    let mut a = [0u8; 96]; let mut b = [0u8; 96]; let mut c = [0u8; 96];
    sc(nr::MKDIRAT, [AT_FDCWD, cs(b"/tmp/mqmnt", &mut a), 0o755, 0, 0, 0]);
    let r = sc(nr::MOUNT, [cs(b"mqueue", &mut b), cs(b"/tmp/mqmnt", &mut a), cs(b"mqueue", &mut c), 0, 0, 0]);
    check(b"mount -t mqueue mqueue /tmp/mqmnt", r, 0);
    truth(b"readdir of the mount lists the queue", list_dir(b"/tmp/mqmnt", b"mqt_fs"));
    let mut p = [0u8; 96];
    let f = sc(nr::OPENAT, [AT_FDCWD, cs(b"/tmp/mqmnt/mqt_fs", &mut p), O_RDONLY, 0, 0, 0]);
    truth(b"open queue file through the mount", f >= 0);
    let mut txt = [0u8; 96];
    let n = sc(nr::READ, [f, txt.as_mut_ptr() as i64, 96, 0, 0, 0]);
    truth(b"QSIZE file read has text", n > 20);
    truth(b"QSIZE counts 8 queued bytes", n > 8 && &txt[..6] == b"QSIZE:" && txt[6] == b'8' && txt[7] == b' ');
    close(f);
    check(b"unlink through the mount (rm)", sc(nr::UNLINKAT, [AT_FDCWD, cs(b"/tmp/mqmnt/mqt_fs", &mut p), 0, 0, 0, 0]), 0);
    check(b"queue is gone after rm", mq_open(b"/mqt_fs", O_RDWR, None), ENOENT);
    truth(b"readdir no longer lists it", !list_dir(b"/dev/mqueue", b"mqt_fs"));
    close(fd);
}

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    t_priority_order();
    t_nonblock_and_timeout();
    t_blocking_wakeup();
    t_poll();
    t_notify();
    t_ipcns();
    t_fs();
    puts(b"--- mqtest done: "); putn(FAILS as i64); puts(b" failed ---\n");
    FAILS
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! { unsafe { exit(134); } }
