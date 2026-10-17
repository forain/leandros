//! sudotest -- su and sudo (sudo-rs, ports/sudo-rs) end to end.
//!
//! Run as root. Every case spawns the real set-user-ID /bin/su or /bin/sudo,
//! either as root or -- through a fork that calls initgroups/setgid/setuid
//! first, as a login would -- as `leandro` (uid 1000, in wheel) or
//! `cosmic-greeter` (uid 990, not in wheel). Passwords go in on stdin
//! (`sudo -S`; su reads stdin when it has no terminal), so nothing here needs
//! a tty. Output: one `PASS`/`FAIL` line per check, then
//! `SUDOTEST pass=N fail=M`; exit status 0 iff every check passed.
//!
//! The image provisions root/root and leandro/leandro (/etc/shadow), wheel
//! (gid 10) = {leandro}, and /etc/sudoers with `root ALL` and `%wheel ALL`.

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SU: &str = "/bin/su";
const SUDO: &str = "/bin/sudo";

#[derive(Clone, Copy)]
enum As {
    Root,
    User(&'static str, u32, u32),
}
const LEANDRO: As = As::User("leandro", 1000, 1000);
const GREETER: As = As::User("cosmic-greeter", 990, 990);

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Run `argv` as `who`, with a fixed baseline environment plus `extra_env`,
/// `input` written to its stdin, stdout/stderr captured. Killed after 60 s.
fn run(who: As, argv: &[&str], extra_env: &[(&str, &str)], input: &str) -> Out {
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "vt100")
        .env("FOO_SECRET", "topsecret")
        .env("LD_LIBRARY_PATH", "/nonexistent")
        .env("WAYLAND_DISPLAY", "wayland-test")
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match who {
        As::Root => {
            cmd.env("HOME", "/root").env("USER", "root").env("XDG_RUNTIME_DIR", "/run/user/0");
        }
        As::User(name, uid, gid) => {
            cmd.env("HOME", format!("/home/{name}"))
                .env("USER", name)
                .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
            let cname = CString::new(name).unwrap();
            // SAFETY: runs in the forked child before exec; only async-signal
            // tolerable libc calls on data prepared before the fork.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::initgroups(cname.as_ptr(), gid) != 0
                        || libc::setgid(gid) != 0
                        || libc::setuid(uid) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Out { code: -1, stdout: String::new(), stderr: format!("spawn: {e}") },
    };
    {
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(input.as_bytes());
    }
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() > Duration::from_secs(60) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    let _ = child.stdout.take().unwrap().read_to_string(&mut stdout);
    let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
    let code = match status {
        Some(s) => s.code().unwrap_or(128 + std::os::unix::process::ExitStatusExt::signal(&s).unwrap_or(0)),
        None => {
            stderr.push_str("[sudotest: timed out after 60 s]");
            124
        }
    };
    Out { code, stdout, stderr }
}

struct T {
    pass: u32,
    fail: u32,
}

impl T {
    fn check(&mut self, name: &str, ok: bool, out: &Out) {
        if ok {
            self.pass += 1;
            println!("PASS {name}");
        } else {
            self.fail += 1;
            println!("FAIL {name}: rc={} out={:?}", out.code, out.all());
        }
    }
}

/// `NAME=value` line of an `env` dump, if present.
fn env_var<'a>(dump: &'a str, name: &str) -> Option<&'a str> {
    dump.lines().find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix('=')))
}

fn has_groups(id_out: &str, gids: &[u32]) -> bool {
    let groups = match id_out.split("groups=").nth(1) {
        Some(g) => g,
        None => return false,
    };
    gids.iter().all(|g| groups.split(',').any(|e| e.trim().starts_with(&format!("{g}("))))
}

/// Field 22 (`starttime`) of /proc/<pid>/stat.
fn proc_starttime(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 2..];
    // after_comm starts at field 3 (state); starttime is field 22.
    after_comm.split_whitespace().nth(22 - 3)?.parse().ok()
}

fn main() {
    if unsafe { libc::getuid() } != 0 {
        eprintln!("sudotest: must run as root");
        std::process::exit(2);
    }
    let mut t = T { pass: 0, fail: 0 };
    let leandro_gids = [1000, 10, 29, 44, 104, 108];

    // ── /proc/<pid>/stat starttime: half of sudo's credential-cache key ──
    // It must be the process's creation time, not "now": two reads of the
    // same process a second apart must agree.
    let st1 = proc_starttime(1);
    std::thread::sleep(Duration::from_millis(1100));
    let st2 = proc_starttime(1);
    let o = Out { code: 0, stdout: format!("{st1:?} {st2:?}"), stderr: String::new() };
    t.check("/proc/1/stat starttime is stable (process creation time)", st1.is_some() && st1 == st2, &o);

    // ── su as root: no password, real identity and groups ────────────────
    let o = run(As::Root, &[SU, "leandro", "-c", "id"], &[], "");
    t.check(
        "su leandro -c id (root, no password): uid 1000 + supplementary groups",
        o.code == 0 && o.stdout.contains("uid=1000(leandro)") && o.stdout.contains("gid=1000(leandro)")
            && has_groups(&o.stdout, &leandro_gids),
        &o,
    );

    let o = run(As::Root, &[SU, "-c", "id"], &[], "");
    t.check("su -c id (no user = root)", o.code == 0 && o.stdout.contains("uid=0(root)"), &o);

    let o = run(As::Root, &[SU, "-", "leandro", "-c", "pwd; env"], &[], "");
    let pwd = o.stdout.lines().next().unwrap_or("");
    t.check(
        "su - leandro: login env (cwd, HOME, SHELL, USER, LOGNAME, PATH, env cleared)",
        o.code == 0
            && pwd == "/home/leandro"
            && env_var(&o.stdout, "HOME") == Some("/home/leandro")
            && env_var(&o.stdout, "SHELL") == Some("/bin/brush")
            && env_var(&o.stdout, "USER") == Some("leandro")
            && env_var(&o.stdout, "LOGNAME") == Some("leandro")
            && env_var(&o.stdout, "PATH").is_some()
            && env_var(&o.stdout, "FOO_SECRET").is_none()
            && !o.stderr.contains("profile:"),
        &o,
    );

    // root's home is 0700: a command su starts as leandro from there must
    // still run (the shell must not chdir into the directory it inherited).
    let o = run(As::Root, &["/bin/sh", "-c", "cd /root && su leandro -c '/bin/true && echo ran'"], &[], "");
    t.check("su leandro -c from root's 0700 home", o.code == 0 && o.stdout.trim() == "ran", &o);

    let o = run(As::Root, &[SU, "-l", "leandro", "-c", "env"], &[], "");
    t.check(
        "su -l leandro: XDG_RUNTIME_DIR is the target's",
        o.code == 0 && env_var(&o.stdout, "XDG_RUNTIME_DIR") == Some("/run/user/1000"),
        &o,
    );

    let o = run(As::Root, &[SU, "leandro", "-c", "env"], &[], "");
    t.check(
        "su leandro (no -l): env kept, HOME/USER/XDG_RUNTIME_DIR switched",
        o.code == 0
            && env_var(&o.stdout, "FOO_SECRET") == Some("topsecret")
            && env_var(&o.stdout, "HOME") == Some("/home/leandro")
            && env_var(&o.stdout, "USER") == Some("leandro")
            && env_var(&o.stdout, "XDG_RUNTIME_DIR") == Some("/run/user/1000"),
        &o,
    );

    let o = run(As::Root, &[SU, "leandro", "-c", "exit 7"], &[], "");
    t.check("su -c propagates the exit status", o.code == 7, &o);

    // ── su as a user: the TARGET's password ──────────────────────────────
    let o = run(LEANDRO, &[SU, "-c", "id", "root"], &[], "root\n");
    t.check("leandro: su root with root's password", o.code == 0 && o.stdout.contains("uid=0(root)"), &o);

    let o = run(LEANDRO, &[SU, "-c", "id", "root"], &[], "wrong\n");
    t.check("leandro: su root with a wrong password fails", o.code != 0 && !o.stdout.contains("uid=0"), &o);

    let o = run(GREETER, &[SU, "-c", "id", "leandro"], &[], "leandro\n");
    t.check(
        "cosmic-greeter: su leandro with leandro's password",
        o.code == 0 && o.stdout.contains("uid=1000(leandro)"),
        &o,
    );

    // ── sudo as root: no password ────────────────────────────────────────
    let o = run(As::Root, &[SUDO, "-n", "id"], &[], "");
    t.check("root: sudo -n id", o.code == 0 && o.stdout.contains("uid=0(root)"), &o);

    let o = run(As::Root, &[SUDO, "-n", "-u", "leandro", "id"], &[], "");
    t.check(
        "root: sudo -u leandro id: uid 1000 + groups",
        o.code == 0 && o.stdout.contains("uid=1000(leandro)") && has_groups(&o.stdout, &leandro_gids),
        &o,
    );

    let o = run(As::Root, &[SUDO, "-n", "-u", "leandro", "env"], &[], "");
    t.check(
        "root: sudo -u leandro env: env_reset (secrets/LD_* gone, WAYLAND_DISPLAY kept, target HOME/XDG_RUNTIME_DIR, secure_path)",
        o.code == 0
            && env_var(&o.stdout, "FOO_SECRET").is_none()
            && env_var(&o.stdout, "LD_LIBRARY_PATH").is_none()
            && env_var(&o.stdout, "WAYLAND_DISPLAY") == Some("wayland-test")
            && env_var(&o.stdout, "HOME") == Some("/home/leandro")
            && env_var(&o.stdout, "USER") == Some("leandro")
            && env_var(&o.stdout, "SUDO_USER") == Some("root")
            && env_var(&o.stdout, "XDG_RUNTIME_DIR") == Some("/run/user/1000")
            && env_var(&o.stdout, "PATH")
                == Some("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
        &o,
    );

    let o = run(As::Root, &[SUDO, "-n", "-E", "-u", "leandro", "env"], &[], "");
    t.check(
        "root: sudo -E keeps the caller's environment (PATH stays secure_path)",
        o.code == 0
            && env_var(&o.stdout, "FOO_SECRET") == Some("topsecret")
            && env_var(&o.stdout, "PATH")
                == Some("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
        &o,
    );

    let o = run(As::Root, &[SUDO, "-n", "-i", "-u", "leandro", "pwd"], &[], "");
    t.check("root: sudo -i -u leandro starts in the target's home", o.code == 0 && o.stdout.trim() == "/home/leandro", &o);

    let o = run(As::Root, &[SUDO, "-n", "-i", "-u", "leandro", "env"], &[], "");
    t.check(
        "root: sudo -i -u leandro keeps WAYLAND_DISPLAY, gets the target's XDG_RUNTIME_DIR",
        o.code == 0
            && env_var(&o.stdout, "WAYLAND_DISPLAY") == Some("wayland-test")
            && env_var(&o.stdout, "XDG_RUNTIME_DIR") == Some("/run/user/1000")
            && !o.stderr.contains("profile:"),
        &o,
    );

    let o = run(As::Root, &[SUDO, "-n", "-s", "-u", "leandro", "echo", "shell-ok"], &[], "");
    t.check("root: sudo -s runs through the shell", o.code == 0 && o.stdout.trim() == "shell-ok", &o);

    let o = run(As::Root, &[SUDO, "-n", "-u", "leandro", "sh", "-c", "exit 9"], &[], "");
    t.check("root: sudo propagates the exit status", o.code == 9, &o);

    // ── sudo as leandro (wheel): password, cache, -n ─────────────────────
    let o = run(LEANDRO, &[SUDO, "-K"], &[], "");
    t.check("leandro: sudo -K (drop cached credentials)", o.code == 0, &o);

    let o = run(LEANDRO, &[SUDO, "-n", "id"], &[], "");
    t.check(
        "leandro: sudo -n id without cached credentials fails",
        o.code != 0 && !o.stdout.contains("uid=0"),
        &o,
    );

    let o = run(LEANDRO, &[SUDO, "-S", "id"], &[], "wrong\n");
    t.check("leandro: sudo -S with a wrong password fails", o.code != 0 && !o.stdout.contains("uid=0"), &o);

    let o = run(LEANDRO, &[SUDO, "-S", "id"], &[], "leandro\n");
    t.check("leandro: sudo -S with the right password", o.code == 0 && o.stdout.contains("uid=0(root)"), &o);

    let o = run(LEANDRO, &[SUDO, "-n", "id"], &[], "");
    t.check(
        "leandro: sudo -n id succeeds from the cached credential",
        o.code == 0 && o.stdout.contains("uid=0(root)"),
        &o,
    );

    let o = run(LEANDRO, &[SUDO, "-n", "-l"], &[], "");
    t.check("leandro: sudo -l lists (ALL : ALL) ALL", o.code == 0 && o.all().contains("(ALL : ALL) ALL"), &o);

    let o = run(LEANDRO, &[SUDO, "-K"], &[], "");
    t.check("leandro: sudo -K again", o.code == 0, &o);
    let o = run(LEANDRO, &[SUDO, "-n", "id"], &[], "");
    t.check("leandro: sudo -n id fails again after -K", o.code != 0 && !o.stdout.contains("uid=0"), &o);

    // ── sudo as a non-wheel user: denied, whatever the password ──────────
    let o = run(GREETER, &[SUDO, "-S", "id"], &[], "leandro\n");
    t.check("cosmic-greeter (not in wheel): sudo denied", o.code != 0 && !o.stdout.contains("uid=0"), &o);

    // ── the binaries themselves ──────────────────────────────────────────
    let o = Out { code: 0, stdout: String::new(), stderr: String::new() };
    for (path, mode) in [(SU, 0o104755u32), (SUDO, 0o104755), ("/etc/sudoers", 0o100440)] {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let c = CString::new(path).unwrap();
        let ok = unsafe { libc::stat(c.as_ptr(), &mut st) } == 0
            && st.st_uid == 0
            && st.st_mode == mode;
        t.check(&format!("{path} is root-owned, mode {mode:o} (is {:o})", st.st_mode), ok, &o);
    }

    println!("SUDOTEST pass={} fail={}", t.pass, t.fail);
    std::process::exit(if t.fail == 0 { 0 } else { 1 });
}
