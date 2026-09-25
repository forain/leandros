//! greeter-launch — drop privileges to the greeter account, then exec the greeter.
//!
//! cosmic-greeter has no flag and no environment variable that selects its role.
//! It calls `getpwuid(getuid())` and takes the greeter arm only when the name
//! that comes back is `cosmic-greeter`; anything else runs the lock screen
//! (cosmic-greeter/src/main.rs). So the *account* is the role selector, and the
//! only honest way to reach the greeter role is to actually be that account.
//!
//! This binary is the privilege boundary of the greeter phase. cosmic-comp runs
//! as root — libseat is a shim with no seatd behind it, there is no VT layer,
//! and the /dev/dri and /dev/input node modes are unverified — so the compositor
//! stays privileged and only its kiosk child crosses down. cosmic-comp spawns
//! this program with the compositor environment injected; it looks the greeter
//! account up in /etc/passwd, rewrites the per-user part of the environment,
//! setresgid/setresuid's, and execs the greeter binary. It never forks: the
//! greeter inherits this pid, so cosmic-comp's kiosk-child exit handling and
//! greetd's session lifetime both keep working unchanged.
//!
//! The two sockets the greeter must reach, and why this launcher chowns them.
//! The VFS enforces POSIX permissions on path traversal and on AF_UNIX connect
//! (write on the socket inode). The compositor runs as root and binds its
//! wayland socket 0755 root in $XDG_RUNTIME_DIR; greetd binds its control
//! socket 0755 and chowns it to `default_session.user`, which is root here
//! because the compositor is the session. A uid-990 client can write to
//! neither. So, while still root, this launcher chowns both nodes —
//! `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY` and `$GREETD_SOCK` — to the greeter
//! account, which is exactly the ownership upstream greetd arranges (its
//! socket is chowned to the greeter user, and the greeter's compositor runs as
//! that user). The directory they live in is the greeter account's own runtime
//! dir, `/run/user/990`, selected by /etc/profile for the greeter session class
//! and by /bin/greeter-env for greetd itself, so traversal is the owner's.

#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{
    chdir, chown, close, execve, exit, getuid, open, read, setgroups, setresgid, setresuid,
    write, O_RDONLY, STDERR_FILENO,
};

/// The account name cosmic-greeter matches on. Staged by
/// scripts/mkfs-f2fs-populated.py with a uid below UID_MIN so the greeter's own
/// account never appears in the list of users it offers for login.
const GREETER_USER: &[u8] = b"cosmic-greeter";

/// The greeter binary, staged under a name cosmic-session does not spawn so that
/// the in-session lock-screen role is never started at all.
const GREETER_BIN: &[u8] = b"/bin/cosmic-greeter-login\0";

const PASSWD_PATH: &[u8] = b"/etc/passwd\0";
const GROUP_PATH: &[u8] = b"/etc/group\0";
const MAX_GROUPS: usize = 32;

/// Environment names this launcher replaces. Everything else is inherited
/// verbatim — XDG_RUNTIME_DIR and WAYLAND_DISPLAY name the compositor's socket,
/// GREETD_SOCK names the daemon's, and the render settings are load-bearing.
///
/// DBUS_SYSTEM_BUS_ADDRESS is dropped rather than replaced. greeter::main calls
/// user_data_dbus() before it builds any window, against a bus name owned only
/// by cosmic-greeter-daemon, which is not built; busd never replies to an
/// unowned name and neither side has a timeout, so a set value is a permanent
/// hang on a blank screen. Unset, the connection fails fast and the greeter
/// falls through to reading /etc/passwd.
const REPLACED: &[&[u8]] = &[
    b"DBUS_SYSTEM_BUS_ADDRESS=",
    b"HOME=",
    b"USER=",
    b"LOGNAME=",
    b"SHELL=",
    b"XDG_CONFIG_HOME=",
    b"XDG_CACHE_HOME=",
    b"XDG_DATA_HOME=",
    b"XDG_STATE_HOME=",
];

const MAX_ENV: usize = 192;
const MAX_ARGV: usize = 32;
const ARENA_CAP: usize = 1024;
const PASSWD_CAP: usize = 8192;
const HOME_CAP: usize = 128;

fn emit(s: &[u8]) {
    unsafe {
        write(STDERR_FILENO, s.as_ptr(), s.len());
    }
}

fn emit_u32(mut v: u32) {
    let mut buf = [0u8; 10];
    let mut n = 0;
    if v == 0 {
        emit(b"0");
        return;
    }
    while v > 0 && n < buf.len() {
        buf[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    let mut out = [0u8; 10];
    for i in 0..n {
        out[i] = buf[n - 1 - i];
    }
    emit(&out[..n]);
}

/// A NUL-terminated string arena. Every string this program adds to the child's
/// environment is built here, so the pointers handed to execve stay valid for as
/// long as the process does.
struct Arena {
    buf: [u8; ARENA_CAP],
    len: usize,
}

impl Arena {
    const fn new() -> Self {
        Arena {
            buf: [0u8; ARENA_CAP],
            len: 0,
        }
    }

    /// Concatenate `parts`, NUL-terminate, and return a pointer to the result.
    /// Returns null if the arena is full, which the caller treats as fatal
    /// rather than shipping a truncated environment.
    fn push(&mut self, parts: &[&[u8]]) -> *const u8 {
        let start = self.len;
        let mut need = 1;
        for p in parts {
            need += p.len();
        }
        if start + need > ARENA_CAP {
            return core::ptr::null();
        }
        for p in parts {
            for &b in *p {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
        self.buf[self.len] = 0;
        self.len += 1;
        unsafe { self.buf.as_ptr().add(start) }
    }
}

/// The `n`th colon-separated field of a passwd line, or None if the line is short.
fn field(line: &[u8], n: usize) -> Option<&[u8]> {
    let mut idx = 0usize;
    let mut start = 0usize;
    let mut i = 0usize;
    while i <= line.len() {
        if i == line.len() || line[i] == b':' {
            if idx == n {
                return Some(&line[start..i]);
            }
            idx += 1;
            start = i + 1;
        }
        i += 1;
    }
    None
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 10 {
        return None;
    }
    let mut v: u32 = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(v)
}

struct Account {
    uid: u32,
    gid: u32,
    home: [u8; HOME_CAP],
    home_len: usize,
}

/// Supplementary groups of `username` from /etc/group: every group whose
/// member list names it. Byte-oriented like the rest of this module (no str
/// parsing crate to pull into a `no_std` binary) — mirrors `/bin/login`'s
/// `lookup_groups`/`initgroups`, duplicated rather than shared since this is
/// a separate crate. Without this, `setresgid`/`setresuid` below dropped to
/// the greeter account's primary group only, and the greeter session ran
/// with zero supplementary groups no matter what `/etc/group` said.
unsafe fn lookup_groups(username: &[u8], out: &mut [u32; MAX_GROUPS]) -> usize {
    let fd = open(GROUP_PATH.as_ptr(), O_RDONLY, 0);
    if fd < 0 { return 0; }
    let mut buf = [0u8; 4096];
    let mut total = 0usize;
    loop {
        if total >= buf.len() { break; }
        let n = read(fd, buf.as_mut_ptr().add(total), buf.len() - total);
        if n <= 0 { break; }
        total += n as usize;
    }
    close(fd);

    let data = &buf[..total];
    let mut count = 0usize;
    let mut start = 0usize;
    let mut i = 0usize;
    while i <= data.len() {
        if i == data.len() || data[i] == b'\n' {
            let line = &data[start..i];
            start = i + 1;
            if !line.is_empty() {
                let gid = field(line, 2).and_then(parse_u32);
                let members = field(line, 3);
                if let (Some(gid), Some(members)) = (gid, members) {
                    if count < out.len() && members.split(|&b| b == b',').any(|m| m == username) {
                        out[count] = gid;
                        count += 1;
                    }
                }
            }
        }
        i += 1;
    }
    count
}

/// Look `GREETER_USER` up in /etc/passwd. Matching by NAME rather than by uid is
/// deliberate: the uid is an implementation detail of the image and lives in one
/// place, the passwd file the mkfs script writes.
unsafe fn lookup_greeter() -> Option<Account> {
    let fd = open(PASSWD_PATH.as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; PASSWD_CAP];
    let mut total = 0usize;
    loop {
        if total >= PASSWD_CAP {
            break;
        }
        let n = read(fd, buf.as_mut_ptr().add(total), PASSWD_CAP - total);
        if n <= 0 {
            break;
        }
        total += n as usize;
    }
    close(fd);

    let data = &buf[..total];
    let mut start = 0usize;
    let mut i = 0usize;
    while i <= data.len() {
        if i == data.len() || data[i] == b'\n' {
            let line = &data[start..i];
            start = i + 1;
            i += 1;
            if line.is_empty() {
                continue;
            }
            if field(line, 0) != Some(GREETER_USER) {
                continue;
            }
            let uid = match field(line, 2).and_then(parse_u32) {
                Some(v) => v,
                None => continue,
            };
            let gid = match field(line, 3).and_then(parse_u32) {
                Some(v) => v,
                None => continue,
            };
            let home_field = match field(line, 5) {
                Some(h) if !h.is_empty() && h.len() < HOME_CAP => h,
                _ => continue,
            };
            let mut home = [0u8; HOME_CAP];
            home[..home_field.len()].copy_from_slice(home_field);
            return Some(Account {
                uid,
                gid,
                home,
                home_len: home_field.len(),
            });
        }
        i += 1;
    }
    None
}

/// Value of `KEY=` in `envp`, or None.
unsafe fn env_get<'a>(envp: *const *const u8, key: &[u8]) -> Option<&'a [u8]> {
    if envp.is_null() { return None; }
    let mut i = 0usize;
    loop {
        let e = *envp.add(i);
        if e.is_null() { return None; }
        let mut len = 0usize;
        while len < 4096 && *e.add(len) != 0 { len += 1; }
        let entry = core::slice::from_raw_parts(e, len);
        if entry.len() >= key.len() && &entry[..key.len()] == key {
            return Some(&entry[key.len()..]);
        }
        i += 1;
    }
}

/// chown `$dir_key/$name_key` (or just `$name_key` when `dir_key` is empty or
/// the name is absolute) to the greeter account. See the module comment.
unsafe fn chown_to_greeter(envp: *const *const u8, dir_key: &[u8], name_key: &[u8], acct: &Account) {
    let name = match env_get(envp, name_key) {
        Some(n) if !n.is_empty() => n,
        _ => { emit(b"greeter-launch: "); emit(name_key); emit(b" unset; socket not handed over\n"); return; }
    };
    let mut path = [0u8; 256];
    let mut n = 0usize;
    if name[0] != b'/' && !dir_key.is_empty() {
        let dir = match env_get(envp, dir_key) {
            Some(d) if !d.is_empty() => d,
            _ => { emit(b"greeter-launch: "); emit(dir_key); emit(b" unset; socket not handed over\n"); return; }
        };
        if dir.len() + 1 + name.len() >= path.len() { return; }
        path[..dir.len()].copy_from_slice(dir);
        n = dir.len();
        path[n] = b'/';
        n += 1;
    } else if name.len() >= path.len() {
        return;
    }
    path[n..n + name.len()].copy_from_slice(name);
    n += name.len();
    path[n] = 0;
    if chown(path.as_ptr(), acct.uid, acct.gid) != 0 {
        emit(b"greeter-launch: WARNING: chown of ");
        emit(&path[..n]);
        emit(b" to the greeter account failed\n");
    } else {
        emit(b"GREETER-LAUNCH: handed ");
        emit(&path[..n]);
        emit(b" to the greeter account\n");
    }
}

fn fail(msg: &[u8]) -> ! {
    emit(b"greeter-launch: ");
    emit(msg);
    emit(b"\n");
    exit(1);
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, envp: *const *const u8) -> i32 {
    let acct = match lookup_greeter() {
        Some(a) => a,
        None => fail(b"no cosmic-greeter account in /etc/passwd"),
    };
    if acct.uid == 0 {
        // A uid-0 greeter account is the bring-up scaffold this launcher exists
        // to replace, and it would make the drop below a silent no-op.
        fail(b"cosmic-greeter account has uid 0; it must be a real unprivileged account");
    }
    let home = &acct.home[..acct.home_len];

    // ---- the child's environment --------------------------------------------
    let mut arena = Arena::new();
    let mut env: [*const u8; MAX_ENV] = [core::ptr::null(); MAX_ENV];
    let mut nenv = 0usize;

    if !envp.is_null() {
        let mut i = 0usize;
        loop {
            let e = *envp.add(i);
            if e.is_null() {
                break;
            }
            // Length of the inherited entry, bounded so a corrupt envp cannot
            // walk off the end.
            let mut len = 0usize;
            while len < 4096 && *e.add(len) != 0 {
                len += 1;
            }
            let entry = core::slice::from_raw_parts(e, len);
            let mut replaced = false;
            for pfx in REPLACED {
                if entry.len() >= pfx.len() && &entry[..pfx.len()] == *pfx {
                    replaced = true;
                    break;
                }
            }
            if !replaced {
                if nenv + 16 >= MAX_ENV {
                    fail(b"inherited environment too large");
                }
                env[nenv] = e;
                nenv += 1;
            }
            i += 1;
        }
    }

    let added: [*const u8; 8] = [
        arena.push(&[b"HOME=", home]),
        arena.push(&[b"USER=", GREETER_USER]),
        arena.push(&[b"LOGNAME=", GREETER_USER]),
        arena.push(&[b"SHELL=/bin/false"]),
        arena.push(&[b"XDG_CONFIG_HOME=", home, b"/.config"]),
        arena.push(&[b"XDG_CACHE_HOME=", home, b"/.cache"]),
        arena.push(&[b"XDG_DATA_HOME=", home, b"/.local/share"]),
        arena.push(&[b"XDG_STATE_HOME=", home, b"/.local/state"]),
    ];
    for p in added {
        if p.is_null() {
            fail(b"environment arena exhausted");
        }
        env[nenv] = p;
        nenv += 1;
    }
    env[nenv] = core::ptr::null();

    // ---- argv ----------------------------------------------------------------
    // cosmic-comp hands its kiosk child the arguments that followed it on the
    // command line, and cosmic-greeter's own argument loop ignores what it does
    // not recognise, so they are passed straight through.
    let mut args: [*const u8; MAX_ARGV] = [core::ptr::null(); MAX_ARGV];
    args[0] = GREETER_BIN.as_ptr();
    let mut nargs = 1usize;
    if !argv.is_null() {
        let mut i = 1i32;
        while i < argc && nargs + 1 < MAX_ARGV {
            let a = *argv.add(i as usize);
            if a.is_null() {
                break;
            }
            args[nargs] = a;
            nargs += 1;
            i += 1;
        }
    }
    args[nargs] = core::ptr::null();

    // ---- hand the sockets to the greeter account -----------------------------
    // Still root here. Failure is reported, not fatal: the greeter then fails to
    // connect exactly as it would have without this step, and greetd's
    // supervision (and init's) applies — a hard exit here would turn a missing
    // variable into a respawn loop with no screen to show for it.
    chown_to_greeter(envp, b"XDG_RUNTIME_DIR=", b"WAYLAND_DISPLAY=", &acct);
    chown_to_greeter(envp, b"", b"GREETD_SOCK=", &acct);

    // ---- cross the boundary --------------------------------------------------
    // Supplementary groups first: setgroups needs root, which setresuid below
    // gives up, and the effective gid becoming non-root right after setresgid
    // would make it ambiguous which privilege level a failure here ran at.
    let mut groups = [0u32; MAX_GROUPS];
    let ngroups = lookup_groups(GREETER_USER, &mut groups);
    if setgroups(ngroups, groups.as_ptr()) != 0 {
        fail(b"setgroups failed");
    }
    // gid first: after the uid drop the process can no longer change its gid.
    if setresgid(acct.gid, acct.gid, acct.gid) != 0 {
        fail(b"setresgid failed");
    }
    if setresuid(acct.uid, acct.uid, acct.uid) != 0 {
        fail(b"setresuid failed");
    }
    // A launcher that reports a drop it did not perform is indistinguishable on
    // screen from one that did — the greeter would simply run as root and look
    // identical. Read the uid back rather than assuming the syscall meant it.
    let now = getuid();
    if now != acct.uid {
        fail(b"privilege drop did not take effect");
    }

    let mut home_c = [0u8; HOME_CAP + 1];
    home_c[..acct.home_len].copy_from_slice(home);
    if chdir(home_c.as_ptr()) != 0 {
        chdir(b"/\0".as_ptr());
    }

    emit(b"GREETER-LAUNCH: dropped to uid ");
    emit_u32(now);
    emit(b" gid ");
    emit_u32(acct.gid);
    emit(b", exec /bin/cosmic-greeter-login\n");

    execve(GREETER_BIN.as_ptr(), args.as_ptr(), env.as_ptr());
    fail(b"execve of /bin/cosmic-greeter-login failed");
}
