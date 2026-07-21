//! LeandrOS Login - authenticates a user and execs their shell.
//!
//! Spawned by init for each console session. Reads a username and password,
//! checks them against /etc/passwd and /etc/shadow, drops privileges to the
//! matched user via setresgid/setresuid, and execs their configured shell.

#![no_std]
#![no_main]

extern crate leandros_libc;

mod sha256;

use leandros_libc::{
    write, read, STDOUT_FILENO, STDIN_FILENO,
    open, close, O_RDONLY,
    execve, chdir, exit,
    setresuid, setresgid,
    ioctl,
};

const TCGETS:  usize = 0x5401;
const TCSETSF: usize = 0x5404;
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
        let user_len = read_line(&mut user_buf, true);
        write_str("\n");

        write_str("Password: ");
        let mut pass_buf = [0u8; 128];
        let pass_len = read_password(&mut pass_buf);
        write_str("\n");

        let username = match core::str::from_utf8(&user_buf[..user_len]) {
            Ok(s) => s,
            Err(_) => { write_str("\nLogin incorrect\n\n"); continue; }
        };

        match authenticate(username, &pass_buf[..pass_len]) {
            Some(rec) => do_login(&rec),
            None => write_str("\nLogin incorrect\n\n"),
        }
    }
}

/// Read a line from fd 0, optionally echoing typed characters. Handles
/// backspace/DEL. Stops at '\n'/'\r' (not included in the returned length).
unsafe fn read_line(buf: &mut [u8], echo: bool) -> usize {
    let mut len = 0;
    loop {
        let mut ch = [0u8; 1];
        let n = read(STDIN_FILENO, ch.as_mut_ptr(), 1);
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
        Some(rec)
    } else {
        None
    }
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

/// Shadow hash format: `$sha256$<salt>$<hex>`. An empty field means
/// passwordless login. `hex` is the lowercase SHA-256 hexdigest of
/// `salt bytes ++ password bytes`.
fn verify_password(hash_field: &str, password: &[u8]) -> bool {
    if hash_field.is_empty() {
        return true;
    }
    let rest = match hash_field.strip_prefix("$sha256$") {
        Some(r) => r,
        None => return false,
    };
    let mut parts = rest.splitn(2, '$');
    let salt = parts.next().unwrap_or("");
    let hex = parts.next().unwrap_or("");
    if hex.is_empty() {
        return false;
    }

    let salt_bytes = salt.as_bytes();
    let mut msg = [0u8; 256];
    if salt_bytes.len() + password.len() > msg.len() {
        return false;
    }
    let mut n = 0;
    msg[..salt_bytes.len()].copy_from_slice(salt_bytes);
    n += salt_bytes.len();
    msg[n..n + password.len()].copy_from_slice(password);
    n += password.len();

    let digest = sha256::sha256(&msg[..n]);
    let mut hexbuf = [0u8; 64];
    sha256::to_hex(&digest, &mut hexbuf);

    ct_eq(&hexbuf, hex.as_bytes())
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Drop privileges to the matched user and exec their shell. Never returns.
unsafe fn do_login(rec: &UserRec) -> ! {
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

    static PATH_ENV: &[u8] = b"PATH=/bin\0";
    static TERM_ENV: &[u8] = b"TERM=xterm-256color\0";

    let envp: [*const u8; 7] = [
        home_env.as_ptr(),
        shell_env.as_ptr(),
        user_env.as_ptr(),
        logname_env.as_ptr(),
        PATH_ENV.as_ptr(),
        TERM_ENV.as_ptr(),
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
