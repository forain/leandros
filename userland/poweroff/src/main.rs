//! poweroff / reboot / halt — one binary, the action picked by argv[0] (the
//! image hardlinks all three names to it), like systemctl's and busybox's.
//!
//!   poweroff|reboot|halt          ask init for an orderly shutdown
//!                                 (`/run/user/initctl`, userland/init): every
//!                                 process gets SIGTERM, then SIGKILL, the
//!                                 filesystems are synced and remounted
//!                                 read-only, then reboot(2)
//!   ... -f | --force              skip init: sync(2), then reboot(2) at once
//!                                 (root only: reboot(2) needs CAP_SYS_BOOT)
//!   halt -p | --poweroff          power off instead of halting
//!   reboot|halt --reboot / poweroff --halt  override the action
//!
//!   systemctl poweroff|reboot|halt   the same, systemctl-style
//!
//! A non-root caller with a D-Bus system bus (the graphical session, the
//! greeter) goes through login1 like systemctl does: this execs
//! `/usr/libexec/leandros-sysbus power PowerOff|Reboot|Halt`, and login1
//! forwards to init. Without a bus (console, ssh) it asks init directly.
//! Either way init grants the request to root and to processes in a local
//! session (the console logins and the graphical session), as logind's
//! default polkit policy (`allow_active`) does; reboot(2) itself stays
//! root-only.

#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{write, read, close, STDOUT_FILENO, STDERR_FILENO};
use leandros_libc::syscall::{nr, syscall0, syscall3, syscall4};

const LINUX_REBOOT_MAGIC1: usize = 0xfee1_dead;
const LINUX_REBOOT_MAGIC2: usize = 672_274_793;
const LINUX_REBOOT_CMD_RESTART: usize = 0x0123_4567;
const LINUX_REBOOT_CMD_HALT: usize = 0xCDEF_0123;
const LINUX_REBOOT_CMD_POWER_OFF: usize = 0x4321_FEDC;

#[derive(Clone, Copy, PartialEq)]
enum Action { PowerOff, Reboot, Halt }

impl Action {
    fn name(self) -> &'static [u8] {
        match self { Action::PowerOff => b"poweroff", Action::Reboot => b"reboot", Action::Halt => b"halt" }
    }
    fn cmd(self) -> usize {
        match self {
            Action::PowerOff => LINUX_REBOOT_CMD_POWER_OFF,
            Action::Reboot => LINUX_REBOOT_CMD_RESTART,
            Action::Halt => LINUX_REBOOT_CMD_HALT,
        }
    }
}

unsafe fn out(fd: i32, s: &[u8]) {
    write(fd, s.as_ptr(), s.len());
}

unsafe fn cstr<'a>(p: *const u8) -> &'a [u8] {
    if p.is_null() { return &[]; }
    let mut n = 0;
    while *p.add(n) != 0 { n += 1; }
    core::slice::from_raw_parts(p, n)
}

fn errno_name(e: isize) -> &'static [u8] {
    match -e {
        1 => b"Operation not permitted",
        2 => b"No such file or directory",
        13 => b"Permission denied",
        22 => b"Invalid argument",
        111 => b"Connection refused",
        _ => b"error",
    }
}

/// Ask init. Ok(()) once init accepted; Err(reply or errno text) otherwise.
unsafe fn ask_init(a: Action) -> Result<(), &'static [u8]> {
    const AF_UNIX: usize = 1;
    const SOCK_STREAM: usize = 1;
    const SOCK_CLOEXEC: usize = 0x8_0000;
    let fd = syscall3(nr::SOCKET, AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if fd < 0 { return Err(errno_name(fd)); }
    let fd = fd as i32;
    let path = b"/run/user/initctl\0";
    let mut addr = [0u8; 110];
    addr[0] = AF_UNIX as u8;
    addr[2..2 + path.len()].copy_from_slice(path);
    let r = syscall3(nr::CONNECT, fd as usize, addr.as_ptr() as usize, 2 + path.len());
    if r < 0 { close(fd); return Err(errno_name(r)); }
    let mut req = [0u8; 16];
    let name = a.name();
    req[..name.len()].copy_from_slice(name);
    req[name.len()] = b'\n';
    write(fd, req.as_ptr(), name.len() + 1);
    let mut buf = [0u8; 16];
    let mut n = 0usize;
    while n < buf.len() {
        let r = read(fd, buf.as_mut_ptr().add(n), buf.len() - n);
        if r <= 0 { break; }
        n += r as usize;
        if buf[..n].contains(&b'\n') { break; }
    }
    close(fd);
    match &buf[..n] {
        b"ok\n" => Ok(()),
        b"denied\n" => Err(b"Access denied (not root, and not in a local session)"),
        _ => Err(b"init did not accept the request"),
    }
}

/// True when the environment names a D-Bus system (or session) bus.
unsafe fn has_bus(envp: *const *const u8) -> bool {
    if envp.is_null() { return false; }
    let mut i = 0;
    loop {
        let e = *envp.add(i);
        if e.is_null() { return false; }
        let v = cstr(e);
        for k in [&b"DBUS_SYSTEM_BUS_ADDRESS="[..], &b"DBUS_SESSION_BUS_ADDRESS="[..]] {
            if v.len() > k.len() && v.starts_with(k) { return true; }
        }
        i += 1;
    }
}

/// Exec the login1 client; returns only if the exec failed.
unsafe fn via_login1(a: Action, envp: *const *const u8) {
    let method: &[u8] = match a {
        Action::PowerOff => b"PowerOff\0",
        Action::Reboot => b"Reboot\0",
        Action::Halt => b"Halt\0",
    };
    let bin = b"/usr/libexec/leandros-sysbus\0";
    let args = [bin.as_ptr(), b"power\0".as_ptr(), method.as_ptr(), core::ptr::null()];
    leandros_libc::process::execve(bin.as_ptr(), args.as_ptr(), envp);
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, envp: *const *const u8) -> i32 {
    let arg0 = cstr(*argv);
    let base = match arg0.iter().rposition(|&b| b == b'/') { Some(i) => &arg0[i + 1..], None => arg0 };
    let mut first = 1isize;
    let mut action = match base {
        b"reboot" => Action::Reboot,
        b"halt" => Action::Halt,
        b"systemctl" => {
            first = 2;
            match if argc > 1 { cstr(*argv.offset(1)) } else { &[] } {
                b"poweroff" => Action::PowerOff,
                b"reboot" => Action::Reboot,
                b"halt" => Action::Halt,
                _ => {
                    out(STDERR_FILENO, b"usage: systemctl poweroff|reboot|halt [-f|--force]\n");
                    return 2;
                }
            }
        }
        _ => Action::PowerOff,
    };
    let mut force = false;
    for i in first..argc as isize {
        match cstr(*argv.offset(i)) {
            b"-f" | b"--force" | b"-ff" => force = true,
            b"-p" | b"--poweroff" => action = Action::PowerOff,
            b"--reboot" => action = Action::Reboot,
            b"--halt" => action = Action::Halt,
            b"-n" | b"--no-sync" | b"-w" | b"--wtmp-only" | b"-d" | b"--no-wtmp"
            | b"--no-wall" | b"-i" => {}
            b"-h" | b"--help" => {
                out(STDOUT_FILENO, b"usage: poweroff|reboot|halt [-f|--force] [-p|--poweroff] [--reboot] [--halt]\n");
                return 0;
            }
            other => {
                out(STDERR_FILENO, base);
                out(STDERR_FILENO, b": unknown option ");
                out(STDERR_FILENO, other);
                out(STDERR_FILENO, b"\n");
                return 2;
            }
        }
    }

    if !force && leandros_libc::process::getuid() != 0 && has_bus(envp) {
        via_login1(action, envp);
    }

    if !force {
        match ask_init(action) {
            Ok(()) => return 0,
            Err(e) => {
                out(STDERR_FILENO, base);
                out(STDERR_FILENO, b": ");
                out(STDERR_FILENO, e);
                out(STDERR_FILENO, b"\n");
                // init unreachable: like systemctl without a running manager,
                // root may still go down directly.
                if e == b"Access denied (not root, and not in a local session)" { return 1; }
                out(STDERR_FILENO, base);
                out(STDERR_FILENO, b": falling back to --force\n");
            }
        }
    }

    syscall0(nr::SYNC);
    let r = syscall4(nr::REBOOT, LINUX_REBOOT_MAGIC1, LINUX_REBOOT_MAGIC2, action.cmd(), 0);
    out(STDERR_FILENO, base);
    out(STDERR_FILENO, b": reboot(2): ");
    out(STDERR_FILENO, errno_name(r));
    out(STDERR_FILENO, b"\n");
    1
}
