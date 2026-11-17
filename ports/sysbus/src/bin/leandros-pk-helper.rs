//! leandros-pk-helper: the set-user-ID-root half of the PackageKit facade
//! (`leandros-sysbus packagekit`, which runs as the session user).
//!
//!   leandros-pk-helper <update|add|del|upgrade> [--] [names...]
//!
//! Authorises the REAL uid (root, or a member of wheel, gid 10 -- the same
//! admin group as /etc/sudoers), validates every package name, clears the
//! environment, becomes root and execs /usr/sbin/apk (the LeandrOS wrapper
//! that refuses to remove leandros-base/musl) with fixed flags. Exit 77 =
//! not authorised.

use std::os::unix::process::CommandExt;
use std::process::Command;

const WHEEL: libc::gid_t = 10;
const APK: &str = "/usr/sbin/apk";

fn die(code: i32, msg: &str) -> ! {
    eprintln!("leandros-pk-helper: {msg}");
    std::process::exit(code);
}

fn authorised() -> bool {
    // SAFETY: plain libc getters; getgroups is called with a buffer of the size it reported.
    unsafe {
        if libc::getuid() == 0 {
            return true;
        }
        if libc::getgid() == WHEEL {
            return true;
        }
        let n = libc::getgroups(0, std::ptr::null_mut());
        if n > 0 {
            let mut g = vec![0 as libc::gid_t; n as usize];
            let n = libc::getgroups(n, g.as_mut_ptr());
            if n > 0 {
                return g[..n as usize].contains(&WHEEL);
            }
        }
    }
    false
}

fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'+' | b'-'))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(op) = args.next() else { die(2, "usage: leandros-pk-helper <update|add|del|upgrade> [--] names...") };
    if !authorised() {
        die(77, "not authorised (administrators are root or members of the wheel group)");
    }
    let mut names: Vec<String> = args.collect();
    if names.first().map(String::as_str) == Some("--") {
        names.remove(0);
    }
    for n in &names {
        if !valid_name(n) {
            die(2, &format!("invalid package name '{n}'"));
        }
        if op == "del" && (n == "leandros-base" || n == "musl") {
            die(2, &format!("'{n}' is part of the base system and cannot be removed"));
        }
    }
    let mut cmd = Command::new(APK);
    match op.as_str() {
        "update" => {
            if !names.is_empty() {
                die(2, "update takes no names");
            }
            cmd.arg("update");
        }
        "add" | "del" => {
            if names.is_empty() {
                die(2, "no package names");
            }
            // Names cannot start with "-", so no "--" is needed; stdin is not
            // a tty, so apk never prompts (no --no-interactive in apk-tools 3).
            cmd.arg(&op).args(&names);
        }
        "upgrade" => {
            cmd.arg("upgrade").args(&names);
        }
        _ => die(2, "unknown operation"),
    }
    cmd.env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").env("HOME", "/root");
    // SAFETY: plain libc calls; the order drops supplementary groups first.
    unsafe {
        libc::umask(0o022);
        libc::setgroups(0, std::ptr::null());
        if libc::setgid(0) != 0 || libc::setuid(0) != 0 {
            die(1, "cannot become root (is the helper set-user-ID root?)");
        }
    }
    let e = cmd.exec();
    die(1, &format!("exec {APK}: {e}"));
}
