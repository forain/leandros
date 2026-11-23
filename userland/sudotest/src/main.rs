//! sudotest -- su and sudo (sudo-rs, ports/sudo-rs) end to end.
//!
//! Run as root. Every case spawns the real set-user-ID /bin/su or /bin/sudo,
//! either as root or -- through a fork that calls initgroups/setgid/setuid
//! first, as a login would -- as `leandro` (uid 1000, in wheel) or
//! `cosmic-greeter` (uid 990, not in wheel). Passwords go in on stdin
//! (`sudo -S`; su reads stdin when it has no terminal), so nothing here needs
//! a tty: the checks run in a forked child that starts a session of its own
//! with no controlling terminal (`setsid`), as a daemon or an ssh exec would,
//! so `/dev/tty` is ENXIO for su and sudo exactly as on Linux. (Run from the
//! serial console, the console would be the controlling terminal, and su
//! would prompt there.) One session for all of them, so sudo's credential
//! cache, keyed on the session, carries from one call to the next.
//! Output: one `PASS`/`FAIL` line per check, then
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
    // A session with no controlling terminal (see the top of the file). The
    // caller is usually a process-group leader, which cannot setsid itself.
    let child = unsafe { libc::fork() };
    if child > 0 {
        let mut st = 0;
        unsafe { libc::waitpid(child, &mut st, 0) };
        std::process::exit(if libc::WIFEXITED(st) { libc::WEXITSTATUS(st) } else { 1 });
    }
    if child < 0 || unsafe { libc::setsid() } < 0 {
        eprintln!("sudotest: fork/setsid failed");
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

    // AT_SECURE: a setuid dynamic binary must ignore LD_PRELOAD (musl would
    // otherwise abort the exec with "Error loading shared library").
    let o = run(LEANDRO, &[SUDO, "-S", "id"], &[("LD_PRELOAD", "/nonexistent/evil.so")], "leandro\n");
    t.check(
        "leandro: setuid sudo ignores LD_PRELOAD (AT_SECURE)",
        o.code == 0 && o.stdout.contains("uid=0(root)") && !o.all().contains("Error loading"),
        &o,
    );
    let o = run(LEANDRO, &[SU, "-c", "id", "root"], &[("LD_PRELOAD", "/nonexistent/evil.so")], "root\n");
    t.check(
        "leandro: setuid su ignores LD_PRELOAD (AT_SECURE)",
        o.code == 0 && o.stdout.contains("uid=0(root)") && !o.all().contains("Error loading"),
        &o,
    );
    // A locked account (`passwd -l`: "!" prefix on the hash) is refused even
    // with the right password; unlocking restores it.
    if let Ok(orig) = std::fs::read_to_string("/etc/shadow") {
        let locked: String = orig
            .lines()
            .map(|l| if l.starts_with("leandro:") { format!("leandro:!{}", &l["leandro:".len()..]) } else { l.to_string() })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        if std::fs::write("/etc/shadow", &locked).is_ok() {
            let _ = run(LEANDRO, &[SUDO, "-K"], &[], "");
            let o = run(LEANDRO, &[SUDO, "-S", "id"], &[], "leandro\n");
            t.check("leandro: locked (passwd -l) account rejected by sudo", o.code != 0 && !o.stdout.contains("uid=0"), &o);
            let o = run(GREETER, &[SU, "-c", "id", "leandro"], &[], "leandro\n");
            t.check("locked account rejected by su", o.code != 0 && !o.stdout.contains("uid=1000"), &o);
            let _ = std::fs::write("/etc/shadow", &orig);
            let o = run(LEANDRO, &[SUDO, "-S", "id"], &[], "leandro\n");
            t.check("leandro: unlocked account works again", o.code == 0 && o.stdout.contains("uid=0(root)"), &o);
        }
    }

    // The unlocked-account check above authenticated and cached a timestamp: start the
    // "no cached credentials" checks from a clean slate.
    let _ = run(LEANDRO, &[SUDO, "-K"], &[], "");
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

    // ── su/sudo write no utmp/wtmp records (Linux: pam_unix does not; only
    // login(1), sshd, getty and display managers do) ────────────────────────
    let sz = |p: &str| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let (u0, w0) = (sz("/run/utmps/utmp"), sz("/var/log/wtmp"));
    let o = run(As::Root, &[SUDO, "-n", "id"], &[], "");
    let o2 = run(As::Root, &[SU, "leandro", "-c", "id"], &[], "");
    t.check(
        "su and sudo leave utmp and wtmp untouched (as Linux)",
        o.code == 0 && o2.code == 0 && sz("/run/utmps/utmp") == u0 && sz("/var/log/wtmp") == w0,
        &o,
    );

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

    pam_checks(&mut t);
    login_checks(&mut t);
    devtty_checks(&mut t);
    su_background(&mut t);
    su_background_pty(&mut t);

    println!("SUDOTEST pass={} fail={}", t.pass, t.fail);
    std::process::exit(if t.fail == 0 { 0 } else { 1 });
}

/// `/proc/<pid>/stat` fields 3..=6 (state, ppid, pgrp, session).
fn proc_ids(pid: i32) -> Option<(char, i32, i32, i32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut f = stat[stat.rfind(')')? + 2..].split_whitespace();
    let st = f.next()?.chars().next()?;
    let ppid = f.next()?.parse().ok()?;
    let pgrp = f.next()?.parse().ok()?;
    let sid = f.next()?.parse().ok()?;
    Some((st, ppid, pgrp, sid))
}

/// Real and effective uid from `/proc/<pid>/status`.
fn proc_uids(pid: i32) -> Option<(u32, u32)> {
    let st = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let l = st.lines().find(|l| l.starts_with("Uid:"))?;
    let mut it = l[4..].split_whitespace().map(|w| w.parse::<u32>());
    Some((it.next()?.ok()?, it.next()?.ok()?))
}

fn alive(pid: i32) -> bool {
    matches!(proc_ids(pid), Some((st, ..)) if st != 'Z')
}

/// Wait up to `ms` for `pid` to be gone (or a zombie).
fn wait_gone(pid: i32, ms: u64) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(ms) {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}

/// `/dev/tty` with no controlling terminal is ENXIO (Linux), not a proxy of
/// stdin: sudo-rs used to carry a patch for that (ports/sudo-rs README).
fn devtty_checks(t: &mut T) {
    // In a fresh session (no ctty), stdin a pipe.
    let o = run(As::Root, &["/bin/sh", "-c", "cat /dev/tty </dev/null; echo rc=$?"], &[], "");
    t.check(
        "/dev/tty without a controlling terminal: open fails (ENXIO)",
        o.stdout.contains("rc=1") && o.all().contains("No such device or address"),
        &o,
    );
    let o = run(As::Root, &["/bin/sh", "-c", "stat -c '%t:%T' /dev/tty /dev/console /dev/null"], &[], "");
    t.check(
        "stat /dev/tty /dev/console /dev/null: st_rdev 5:0, 5:1, 1:3",
        o.code == 0 && o.stdout.split_whitespace().collect::<Vec<_>>() == ["5:0", "5:1", "1:3"],
        &o,
    );
}

/// Parse `BG=<pid> SH=<pid>` from a `-c` script's output.
fn bg_line(out: &str) -> Option<(i32, i32)> {
    let l = out.lines().find(|l| l.starts_with("BG="))?;
    let mut it = l.split_whitespace();
    let bg = it.next()?.strip_prefix("BG=")?.parse().ok()?;
    let sh = it.next()?.strip_prefix("SH=")?.parse().ok()?;
    Some((bg, sh))
}

const BG_SCRIPT: &str = "sleep 30 </dev/null >/dev/null 2>&1 & echo BG=$! SH=$$; cat /proc/$$/stat";

/// `su leandro -c 'cmd &'` with no terminal (an ssh exec, a script): su
/// returns as soon as the shell does, and the background child lives on as
/// leandro, reparented to init, in the shell's session and process group (a
/// non-interactive shell has no job control), as on Linux.
fn su_background(t: &mut T) {
    let t0 = Instant::now();
    let o = run(As::Root, &[SU, "leandro", "-c", BG_SCRIPT], &[], "");
    let secs = t0.elapsed().as_secs_f32();
    t.check(&format!("su leandro -c 'sleep 30 &' returns at once ({secs:.1} s)"), o.code == 0 && secs < 10.0, &o);
    let Some((bg, sh)) = bg_line(&o.stdout) else {
        t.check("su background: script printed the pids", false, &o);
        return;
    };
    // The shell's own ids (pgrp, session), read by the shell itself.
    let sh_ids: Vec<i32> = o
        .stdout
        .lines()
        .find(|l| l.starts_with(&format!("{sh} (")))
        .map(|l| {
            l[l.rfind(')').unwrap_or(0) + 2..]
                .split_whitespace()
                .skip(2)
                .take(2)
                .filter_map(|w| w.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    std::thread::sleep(Duration::from_millis(300));
    let ids = proc_ids(bg);
    let uids = proc_uids(bg);
    let d = Out {
        code: o.code,
        stdout: format!("{} bg={bg} sh={sh} sh_ids={sh_ids:?} bg_ids={ids:?} uids={uids:?}", o.stdout),
        stderr: o.stderr.clone(),
    };
    t.check("su background: the child survives su", alive(bg), &d);
    t.check("su background: the child runs as leandro (uid and euid 1000)", uids == Some((1000, 1000)), &d);
    let init = ids.map_or(false, |(_, pp, _, _)| {
        std::fs::read_to_string(format!("/proc/{pp}/comm")).map_or(false, |c| c.trim() == "init")
    });
    t.check("su background: the orphan is reparented to init", init, &d);
    t.check(
        "su background: same session and process group as the shell that started it",
        sh_ids.len() == 2 && matches!(ids, Some((_, _, pg, sid)) if pg == sh_ids[0] && sid == sh_ids[1]),
        &d,
    );
    unsafe {
        libc::kill(bg, libc::SIGKILL);
    }
    t.check("su background: the child is killable and goes away", wait_gone(bg, 3000), &d);
}

/// The same from a terminal (an ssh login): su runs the command on a pty of
/// its own (sudo-rs `use_pty`). su must still return when the shell exits.
/// Whether the background child then gets a SIGHUP depends on which group is
/// in the foreground of su's pty when its session ends (sudo-rs's monitor
/// takes the terminal back first); one that ignores SIGHUP survives as
/// leandro either way.
fn su_background_pty(t: &mut T) {
    for (ignore_hup, name) in [(false, "SIGHUP default"), (true, "SIGHUP ignored")] {
        let mut m: libc::c_int = -1;
        let mut s: libc::c_int = -1;
        let r = unsafe {
            libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null(), std::ptr::null())
        };
        if r != 0 {
            t.check("su on a pty: openpty", false, &Out { code: -1, stdout: String::new(), stderr: String::new() });
            return;
        }
        let script = if ignore_hup { format!("trap '' HUP; {BG_SCRIPT}") } else { BG_SCRIPT.to_string() };
        let cscript = CString::new(script).unwrap();
        let csu = CString::new(SU).unwrap();
        let cuser = CString::new("leandro").unwrap();
        let cc = CString::new("-c").unwrap();
        let e1 = CString::new("PATH=/usr/bin:/bin").unwrap();
        let e2 = CString::new("TERM=vt100").unwrap();
        let e3 = CString::new("HOME=/root").unwrap();
        let t0 = Instant::now();
        let child = unsafe { libc::fork() };
        if child == 0 {
            unsafe {
                libc::close(m);
                libc::setsid();
                libc::ioctl(s, libc::TIOCSCTTY, 0);
                libc::dup2(s, 0);
                libc::dup2(s, 1);
                libc::dup2(s, 2);
                if s > 2 {
                    libc::close(s);
                }
                let argv = [csu.as_ptr(), cuser.as_ptr(), cc.as_ptr(), cscript.as_ptr(), std::ptr::null()];
                let envp = [e1.as_ptr(), e2.as_ptr(), e3.as_ptr(), std::ptr::null()];
                libc::execve(csu.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }
        unsafe {
            libc::close(s);
            libc::fcntl(m, libc::F_SETFL, libc::O_NONBLOCK);
        }
        // Drain the master until su exits.
        let mut out = Vec::new();
        let mut status = 0;
        let mut done = false;
        while t0.elapsed() < Duration::from_secs(30) {
            let mut b = [0u8; 512];
            let n = unsafe { libc::read(m, b.as_mut_ptr() as *mut libc::c_void, b.len()) };
            if n > 0 {
                out.extend_from_slice(&b[..n as usize]);
                continue;
            }
            if unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) } == child {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let secs = t0.elapsed().as_secs_f32();
        if !done {
            unsafe {
                libc::kill(child, libc::SIGKILL);
                libc::waitpid(child, &mut status, 0);
            }
        }
        unsafe {
            libc::close(m);
        }
        let text = String::from_utf8_lossy(&out).replace('\r', "");
        let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 };
        let o = Out { code, stdout: text.clone(), stderr: String::new() };
        t.check(
            &format!("su on a pty, {name}: su -c 'sleep 30 &' returns ({secs:.1} s)"),
            done && code == 0 && secs < 15.0,
            &o,
        );
        let Some((bg, _)) = bg_line(&text) else {
            t.check(&format!("su on a pty, {name}: script printed the pids"), false, &o);
            continue;
        };
        if ignore_hup {
            std::thread::sleep(Duration::from_millis(300));
            let d = Out {
                code,
                stdout: format!("{text} ids={:?} uids={:?}", proc_ids(bg), proc_uids(bg)),
                stderr: String::new(),
            };
            t.check(
                &format!("su on a pty, {name}: the child survives as leandro"),
                alive(bg) && proc_uids(bg) == Some((1000, 1000)),
                &d,
            );
        } else {
            std::thread::sleep(Duration::from_millis(300));
            let ok = !alive(bg) || proc_uids(bg) == Some((1000, 1000));
            let d = Out { code, stdout: format!("{text} ids={:?}", proc_ids(bg)), stderr: String::new() };
            println!("  su on a pty, {name}: background child {}", if alive(bg) { "survived" } else { "gone" });
            t.check(&format!("su on a pty, {name}: no stray root child"), ok, &d);
        }
        unsafe {
            libc::kill(bg, libc::SIGKILL);
        }
        wait_gone(bg, 2000);
    }
}

// ── libpam ABI (Alpine's real Linux-PAM, /etc/pam.d), driven directly ──────────────────────────────
//
// The same functions greetd (login screen), su and sudo-rs call. A wrong
// password must be rejected on every service name, accepted when right, and a
// failure must be delayed (rate limiting).

use std::os::raw::{c_char, c_int, c_void};

#[repr(C)]
struct PamMsg {
    style: c_int,
    msg: *const c_char,
}
#[repr(C)]
struct PamResp {
    resp: *mut c_char,
    code: c_int,
}
#[repr(C)]
struct PamConvT {
    conv: Option<extern "C" fn(c_int, *mut *const PamMsg, *mut *mut PamResp, *mut c_void) -> c_int>,
    data: *mut c_void,
}

#[link(name = "pam")]
extern "C" {
    fn pam_start(svc: *const c_char, user: *const c_char, conv: *const PamConvT, h: *mut *mut c_void) -> c_int;
    fn pam_authenticate(h: *mut c_void, flags: c_int) -> c_int;
    fn pam_acct_mgmt(h: *mut c_void, flags: c_int) -> c_int;
    fn pam_setcred(h: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(h: *mut c_void, status: c_int) -> c_int;
    fn pam_strerror(h: *mut c_void, e: c_int) -> *const c_char;
}

extern "C" fn pam_conv_cb(n: c_int, _m: *mut *const PamMsg, r: *mut *mut PamResp, data: *mut c_void) -> c_int {
    unsafe {
        let pw = &*(data as *const CString);
        let resp = libc::calloc(n as usize, std::mem::size_of::<PamResp>()) as *mut PamResp;
        (*resp).resp = libc::strdup(pw.as_ptr());
        *r = resp;
    }
    0
}

/// (rc of pam_authenticate, milliseconds it took) for `svc`/`user`/`pw`.
fn pam_try(svc: &str, user: &str, pw: &str) -> (c_int, u128) {
    let (svc, user, pw) = (CString::new(svc).unwrap(), CString::new(user).unwrap(), CString::new(pw).unwrap());
    let conv = PamConvT { conv: Some(pam_conv_cb), data: &pw as *const CString as *mut c_void };
    let mut h: *mut c_void = std::ptr::null_mut();
    unsafe {
        assert_eq!(pam_start(svc.as_ptr(), user.as_ptr(), &conv, &mut h), 0);
        let t0 = Instant::now();
        let rc = pam_authenticate(h, 0);
        let ms = t0.elapsed().as_millis();
        if rc == 0 {
            assert_eq!(pam_acct_mgmt(h, 0), 0);
            assert_eq!(pam_setcred(h, 2), 0);
        }
        let _ = pam_strerror(h, rc);
        pam_end(h, rc);
        (rc, ms)
    }
}

fn pam_checks(t: &mut T) {
    let none = Out { code: 0, stdout: String::new(), stderr: String::new() };
    // As root, so the su/su-l pam_rootok shortcut would fire for those names;
    // the login-style services must still demand the password.
    for svc in ["greetd", "login", "sshd", "sudo"] {
        let (rc, _) = pam_try(svc, "leandro", "leandro");
        t.check(&format!("pam {svc}: right password accepted"), rc == 0, &none);
        let (rc, ms) = pam_try(svc, "leandro", "wrong");
        t.check(&format!("pam {svc}: wrong password rejected (rc={rc})"), rc == 7, &none);
        t.check(&format!("pam {svc}: failure is delayed (rate limit), took {ms} ms"), ms >= 900, &none);
        let (rc, _) = pam_try(svc, "leandro", "");
        t.check(&format!("pam {svc}: empty password rejected"), rc != 0, &none);
        let (rc, _) = pam_try(svc, "nosuchuser", "leandro");
        t.check(&format!("pam {svc}: unknown user rejected"), rc != 0, &none);
        let (rc, _) = pam_try(svc, "leandro", "leandro ");
        t.check(&format!("pam {svc}: password with trailing byte rejected"), rc != 0, &none);
    }
    // pam_rootok (Alpine's su stack): root switches user with no password at all,
    // for su and su-l, while a non-root caller is covered by sudotest's su cases.
    for svc in ["su", "su-l"] {
        let (rc, _) = pam_try(svc, "leandro", "");
        t.check(&format!("pam {svc}: root needs no password (pam_rootok)"), rc == 0, &none);
    }
    // Locked accounts (`!`/`*` hash) never authenticate.
    let sh = std::fs::read_to_string("/etc/shadow").unwrap_or_default();
    let locked = sh.lines().find_map(|l| {
        let mut f = l.split(':');
        let u = f.next()?;
        let h = f.next()?;
        (h.starts_with('!') || h == "*").then(|| u.to_string())
    });
    if let Some(u) = locked {
        let (rc, _) = pam_try("greetd", &u, "");
        let (rc2, _) = pam_try("greetd", &u, "x");
        t.check(&format!("pam greetd: locked account {u} rejected"), rc != 0 && rc2 != 0, &none);
    }
}

/// /bin/login (console/VT sessions; no PAM): a failed attempt stalls ~1 s like
/// pam-leandros, then a second, correct attempt on the same prompt loop works.
fn login_checks(t: &mut T) {
    let t0 = Instant::now();
    let bad = run(As::Root, &["/bin/login"], &[], "leandro\nwrong\n");
    let ms = t0.elapsed().as_millis();
    t.check("login: wrong password rejected", bad.all().contains("Login incorrect") && !bad.all().contains("uid="), &bad);
    t.check(&format!("login: failure is delayed (rate limit), took {ms} ms"), ms >= 900, &bad);
    let t1 = Instant::now();
    let ok = run(As::Root, &["/bin/login"], &[], "leandro\nwrong\nleandro\nleandro\nid -un\nexit\n");
    let ms = t1.elapsed().as_millis();
    t.check("login: wrong then right password logs in", ok.all().contains("Login incorrect") && ok.stdout.lines().any(|l| l.trim() == "leandro"), &ok);
    t.check(&format!("login: delay applied before the retry, took {ms} ms"), ms >= 900, &ok);
}
