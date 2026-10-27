//! btd — the Bluetooth host daemon.
//!
//! ```text
//! btd [--dev PATH | --fd N] [--name NAME] [--sock PATH] [--keys PATH]
//!     defaults: /dev/hci0, "LeandrOS", /run/bluetooth/btd.sock, /var/lib/bluetooth/linkkeys
//!     (--fd/--sock/--keys exist for bttest, which plays the controller over a socketpair)
//! ```
//! Owns the raw HCI node (whole H4 packets per read/write; a byte stream is
//! re-framed too), runs the bt-host core, serves /run/bluetooth/btd.sock
//! (root only, 0600) and persists link keys in /var/lib/bluetooth/linkkeys.
//! Exits 1 when there is no controller. Contract: docs/servers/bluetooth.md.
#![no_std]
#![no_main]

extern crate alloc;
#[macro_use]
extern crate btd;

use alloc::vec::Vec;
use bt_host::api::FrameBuf;
use bt_host::hci;
use bt_host::host::{Host, Owner, State};
use bt_host::keys::KeyStore;
use btd::sys::{self, PollFd, POLLERR, POLLHUP, POLLIN, POLLOUT};

struct Client { fd: i32, id: Owner, fb: FrameBuf, dead: bool }

unsafe fn arg(argv: *const *const u8, i: isize) -> &'static [u8] {
    let p = *argv.offset(i);
    let mut n = 0;
    while *p.add(n) != 0 { n += 1; }
    core::slice::from_raw_parts(p, n + 1) // keeps the NUL
}

fn load_keys(path: &[u8]) -> KeyStore {
    let Ok(fd) = sys::open(path, sys::O_RDONLY | sys::O_CLOEXEC, 0) else { return KeyStore::default() };
    let mut v = Vec::new();
    let mut b = [0u8; 1024];
    while let Ok(n) = sys::read(fd, &mut b) { if n == 0 { break; } v.extend_from_slice(&b[..n]); }
    sys::close(fd);
    KeyStore::parse(&alloc::string::String::from_utf8_lossy(&v))
}

fn save_keys(path: &[u8], tmp: &[u8], text: &str) {
    let fd = match sys::open(tmp, sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC | sys::O_CLOEXEC, 0o600) {
        Ok(fd) => fd,
        Err(e) => return eprintln!("[btd] cannot write link keys: errno {}", e),
    };
    let mut b = text.as_bytes();
    while !b.is_empty() { match sys::write(fd, b) { Ok(n) if n > 0 => b = &b[n..], _ => break } }
    sys::fsync(fd);
    sys::close(fd);
    if let Err(e) = sys::rename(tmp, path) { eprintln!("[btd] cannot store link keys: errno {}", e); }
}

/// Writes one H4 packet; waits for POLLOUT on EAGAIN.
fn write_pkt(dev: i32, pkt: &[u8]) -> bool {
    for _ in 0..50 {
        match sys::write(dev, pkt) {
            Ok(_) => return true,
            Err(sys::EAGAIN) | Err(sys::EINTR) => { let _ = sys::poll(&mut [PollFd { fd: dev, events: POLLOUT, revents: 0 }], 100); }
            Err(e) => { eprintln!("[btd] hci write failed: errno {}", e); return false; }
        }
    }
    eprintln!("[btd] hci write stalled, packet dropped");
    true
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let mut dev_path: &[u8] = b"/dev/hci0\0";
    let mut name = "LeandrOS";
    let mut dev_fd: Option<i32> = None;
    let mut sock: &[u8] = btd::SOCK_PATH;
    let mut keys: &[u8] = btd::KEYS_PATH;
    let mut i = 1;
    while i < argc as isize {
        match arg(argv, i) {
            b"--dev\0" if i + 1 < argc as isize => { i += 1; dev_path = arg(argv, i); }
            b"--name\0" if i + 1 < argc as isize => {
                i += 1;
                let a = arg(argv, i);
                name = core::str::from_utf8(&a[..a.len() - 1]).unwrap_or("LeandrOS");
            }
            b"--fd\0" if i + 1 < argc as isize => {
                i += 1;
                let a = arg(argv, i);
                dev_fd = core::str::from_utf8(&a[..a.len() - 1]).ok().and_then(|s| s.parse().ok());
            }
            b"--sock\0" if i + 1 < argc as isize => { i += 1; sock = arg(argv, i); }
            b"--keys\0" if i + 1 < argc as isize => { i += 1; keys = arg(argv, i); }
            _ => { eprintln!("usage: btd [--dev PATH | --fd N] [--name NAME] [--sock PATH] [--keys PATH]"); return 2; }
        }
        i += 1;
    }
    let shown = core::str::from_utf8(&dev_path[..dev_path.len() - 1]).unwrap_or("?");
    let mut keys_tmp = keys[..keys.len() - 1].to_vec();
    keys_tmp.extend_from_slice(b".tmp\0");
    let opened = match dev_fd { Some(fd) => Ok(fd), None => sys::open(dev_path, sys::O_RDWR | sys::O_NONBLOCK | sys::O_CLOEXEC, 0) };
    let dev = match opened {
        Ok(fd) => { sys::set_nonblock(fd); fd }
        Err(e) => { eprintln!("[btd] no controller: {}: errno {}", shown, e); return 1; }
    };
    let _ = sys::mkdir(b"/run\0", 0o755);
    let _ = sys::mkdir(b"/run/bluetooth\0", 0o755);
    let _ = sys::mkdir(b"/var\0", 0o755);
    let _ = sys::mkdir(b"/var/lib\0", 0o755);
    let _ = sys::mkdir(b"/var/lib/bluetooth\0", 0o700);
    sys::unlink(sock);
    let lfd = match sys::unix_listen(sock) {
        Ok(fd) => fd,
        Err(e) => { eprintln!("[btd] cannot listen on /run/bluetooth/btd.sock: errno {}", e); return 1; }
    };
    sys::chmod(sock, 0o600);
    match dev_fd {
        Some(fd) => eprintln!("[btd] controller on fd {}, initialising", fd),
        None => eprintln!("[btd] controller {} open, initialising", shown),
    }

    let mut host = Host::new(name, load_keys(keys), sys::now_ms());
    let mut clients: Vec<Client> = Vec::new();
    let mut next_id: Owner = 1;
    let mut rx: Vec<u8> = Vec::new();
    let mut buf = alloc::vec![0u8; 4096];
    host.tick(sys::now_ms());
    loop {
        while let Some(p) = host.take_out() {
            if !write_pkt(dev, &p) { return 1; }
        }
        if let Some(text) = host.take_keys_dirty() { save_keys(keys, &keys_tmp, &text); }
        while let Some((o, m)) = host.take_event() {
            if let Some(c) = clients.iter_mut().find(|c| c.id == o && !c.dead) {
                let mut b = Vec::new();
                m.encode(&mut b);
                if sys::send_all(c.fd, &b).is_err() { c.dead = true; }
            }
        }
        while let Some(l) = host.take_log() { eprintln!("[btd] {}", l); }
        if host.state == State::Dead { eprintln!("[btd] controller dead, exiting"); return 1; }
        let mut gone = Vec::new();
        clients.retain(|c| if c.dead { sys::close(c.fd); gone.push(c.id); false } else { true });
        for id in gone { host.drop_owner(id); }

        let mut fds: Vec<PollFd> = Vec::with_capacity(2 + clients.len());
        fds.push(PollFd { fd: dev, events: POLLIN, revents: 0 });
        fds.push(PollFd { fd: lfd, events: POLLIN, revents: 0 });
        for c in &clients { fds.push(PollFd { fd: c.fd, events: POLLIN, revents: 0 }); }
        // Timers matter only while something is in flight; idle btd sleeps without wakeups.
        let timeout = if host.busy() { 100 } else { -1 };
        let _ = sys::poll(&mut fds, timeout);
        host.tick(sys::now_ms());

        if fds[0].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
            loop {
                match sys::read(dev, &mut buf) {
                    Ok(0) => { eprintln!("[btd] controller closed"); return 1; }
                    Ok(n) => rx.extend_from_slice(&buf[..n]),
                    Err(sys::EAGAIN) => break,
                    Err(sys::EINTR) => {}
                    Err(e) => { eprintln!("[btd] hci read failed: errno {}", e); return 1; }
                }
            }
            loop {
                match hci::h4_len(&rx) {
                    Some(Ok(n)) if rx.len() >= n => { let p: Vec<u8> = rx.drain(..n).collect(); host.on_packet(&p); }
                    Some(Err(())) => { eprintln!("[btd] bad H4 type 0x{:02x}, resyncing", rx[0]); rx.remove(0); }
                    _ => break,
                }
            }
        }
        if fds[1].revents & POLLIN != 0 {
            if let Ok(fd) = sys::accept(lfd) {
                clients.push(Client { fd, id: next_id, fb: FrameBuf::default(), dead: false });
                next_id = next_id.wrapping_add(1).max(1);
            }
        }
        for (k, f) in fds[2..].iter().enumerate() {
            if f.revents == 0 { continue; }
            let c = &mut clients[k];
            match sys::read(c.fd, &mut buf) {
                Ok(n) if n > 0 => {
                    c.fb.push(&buf[..n]);
                    while let Some(m) = c.fb.next() {
                        if let Some(m) = m { host.request(c.id, m); }
                    }
                }
                Err(sys::EINTR) | Err(sys::EAGAIN) => {}
                _ => c.dead = true,
            }
        }
    }
}
