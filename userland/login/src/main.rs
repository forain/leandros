//! LeandrOS Login - authenticates a user and execs their shell.
//!
//! Spawned by init for each console session. Reads a username and password,
//! checks them against /etc/passwd and /etc/shadow, drops privileges to the
//! matched user via setresgid/setresuid, and execs their configured shell.

#![no_std]
#![no_main]

extern crate leandros_libc;

#[path = "../../../lib/shadowhash/shadowhash.rs"]
mod shadowhash;

#[path = "../../../lib/utmpx/utmpx.rs"]
mod utmpx;

use leandros_libc::{
    write, read, STDOUT_FILENO, STDIN_FILENO,
    open, close, O_RDONLY,
    execve, chdir, exit,
    setresuid, setresgid, setgroups,
    ioctl,
};

/// O_EXCL (Linux value, both arches).
const O_EXCL_: i32 = 0o200;

const TCGETS:  usize = 0x5401;
const TCSETSF: usize = 0x5404;
const TIOCGPTN: usize = 0x8004_5430;
const ECHO:    u32   = 0x0008;

const MAX_ATTEMPTS: u32 = 3;

struct UserRec {
    name: [u8; 64],
    name_len: usize,
    uid: u32,
    gid: u32,
    home:  [u8; 128],
    home_len: usize,
    shell: [u8; 128],
    shell_len: usize,
}

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    write_str("\nLeandrOS\n\n");

    let mut attempts = 0u32;
    loop {
        if attempts >= MAX_ATTEMPTS {
            exit(1);
        }
        attempts += 1;

        write_str("login: ");
        let mut user_buf = [0u8; 64];
        // The serial console's line discipline does not echo, so login echoes
        // the username itself there. A pty — which is what a VT 2..6 text
        // session is — runs the full termios discipline and has already
        // echoed it; echoing again printed every username twice.
        let mut ptn = 0u32;
        let tty_echoes = ioctl(STDIN_FILENO as i32, TIOCGPTN, &mut ptn as *mut u32 as usize) == 0;
        let user_len = read_line(&mut user_buf, !tty_echoes);
        write_str("\n");

        write_str("Password: ");
        let mut pass_buf = [0u8; 128];
        let pass_len = read_password(&mut pass_buf);
        write_str("\n");

        let username = match core::str::from_utf8(&user_buf[..user_len]) {
            Ok(s) => s,
            Err(_) => { fail_delay(); write_str("\nLogin incorrect\n\n"); continue; }
        };

        match authenticate(username, &pass_buf[..pass_len]) {
            Some(rec) => do_login(&rec),
            None => { fail_delay(); write_str("\nLogin incorrect\n\n") }
        }
    }
}

/// Stall after a failed attempt (rate limit), matching pam-leandros' default
/// pam_authenticate failure delay of 1 s (Linux login uses ~3 s FAIL_DELAY).
unsafe fn fail_delay() {
    leandros_libc::usleep(1_000_000);
}

/// Read a line from fd 0, optionally echoing typed characters. Handles
/// backspace/DEL. Stops at '\n'/'\r' (not included in the returned length).
unsafe fn read_line(buf: &mut [u8], echo: bool) -> usize {
    let mut len = 0;
    loop {
        let mut ch = [0u8; 1];
        let n = read(STDIN_FILENO, ch.as_mut_ptr(), 1);
        if n == 0 {
            // EOF on a scripted (pipe/socket) stdin: no one can answer, quit
            // instead of spinning on the prompt. A terminal never reports it.
            let mut line = [0u8; 32];
            let l = utmpx::tty_line(0, &mut line);
            if l == 0 || line.starts_with(b"pipe:") || line.starts_with(b"socket:") {
                exit(1);
            }
        }
        if n <= 0 {
            continue;
        }
        let c = ch[0];

        if c == b'\n' || c == b'\r' {
            break;
        }

        if c == 0x08 || c == 0x7F {
            if len > 0 {
                len -= 1;
                if echo {
                    write_str("\x08 \x08");
                }
            }
            continue;
        }

        if c >= 32 && c <= 126 && len < buf.len() {
            buf[len] = c;
            len += 1;
            if echo {
                write(STDOUT_FILENO, &c, 1);
            }
        }
    }
    len
}

/// Read a line with terminal echo disabled for the duration of the read.
unsafe fn read_password(buf: &mut [u8]) -> usize {
    let mut tbuf = [0u8; 36];
    let got_termios = ioctl(0, TCGETS, tbuf.as_mut_ptr() as usize) == 0;

    let orig_lflag = u32::from_ne_bytes([tbuf[12], tbuf[13], tbuf[14], tbuf[15]]);
    if got_termios {
        let new_lflag = orig_lflag & !ECHO;
        tbuf[12..16].copy_from_slice(&new_lflag.to_ne_bytes());
        ioctl(0, TCSETSF, tbuf.as_ptr() as usize);
    }

    let len = read_line(buf, false);

    if got_termios {
        tbuf[12..16].copy_from_slice(&orig_lflag.to_ne_bytes());
        ioctl(0, TCSETSF, tbuf.as_ptr() as usize);
    }

    len
}

/// Look up `username` in /etc/passwd and /etc/shadow and verify `password`.
/// Unknown user and bad password are indistinguishable to the caller.
unsafe fn authenticate(username: &str, password: &[u8]) -> Option<UserRec> {
    let rec = lookup_passwd(username)?;
    let (hash_buf, hash_len) = lookup_shadow(username)?;
    let hash_str = core::str::from_utf8(&hash_buf[..hash_len]).ok()?;
    if verify_password(hash_str, password) {
        if hash_str.starts_with("$sha256$") {
            rehash_legacy(username, password);
        }
        Some(rec)
    } else {
        None
    }
}

/// A legacy `$sha256$` hash that just verified is replaced by a fresh-salted
/// `$6$` one (PAM cannot verify `$sha256$`). Only the hash field changes; the
/// new file is written beside /etc/shadow, fsynced, given the old owner and
/// mode, and renamed over it. Any failure leaves /etc/shadow as it was.
unsafe fn rehash_legacy(username: &str, password: &[u8]) {
    use leandros_libc::O_CREAT;
    let mut rnd = [0u8; 16];
    let r = open(b"/dev/urandom\0".as_ptr(), O_RDONLY, 0);
    if r < 0 {
        return;
    }
    let n = read(r, rnd.as_mut_ptr(), 16);
    close(r);
    if n != 16 {
        return;
    }
    let mut field = [0u8; 128];
    let Some(flen) = shadowhash::upgrade_field(password, &rnd, &mut field) else { return };

    // Same lock as shadowfix and sshd (lckpwdf's /etc/.pwd.lock). The shadow
    // file is read only after it is held; the lock fd is closed on return.
    // O_CLOEXEC|O_NOFOLLOW; created only if missing (a read-only /etc fails here).
    // LOCK_NB retried ~2 s so a stuck holder cannot delay the login.
    const O_CLOEXEC_: i32 = 0o2000000;
    #[cfg(target_arch = "x86_64")]
    const O_NOFOLLOW_: i32 = 0o400000;
    #[cfg(not(target_arch = "x86_64"))]
    const O_NOFOLLOW_: i32 = 0o100000;
    let mut lk = open(b"/etc/.pwd.lock\0".as_ptr(), 1 | O_CLOEXEC_ | O_NOFOLLOW_, 0o600);
    if lk < 0 {
        lk = open(b"/etc/.pwd.lock\0".as_ptr(), 1 | O_CREAT | O_CLOEXEC_ | O_NOFOLLOW_, 0o600);
    }
    if lk < 0 {
        return;
    }
    let mut got = false;
    for _ in 0..40 {
        if leandros_libc::flock(lk, leandros_libc::LOCK_EX | 4) == 0 {
            got = true;
            break;
        }
        leandros_libc::usleep(50_000);
    }
    if !got {
        close(lk);
        return;
    }
    rehash_locked(username, &field[..flen]);
    close(lk);
}

unsafe fn rehash_locked(username: &str, field: &[u8]) {
    use leandros_libc::syscall::{nr, syscall1, syscall2};
    use leandros_libc::{fchmod, fchown, rename, unlink, O_CREAT};
    let flen = field.len();
    let mut src = [0u8; 8192];
    let fd = open(b"/etc/shadow\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        return;
    }
    // st_mode, st_uid, st_gid of the old file (kept on the new one).
    let mut st = [0u8; 144];
    let ok = syscall2(nr::FSTAT, fd as usize, st.as_mut_ptr() as usize) == 0;
    let mut total = 0usize;
    loop {
        let m = read(fd, src.as_mut_ptr().add(total), src.len() - total);
        if m <= 0 {
            break;
        }
        total += m as usize;
        if total == src.len() {
            break;
        }
    }
    close(fd);
    // Too big to rewrite whole: leave it (the file would be truncated otherwise).
    if !ok || total == src.len() {
        return;
    }
    #[cfg(target_arch = "x86_64")]
    let (mo, uo, go) = (24usize, 28usize, 32usize);
    #[cfg(not(target_arch = "x86_64"))]
    let (mo, uo, go) = (16usize, 24usize, 28usize);
    let rd = |o: usize| u32::from_ne_bytes([st[o], st[o + 1], st[o + 2], st[o + 3]]);
    let (mode, uid, gid) = (rd(mo) & 0o7777, rd(uo), rd(go));

    let mut out = [0u8; 8192];
    let Some(olen) = shadowhash::replace_legacy_field(&src[..total], username.as_bytes(), &field[..flen], &mut out) else {
        return;
    };
    // /etc/shadow.login-new.<pid>: unique per process, O_EXCL below.
    let mut tmpb = *b"/etc/shadow.login-new.\0\0\0\0\0\0\0\0\0\0\0\0";
    let mut pid = leandros_libc::getpid() as u32;
    let mut at = 22usize;
    let mut digits = [0u8; 10];
    let mut nd = 0;
    loop {
        digits[nd] = b'0' + (pid % 10) as u8;
        nd += 1;
        pid /= 10;
        if pid == 0 {
            break;
        }
    }
    while nd > 0 {
        nd -= 1;
        tmpb[at] = digits[nd];
        at += 1;
    }
    let tmp = &tmpb;
    unlink(tmp.as_ptr()); // stale same-pid temp (pid reuse); unlink never follows a link
    let w = open(tmp.as_ptr(), 1 | O_CREAT | O_EXCL_, 0o600);
    if w < 0 {
        return;
    }
    let mut done = 0usize;
    let mut good = true;
    while done < olen {
        let m = write(w, out.as_ptr().add(done), olen - done);
        if m <= 0 {
            good = false;
            break;
        }
        done += m as usize;
    }
    #[cfg(target_arch = "x86_64")]
    const FSYNC: usize = 74;
    #[cfg(not(target_arch = "x86_64"))]
    const FSYNC: usize = 82;
    good = good && fchown(w, uid, gid) == 0 && fchmod(w, mode) == 0 && syscall1(FSYNC, w as usize) == 0;
    close(w);
    if !good || rename(tmp.as_ptr(), b"/etc/shadow\0".as_ptr()) != 0 {
        unlink(tmp.as_ptr());
        return;
    }
    write_str("(password hash upgraded to sha512-crypt)\n");
}

unsafe fn lookup_passwd(username: &str) -> Option<UserRec> {
    let fd = open(b"/etc/passwd\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 4096];
    let n = read(fd, buf.as_mut_ptr(), buf.len());
    close(fd);
    if n <= 0 {
        return None;
    }
    let content = core::str::from_utf8(&buf[..n as usize]).ok()?;

    for line in content.lines() {
        let mut fields = line.splitn(7, ':');
        let name = fields.next().unwrap_or("");
        if name != username {
            continue;
        }
        let _passwd = fields.next().unwrap_or("");
        let uid = parse_u32(fields.next().unwrap_or(""))?;
        let gid = parse_u32(fields.next().unwrap_or(""))?;
        let _gecos = fields.next().unwrap_or("");
        let home = fields.next().unwrap_or("/");
        let shell = fields.next().unwrap_or("/bin/brush");

        let mut rec = UserRec {
            name: [0u8; 64], name_len: 0,
            uid, gid,
            home: [0u8; 128], home_len: 0,
            shell: [0u8; 128], shell_len: 0,
        };
        let nlen = username.len().min(rec.name.len());
        rec.name[..nlen].copy_from_slice(&username.as_bytes()[..nlen]);
        rec.name_len = nlen;

        let hlen = home.len().min(rec.home.len());
        rec.home[..hlen].copy_from_slice(&home.as_bytes()[..hlen]);
        rec.home_len = hlen;

        let slen = shell.len().min(rec.shell.len());
        rec.shell[..slen].copy_from_slice(&shell.as_bytes()[..slen]);
        rec.shell_len = slen;

        return Some(rec);
    }
    None
}

unsafe fn lookup_shadow(username: &str) -> Option<([u8; 128], usize)> {
    let fd = open(b"/etc/shadow\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 4096];
    let n = read(fd, buf.as_mut_ptr(), buf.len());
    close(fd);
    if n <= 0 {
        return None;
    }
    let content = core::str::from_utf8(&buf[..n as usize]).ok()?;

    for line in content.lines() {
        let mut fields = line.splitn(3, ':');
        let name = fields.next().unwrap_or("");
        if name != username {
            continue;
        }
        let hash = fields.next().unwrap_or("");
        let mut out = [0u8; 128];
        let hlen = hash.len().min(out.len());
        out[..hlen].copy_from_slice(&hash.as_bytes()[..hlen]);
        return Some((out, hlen));
    }
    None
}

fn parse_u32(s: &str) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    for c in s.bytes() {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u32)?;
    }
    Some(v)
}

/// Shadow hash check: sha512-crypt `$6$...` or legacy `$sha256$...`; an empty
/// field means passwordless login. See lib/shadowhash/shadowhash.rs.
fn verify_password(hash_field: &str, password: &[u8]) -> bool {
    shadowhash::verify(hash_field, password)
}

/// Supplementary groups of `username` from /etc/group: every group whose
/// member list names the user, plus every group whose gid is the user's
/// primary gid is left to setresgid. `initgroups(3)` without the libc.
unsafe fn lookup_groups(username: &str, out: &mut [u32; 32]) -> usize {
    let fd = open(b"/etc/group\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        return 0;
    }
    let mut buf = [0u8; 4096];
    let n = read(fd, buf.as_mut_ptr(), buf.len());
    close(fd);
    if n <= 0 {
        return 0;
    }
    let content = match core::str::from_utf8(&buf[..n as usize]) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let mut count = 0usize;
    for line in content.lines() {
        // name:passwd:gid:member,member,...
        let mut fields = line.splitn(4, ':');
        let _name = fields.next().unwrap_or("");
        let _pw = fields.next().unwrap_or("");
        let gid = match parse_u32(fields.next().unwrap_or("")) {
            Some(g) => g,
            None => continue,
        };
        let members = fields.next().unwrap_or("");
        if members.split(',').any(|m| m == username) && count < out.len() {
            out[count] = gid;
            count += 1;
        }
    }
    count
}

/// Drop privileges to the matched user and exec their shell. Never returns.
unsafe fn do_login(rec: &UserRec) -> ! {
    // utmp/wtmp/lastlog while still root; the shell keeps this pid, so init
    // closes the record when it reaps the session.
    {
        let mut line = [0u8; 32];
        let n = utmpx::tty_line(0, &mut line);
        // A pipe/socket stdin (a scripted login) is no terminal: nothing would
        // ever close that record, so it would stay in `who` forever.
        let l = &line[..n];
        if n > 0 && !l.starts_with(b"pipe:") && !l.starts_with(b"socket:") && !l.starts_with(b"anon_inode:") {
            utmpx::login(utmpx::getpid(), l, &rec.name[..rec.name_len], b"", rec.uid);
        }
    }
    leandros_libc::cgsession::join(rec.uid, rec.gid);
    // Supplementary groups first: setgroups needs root, which setresuid
    // below gives up.
    let name = core::str::from_utf8(&rec.name[..rec.name_len]).unwrap_or("");
    let mut groups = [0u32; 32];
    let ngroups = lookup_groups(name, &mut groups);
    if setgroups(ngroups, groups.as_ptr()) != 0 {
        write_str("login: setgroups failed\n");
        exit(1);
    }
    if setresgid(rec.gid, rec.gid, rec.gid) != 0 {
        write_str("login: setresgid failed\n");
        exit(1);
    }
    if setresuid(rec.uid, rec.uid, rec.uid) != 0 {
        write_str("login: setresuid failed\n");
        exit(1);
    }

    let mut home_c = [0u8; 129];
    let hlen = rec.home_len.min(128);
    home_c[..hlen].copy_from_slice(&rec.home[..hlen]);
    home_c[hlen] = 0;
    if chdir(home_c.as_ptr()) != 0 {
        chdir(b"/\0".as_ptr());
    }

    let mut shell_c = [0u8; 129];
    let slen = rec.shell_len.min(128);
    shell_c[..slen].copy_from_slice(&rec.shell[..slen]);
    shell_c[slen] = 0;

    let mut home_env = [0u8; 160];
    build_env(&mut home_env, "HOME", &rec.home[..hlen]);

    let mut shell_env = [0u8; 160];
    build_env(&mut shell_env, "SHELL", &shell_c[..slen]);

    let mut user_env = [0u8; 96];
    build_env(&mut user_env, "USER", &rec.name[..rec.name_len]);

    let mut logname_env = [0u8; 96];
    build_env(&mut logname_env, "LOGNAME", &rec.name[..rec.name_len]);

    // XDG_RUNTIME_DIR, as pam_systemd sets it for every login, console
    // included: /run/user/<uid>, which init seeds at boot for every account.
    // The user's per-user services live there (PipeWire's pipewire-0, the
    // session bus, wayland-N), so a console login of a user whose desktop is
    // running reaches the same PipeWire. Without it a program started on a
    // text VT found no PipeWire, and the audio device busy (held by that
    // PipeWire's sink): silence.
    let mut xdg_env = [0u8; 64];
    {
        let mut num = [0u8; 10];
        let mut v = rec.uid;
        let mut i = num.len();
        loop {
            i -= 1;
            num[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 || i == 0 { break; }
        }
        let prefix = b"/run/user/";
        let n = num.len() - i;
        let mut val = [0u8; 20];
        val[..prefix.len()].copy_from_slice(prefix);
        val[prefix.len()..prefix.len() + n].copy_from_slice(&num[i..]);
        build_env(&mut xdg_env, "XDG_RUNTIME_DIR", &val[..prefix.len() + n]);
    }

    static PATH_ENV: &[u8] = b"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\0";
    static TERM_ENV: &[u8] = b"TERM=xterm-256color\0";

    let envp: [*const u8; 8] = [
        home_env.as_ptr(),
        shell_env.as_ptr(),
        user_env.as_ptr(),
        logname_env.as_ptr(),
        PATH_ENV.as_ptr(),
        TERM_ENV.as_ptr(),
        xdg_env.as_ptr(),
        core::ptr::null(),
    ];

    let argv: [*const u8; 2] = [shell_c.as_ptr(), core::ptr::null()];
    execve(shell_c.as_ptr(), argv.as_ptr(), envp.as_ptr());

    write_str("login: exec failed\n");
    exit(1);
}

fn build_env(buf: &mut [u8], key: &str, value: &[u8]) -> usize {
    let mut n = 0;
    for b in key.bytes() {
        buf[n] = b;
        n += 1;
    }
    buf[n] = b'=';
    n += 1;
    let vlen = value.len().min(buf.len() - n - 1);
    buf[n..n + vlen].copy_from_slice(&value[..vlen]);
    n += vlen;
    buf[n] = 0;
    n
}

unsafe fn write_str(s: &str) {
    write(STDOUT_FILENO, s.as_ptr(), s.len());
}

