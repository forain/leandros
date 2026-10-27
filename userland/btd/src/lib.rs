//! btd support library: raw syscalls, console output, and the blocking
//! client for `/run/bluetooth/btd.sock` that profile daemons link.
#![no_std]

extern crate alloc;
extern crate leandros_libc;

pub use bt_host;
pub use bt_host::api::{status, DeviceInfo, Msg};
pub use bt_host::Addr;

pub const SOCK_PATH: &[u8] = b"/run/bluetooth/btd.sock\0";
pub const KEYS_PATH: &[u8] = b"/var/lib/bluetooth/linkkeys\0";
pub const KEYS_TMP: &[u8] = b"/var/lib/bluetooth/linkkeys.tmp\0";

pub mod sys {
    use leandros_libc::syscall::{nr, syscall3, syscall4, syscall5, syscall6};

    #[cfg(target_arch = "aarch64")] const SYS_PPOLL: usize = 73;
    #[cfg(target_arch = "x86_64")]  const SYS_PPOLL: usize = 271;
    #[cfg(target_arch = "aarch64")] const SYS_SOCKETPAIR: usize = 199;
    #[cfg(target_arch = "x86_64")]  const SYS_SOCKETPAIR: usize = 53;
    const AT_FDCWD: usize = -100isize as usize;
    pub const O_RDONLY: i32 = 0;
    pub const O_WRONLY: i32 = 1;
    pub const O_RDWR: i32 = 2;
    pub const O_CREAT: i32 = 0o100;
    pub const O_TRUNC: i32 = 0o1000;
    pub const O_NONBLOCK: i32 = 0o4000;
    pub const O_CLOEXEC: i32 = 0o2000000;
    pub const POLLIN: i16 = 1;
    pub const POLLOUT: i16 = 4;
    pub const POLLERR: i16 = 8;
    pub const POLLHUP: i16 = 16;
    pub const EAGAIN: i32 = 11;
    pub const EINTR: i32 = 4;
    pub const EEXIST: i32 = 17;
    const AF_UNIX: usize = 1;
    const SOCK_STREAM: usize = 1;
    const SOCK_CLOEXEC: usize = 0o2000000;
    const MSG_NOSIGNAL: usize = 0x4000;

    fn res(r: isize) -> Result<usize, i32> { if r < 0 { Err((-r) as i32) } else { Ok(r as usize) } }

    /// `path` must end with a NUL.
    pub fn open(path: &[u8], flags: i32, mode: u32) -> Result<i32, i32> {
        res(unsafe { syscall4(nr::OPENAT, AT_FDCWD, path.as_ptr() as usize, flags as usize, mode as usize) }).map(|v| v as i32)
    }
    pub fn close(fd: i32) { unsafe { syscall3(nr::CLOSE, fd as usize, 0, 0); } }
    pub fn read(fd: i32, b: &mut [u8]) -> Result<usize, i32> {
        res(unsafe { syscall3(nr::READ, fd as usize, b.as_mut_ptr() as usize, b.len()) })
    }
    pub fn write(fd: i32, b: &[u8]) -> Result<usize, i32> {
        res(unsafe { syscall3(nr::WRITE, fd as usize, b.as_ptr() as usize, b.len()) })
    }
    /// Writes all of `b` to a socket without raising SIGPIPE.
    pub fn send_all(fd: i32, mut b: &[u8]) -> Result<(), i32> {
        while !b.is_empty() {
            match res(unsafe { syscall6(nr::SENDTO, fd as usize, b.as_ptr() as usize, b.len(), MSG_NOSIGNAL, 0, 0) }) {
                Ok(n) => b = &b[n..],
                Err(EINTR) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    pub fn mkdir(path: &[u8], mode: u32) -> Result<(), i32> {
        match res(unsafe { syscall3(nr::MKDIRAT, AT_FDCWD, path.as_ptr() as usize, mode as usize) }) {
            Ok(_) | Err(EEXIST) => Ok(()),
            Err(e) => Err(e),
        }
    }
    pub fn unlink(path: &[u8]) { unsafe { syscall3(nr::UNLINKAT, AT_FDCWD, path.as_ptr() as usize, 0); } }
    pub fn rename(from: &[u8], to: &[u8]) -> Result<(), i32> {
        res(unsafe { syscall4(nr::RENAMEAT, AT_FDCWD, from.as_ptr() as usize, AT_FDCWD, to.as_ptr() as usize) }).map(|_| ())
    }
    pub fn chmod(path: &[u8], mode: u32) {
        unsafe { syscall4(nr::FCHMODAT, AT_FDCWD, path.as_ptr() as usize, mode as usize, 0); }
    }
    pub fn fsync(fd: i32) {
        #[cfg(target_arch = "aarch64")] const SYS_FSYNC: usize = 82;
        #[cfg(target_arch = "x86_64")]  const SYS_FSYNC: usize = 74;
        unsafe { syscall3(SYS_FSYNC, fd as usize, 0, 0); }
    }

    #[repr(C)]
    struct SockaddrUn { family: u16, path: [u8; 108] }
    fn sun(path: &[u8]) -> (SockaddrUn, usize) {
        let mut a = SockaddrUn { family: AF_UNIX as u16, path: [0; 108] };
        let p = &path[..path.len() - 1]; // without the NUL
        a.path[..p.len()].copy_from_slice(p);
        (a, 2 + p.len() + 1)
    }
    pub fn unix_listen(path: &[u8]) -> Result<i32, i32> {
        let fd = res(unsafe { syscall3(nr::SOCKET, AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0) })? as i32;
        let (a, l) = sun(path);
        let r = res(unsafe { syscall3(nr::BIND, fd as usize, &a as *const _ as usize, l) })
            .and_then(|_| res(unsafe { syscall3(nr::LISTEN, fd as usize, 16, 0) }));
        if let Err(e) = r { close(fd); return Err(e); }
        Ok(fd)
    }
    pub fn unix_connect(path: &[u8]) -> Result<i32, i32> {
        let fd = res(unsafe { syscall3(nr::SOCKET, AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0) })? as i32;
        let (a, l) = sun(path);
        if let Err(e) = res(unsafe { syscall3(nr::CONNECT, fd as usize, &a as *const _ as usize, l) }) { close(fd); return Err(e); }
        Ok(fd)
    }
    pub fn accept(fd: i32) -> Result<i32, i32> {
        res(unsafe { syscall4(nr::ACCEPT4, fd as usize, 0, 0, SOCK_CLOEXEC) }).map(|v| v as i32)
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct PollFd { pub fd: i32, pub events: i16, pub revents: i16 }
    /// ppoll(2); `timeout_ms` < 0 waits forever. Returns the ready count.
    pub fn poll(fds: &mut [PollFd], timeout_ms: i64) -> Result<usize, i32> {
        let ts = [timeout_ms.max(0) / 1000, (timeout_ms.max(0) % 1000) * 1_000_000];
        let tsp = if timeout_ms < 0 { 0 } else { ts.as_ptr() as usize };
        res(unsafe { syscall5(SYS_PPOLL, fds.as_mut_ptr() as usize, fds.len(), tsp, 0, 8) })
    }
    pub fn set_nonblock(fd: i32) {
        const F_GETFL: usize = 3;
        const F_SETFL: usize = 4;
        unsafe {
            let fl = syscall3(nr::FCNTL, fd as usize, F_GETFL, 0);
            if fl >= 0 { syscall3(nr::FCNTL, fd as usize, F_SETFL, fl as usize | O_NONBLOCK as usize); }
        }
    }
    pub fn socketpair() -> Result<[i32; 2], i32> {
        let mut sv = [0i32; 2];
        res(unsafe { syscall4(SYS_SOCKETPAIR, AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv.as_mut_ptr() as usize) })?;
        Ok(sv)
    }
    pub fn now_ms() -> u64 {
        let mut ts = leandros_libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { leandros_libc::clock_gettime(1, &mut ts); }
        ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
    }
}

pub mod out {
    use core::fmt;
    pub struct Out;
    impl fmt::Write for Out {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let mut b = s.as_bytes();
            while !b.is_empty() {
                match super::sys::write(1, b) { Ok(n) if n > 0 => b = &b[n..], _ => return Err(fmt::Error) }
            }
            Ok(())
        }
    }
    pub struct Err2;
    impl fmt::Write for Err2 {
        fn write_str(&mut self, s: &str) -> fmt::Result { let _ = super::sys::write(2, s.as_bytes()); Ok(()) }
    }
}
#[macro_export]
macro_rules! say { ($($t:tt)*) => {{ use core::fmt::Write; let _ = write!($crate::out::Out, $($t)*); }} }
#[macro_export]
macro_rules! sayln { ($($t:tt)*) => {{ use core::fmt::Write; let _ = writeln!($crate::out::Out, $($t)*); }} }
#[macro_export]
macro_rules! eprintln { ($($t:tt)*) => {{ use core::fmt::Write; let _ = writeln!($crate::out::Err2, $($t)*); }} }

pub mod client {
    //! Blocking client for btd. One `Bt` = one socket; channels opened or
    //! accepted through it close when it is dropped.
    use super::sys;
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;
    use bt_host::api::{op, FrameBuf, Msg};
    use bt_host::Addr;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Error {
        /// errno from the socket (btd not running: ENOENT / ECONNREFUSED).
        Io(i32),
        /// a btd status code (`bt_host::api::status`).
        Status(u8),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Channel { pub cid: u16, pub peer_mtu: u16 }

    pub struct Bt { fd: i32, fb: FrameBuf, queue: VecDeque<Msg>, next_req: u16 }

    impl Drop for Bt { fn drop(&mut self) { sys::close(self.fd); } }

    impl Bt {
        pub fn open() -> Result<Bt, Error> { Bt::open_at(super::SOCK_PATH) }
        /// `path` must end with a NUL.
        pub fn open_at(path: &[u8]) -> Result<Bt, Error> {
            let fd = sys::unix_connect(path).map_err(Error::Io)?;
            Ok(Bt { fd, fb: FrameBuf::default(), queue: VecDeque::new(), next_req: 1 })
        }
        /// For poll(2): readable means `next()` will not block for long.
        pub fn fd(&self) -> i32 { self.fd }

        pub fn send_msg(&mut self, m: &Msg) -> Result<(), Error> {
            let mut b = Vec::new();
            m.encode(&mut b);
            sys::send_all(self.fd, &b).map_err(Error::Io)
        }

        fn read_more(&mut self) -> Result<(), Error> {
            let mut buf = [0u8; 2048];
            loop {
                match sys::read(self.fd, &mut buf) {
                    Ok(0) => return Err(Error::Io(104)), // ECONNRESET: btd went away
                    Ok(n) => { self.fb.push(&buf[..n]); return Ok(()); }
                    Err(sys::EINTR) => {}
                    Err(e) => return Err(Error::Io(e)),
                }
            }
        }

        /// The next event (RECV, INCOMING, CLOSED, DEVICE, ...), blocking.
        pub fn next(&mut self) -> Result<Msg, Error> {
            loop {
                if let Some(m) = self.queue.pop_front() { return Ok(m); }
                while let Some(m) = self.fb.next() { if let Some(m) = m { self.queue.push_back(m); } }
                if let Some(m) = self.queue.pop_front() { return Ok(m); }
                self.read_more()?;
            }
        }

        /// Reads until `pick` accepts a message; everything else stays queued for `next()`.
        fn wait<T>(&mut self, mut pick: impl FnMut(&Msg) -> Option<T>) -> Result<T, Error> {
            let mut held = VecDeque::new();
            let r = loop {
                let m = match self.next() { Ok(m) => m, Err(e) => break Err(e) };
                if let Some(t) = pick(&m) { break Ok(t); }
                held.push_back(m);
            };
            while let Some(m) = held.pop_back() { self.queue.push_front(m); }
            r
        }

        /// Opens an L2CAP channel (pages, authenticates and encrypts as needed).
        pub fn connect(&mut self, addr: Addr, psm: u16) -> Result<Channel, Error> {
            let req = self.next_req;
            self.next_req = self.next_req.wrapping_add(1);
            self.send_msg(&Msg::Connect { req, addr, psm })?;
            self.wait(|m| match *m {
                Msg::ConnectResult { req: r, status: 0, cid, peer_mtu } if r == req => Some(Ok(Channel { cid, peer_mtu })),
                Msg::ConnectResult { req: r, status, .. } if r == req => Some(Err(Error::Status(status))),
                _ => None,
            })?
        }

        fn op(&mut self, m: Msg, o: u8) -> Result<(), Error> {
            self.send_msg(&m)?;
            match self.wait(|m| match *m { Msg::Result { op: x, status } if x == o => Some(status), _ => None })? {
                0 => Ok(()),
                s => Err(Error::Status(s)),
            }
        }

        /// Incoming channels on `psm` are delivered to this client as `Msg::Incoming`.
        pub fn listen(&mut self, psm: u16) -> Result<(), Error> { self.op(Msg::Listen { psm }, op::LISTEN) }
        pub fn send(&mut self, cid: u16, data: &[u8]) -> Result<(), Error> { self.send_msg(&Msg::Send { cid, data: data.to_vec() }) }
        pub fn close(&mut self, cid: u16) -> Result<(), Error> { self.send_msg(&Msg::Close { cid }) }
        pub fn pair(&mut self, addr: Addr) -> Result<(), Error> { self.op(Msg::Pair { addr }, op::PAIR) }
        pub fn remove(&mut self, addr: Addr) -> Result<(), Error> { self.op(Msg::Remove { addr }, op::REMOVE) }
        pub fn disconnect(&mut self, addr: Addr) -> Result<(), Error> { self.op(Msg::Disconnect { addr }, op::DISCONNECT) }
        /// Collects DEVICE replies until the RESULT for `o` (SCAN or DEVICES); `each` sees them as they arrive.
        pub fn devices(&mut self, m: Msg, o: u8, mut each: impl FnMut(&bt_host::api::DeviceInfo)) -> Result<(), Error> {
            self.send_msg(&m)?;
            let mut held = VecDeque::new();
            let r = loop {
                match self.next() {
                    Ok(Msg::Device(d)) => each(&d),
                    Ok(Msg::Result { op: x, status }) if x == o => break if status == 0 { Ok(()) } else { Err(Error::Status(status)) },
                    Ok(m) => held.push_back(m),
                    Err(e) => break Err(e),
                }
            };
            while let Some(m) = held.pop_back() { self.queue.push_front(m); }
            r
        }
        pub fn info(&mut self) -> Result<Msg, Error> {
            self.send_msg(&Msg::Info)?;
            self.wait(|m| if matches!(m, Msg::InfoRsp { .. }) { Some(m.clone()) } else { None })
        }
    }
}
