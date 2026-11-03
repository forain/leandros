//! loadkmap — load a console keymap into the kernel (`KDSKBENT`).
//!
//! The binary keymap format is the one kbd's `loadkeys -b` writes and busybox
//! `loadkmap` reads: the magic `bkeymap`, 256 flag bytes (1 = this keymap is
//! present), then for every present keymap its entries as native-endian u16
//! keysyms, one per keycode. kbd writes 128 per map (NR_KEYS / 2, for
//! busybox); the count is taken from the file size, so 256-entry files load
//! too. Keymaps the file does not carry are deallocated, as a real `loadkeys`
//! does, so a loaded map defines the whole layout.
//!
//!     loadkmap < file.bmap        busybox-compatible
//!     loadkmap NAME|PATH          NAME = /usr/share/bkeymaps/<dir>/<file>.bmap.gz
//!                                 (Alpine's kbd-bkeymaps; the aliases de-latin1,
//!                                 fr-latin1, uk, br-abnt2... map onto its layout;
//!                                 .gz files are read through `busybox.static zcat`)
//!     loadkmap --vconsole         /etc/vconsole.conf KEYMAP= (none: no-op)
//!
//! Keymaps are global to the console, as on Linux; any VT fd will do, and
//! `/dev/tty0` is used. Needs root (`KDSKBENT` is EPERM otherwise).

#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{close, dup3, execve, exit, fork, ioctl, open, pipe, read, wait4, write, O_RDONLY, O_RDWR, STDIN_FILENO};

const KDSKBENT: usize = 0x4B47;
const K_NOSUCHMAP: u16 = 0x027f;
const MAX_NR_KEYMAPS: usize = 256;
const MAGIC: &[u8; 7] = b"bkeymap";

/// Big enough for 256 maps of 256 keys; in `.bss`, not on the stack.
static mut BUF: [u8; 7 + MAX_NR_KEYMAPS + MAX_NR_KEYMAPS * 256 * 2] =
    [0; 7 + MAX_NR_KEYMAPS + MAX_NR_KEYMAPS * 256 * 2];

fn out(s: &[u8]) { unsafe { write(2, s.as_ptr(), s.len()); } }

fn out_u(mut v: usize) {
    let mut d = [0u8; 20];
    let mut i = d.len();
    if v == 0 { i -= 1; d[i] = b'0'; }
    while v > 0 { i -= 1; d[i] = b'0' + (v % 10) as u8; v /= 10; }
    out(&d[i..]);
}

unsafe fn read_all(fd: i32) -> Option<usize> {
    let buf = &mut *core::ptr::addr_of_mut!(BUF);
    let mut n = 0usize;
    loop {
        if n == buf.len() { return Some(n); }
        let r = read(fd, buf.as_mut_ptr().add(n), buf.len() - n);
        if r < 0 { return None; }
        if r == 0 { return Some(n); }
        n += r as usize;
    }
}

/// Read `/etc/vconsole.conf` and return the KEYMAP= value, unquoted.
unsafe fn vconsole_keymap(name: &mut [u8; 128]) -> Option<usize> {
    let fd = open(b"/etc/vconsole.conf\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 { return None; }
    let mut b = [0u8; 2048];
    let n = read(fd, b.as_mut_ptr(), b.len());
    close(fd);
    if n <= 0 { return None; }
    let text = &b[..n as usize];
    let mut found = None;
    for line in text.split(|&c| c == b'\n') {
        let mut l = line;
        while let [b' ' | b'\t', rest @ ..] = l { l = rest; }
        if !l.starts_with(b"KEYMAP=") { continue; }
        let mut v = &l[7..];
        while let [rest @ .., b' ' | b'\t' | b'\r'] = v { v = rest; }
        if v.len() >= 2 && (v[0] == b'"' || v[0] == b'\'') && v[v.len() - 1] == v[0] {
            v = &v[1..v.len() - 1];
        }
        if v.is_empty() || v.len() >= name.len() - 32 { continue; }
        name[..v.len()].copy_from_slice(v);
        found = Some(v.len());
    }
    found
}

/// Alias table: console keymap names from kbd that kbd-bkeymaps lays out
/// differently -> (directory, file) under /usr/share/bkeymaps. Anything else
/// is `<name>/<name>`.
fn alias(arg: &[u8]) -> (&[u8], &[u8]) {
    match arg {
        b"de" | b"de-latin1" => (b"de", b"de"),
        b"de-latin1-nodeadkeys" => (b"de", b"de-nodeadkeys"),
        b"fr-latin1" => (b"fr", b"fr"),
        b"uk" => (b"gb", b"gb"),
        b"br-abnt2" => (b"br", b"br"),
        _ => (arg, arg),
    }
}

/// Resolve NAME or PATH to a NUL-terminated path in `path`.
fn resolve(arg: &[u8], path: &mut [u8; 256]) -> bool {
    let parts: [&[u8]; 5];
    if arg.contains(&b'/') {
        parts = [arg, b"", b"", b"", b""];
    } else {
        let (d, f) = alias(arg);
        parts = [b"/usr/share/bkeymaps/", d, b"/", f, b".bmap.gz"];
    }
    let mut n = 0;
    for p in parts {
        if n + p.len() + 1 > path.len() { return false; }
        path[n..n + p.len()].copy_from_slice(p);
        n += p.len();
    }
    path[n] = 0;
    true
}

/// Read a .gz keymap by spawning `busybox.static zcat PATH` (no inflate here).
unsafe fn read_gz(path: *const u8) -> Option<usize> {
    let mut fds = [0i32; 2];
    if pipe(fds.as_mut_ptr()) < 0 { return None; }
    let pid = fork();
    if pid < 0 { close(fds[0]); close(fds[1]); return None; }
    if pid == 0 {
        dup3(fds[1], 1, 0);
        close(fds[0]);
        close(fds[1]);
        let argv: [*const u8; 4] = [b"busybox.static\0".as_ptr(), b"zcat\0".as_ptr(), path, core::ptr::null()];
        let envp: [*const u8; 1] = [core::ptr::null()];
        execve(b"/bin/busybox.static\0".as_ptr(), argv.as_ptr(), envp.as_ptr());
        exit(127);
    }
    close(fds[1]);
    let n = read_all(fds[0]);
    close(fds[0]);
    let mut st = 0i32;
    wait4(pid, &mut st, 0, core::ptr::null_mut());
    if st != 0 { return None; }
    n
}

unsafe fn apply(len: usize, label: &[u8]) -> i32 {
    let buf = &*core::ptr::addr_of!(BUF);
    if len < 7 + MAX_NR_KEYMAPS || &buf[..7] != MAGIC {
        out(b"loadkmap: "); out(label); out(b": not a binary keymap\n");
        return 1;
    }
    let flags = &buf[7..7 + MAX_NR_KEYMAPS];
    let maps = flags.iter().filter(|&&f| f == 1).count();
    let body = len - 7 - MAX_NR_KEYMAPS;
    if maps == 0 || body % (maps * 2) != 0 || body / (maps * 2) > 256 {
        out(b"loadkmap: "); out(label); out(b": bad size\n");
        return 1;
    }
    let keys = body / (maps * 2);
    let fd = open(b"/dev/tty0\0".as_ptr(), O_RDWR, 0);
    if fd < 0 { out(b"loadkmap: cannot open /dev/tty0\n"); return 1; }
    let mut off = 7 + MAX_NR_KEYMAPS;
    let mut fails = 0usize;
    for m in 0..MAX_NR_KEYMAPS {
        if flags[m] != 1 { continue; }
        for k in 0..keys {
            let v = u16::from_ne_bytes([buf[off], buf[off + 1]]);
            off += 2;
            // struct kbentry { u8 kb_table; u8 kb_index; u16 kb_value; }
            let mut ke = [m as u8, k as u8, 0, 0];
            ke[2..4].copy_from_slice(&v.to_ne_bytes());
            if ioctl(fd, KDSKBENT, ke.as_mut_ptr() as usize) < 0 { fails += 1; }
        }
    }
    // Drop the maps the file does not define (never map 0).
    for m in 1..16 {
        if flags[m] == 1 { continue; }
        let mut ke = [m as u8, 0, 0, 0];
        ke[2..4].copy_from_slice(&K_NOSUCHMAP.to_ne_bytes());
        ioctl(fd, KDSKBENT, ke.as_mut_ptr() as usize);
    }
    close(fd);
    out(b"loadkmap: "); out(label); out(b": "); out_u(maps); out(b" keymaps x ");
    out_u(keys); out(b" keys");
    if fails > 0 { out(b", "); out_u(fails); out(b" entries refused"); }
    out(b"\n");
    if fails > 0 { 1 } else { 0 }
}

unsafe fn cstr<'a>(p: *const u8) -> &'a [u8] {
    let mut n = 0;
    while *p.add(n) != 0 { n += 1; }
    core::slice::from_raw_parts(p, n)
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argc < 2 {
        let n = match read_all(STDIN_FILENO) { Some(n) => n, None => { out(b"loadkmap: read error\n"); return 1; } };
        return apply(n, b"stdin");
    }
    let a = cstr(*argv.add(1));
    let mut name = [0u8; 128];
    let arg: &[u8] = if a == b"--vconsole" {
        match vconsole_keymap(&mut name) {
            Some(n) => &name[..n],
            None => return 0,
        }
    } else {
        a
    };
    let mut path = [0u8; 256];
    if !resolve(arg, &mut path) { out(b"loadkmap: name too long\n"); return 1; }
    let plen = path.iter().position(|&c| c == 0).unwrap_or(0);
    let n = if path[..plen].ends_with(b".gz") {
        read_gz(path.as_ptr())
    } else {
        let fd = open(path.as_ptr(), O_RDONLY, 0);
        if fd < 0 {
            out(b"loadkmap: cannot open "); out(&path[..plen]); out(b"\n");
            return 1;
        }
        let n = read_all(fd);
        close(fd);
        n
    };
    match n {
        Some(n) => apply(n, arg),
        None => { out(b"loadkmap: read error\n"); 1 }
    }
}
