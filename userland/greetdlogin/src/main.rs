//! greetd-login: start a desktop session from a root shell, without the
//! keyboard, by speaking greetd's IPC protocol to the running greeter's greetd.
//!
//!   greetd-login [--user NAME] [--sock PATH] [--cmd 'ARGV...'] < password
//!
//! The password is read from the first line of stdin and never printed. The
//! session command defaults to the `Exec=` line of
//! /usr/share/wayland-sessions/cosmic.desktop, which is what cosmic-greeter
//! starts for a keyboard login. The socket defaults to `$GREETD_SOCK`, else
//! the newest `/run/user/990/greetd-*.sock` (docs/desktop/login.md).
//!
//! Protocol (greetd-ipc): every message is a native-endian u32 length followed
//! by that many bytes of JSON. Requests: create_session,
//! post_auth_message_response, start_session, cancel_session. Responses:
//! success, error, auth_message.
//!
//! Exit status: 0 session started, 1 authentication or greetd error, 2 usage
//! or I/O error.

use std::io::{self, BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::exit;

const DESKTOP: &str = "/usr/share/wayland-sessions/cosmic.desktop";
const FALLBACK_CMD: &[&str] = &["/bin/sh", "/bin/start-cosmic-leandros"];
const SESSION_ENV: &[&str] = &[
    "XDG_SESSION_TYPE=wayland",
    "XDG_CURRENT_DESKTOP=COSMIC",
    "XDG_SESSION_DESKTOP=COSMIC",
];

fn die(code: i32, msg: &str) -> ! {
    eprintln!("greetd-login: {msg}");
    exit(code)
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn json_list(v: &[String]) -> String {
    let items: Vec<String> = v.iter().map(|s| json_str(s)).collect();
    format!("[{}]", items.join(","))
}

/// Value of the top-level string field `key` in a flat JSON object
/// (greetd's responses are flat). None when absent or not a string.
fn field(j: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let mut at = j.find(&pat)? + pat.len();
    let b = j.as_bytes();
    while at < b.len() && (b[at] == b' ' || b[at] == b':') {
        at += 1;
    }
    if at >= b.len() || b[at] != b'"' {
        return None;
    }
    let mut out = String::new();
    let mut it = j[at + 1..].chars();
    while let Some(c) = it.next() {
        match c {
            '"' => return Some(out),
            '\\' => match it.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'u' => {
                    let h: String = it.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&h, 16).ok()?).unwrap_or('?'));
                }
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
    None
}

struct Greetd(UnixStream);

impl Greetd {
    fn call(&mut self, req: &str) -> String {
        let len = (req.len() as u32).to_ne_bytes();
        if let Err(e) = self.0.write_all(&len).and_then(|_| self.0.write_all(req.as_bytes())) {
            die(2, &format!("write to greetd: {e}"));
        }
        let mut l = [0u8; 4];
        if let Err(e) = self.0.read_exact(&mut l) {
            die(2, &format!("read from greetd: {e}"));
        }
        let n = u32::from_ne_bytes(l) as usize;
        if n > 1 << 20 {
            die(2, &format!("greetd reply too long ({n} bytes)"));
        }
        let mut buf = vec![0u8; n];
        if let Err(e) = self.0.read_exact(&mut buf) {
            die(2, &format!("read from greetd: {e}"));
        }
        String::from_utf8_lossy(&buf).into_owned()
    }
}

fn find_sock() -> String {
    if let Ok(s) = std::env::var("GREETD_SOCK") {
        if !s.is_empty() {
            return s;
        }
    }
    let dir = "/run/user/990";
    let mut best: Option<(std::time::SystemTime, String)> = None;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("greetd-") && n.ends_with(".sock") {
                let t = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                if best.as_ref().map_or(true, |(bt, _)| t > *bt) {
                    best = Some((t, format!("{dir}/{n}")));
                }
            }
        }
    }
    match best {
        Some((_, p)) => p,
        None => die(2, "no greetd socket (set GREETD_SOCK or --sock); is the greeter running?"),
    }
}

fn session_cmd() -> Vec<String> {
    if let Ok(t) = std::fs::read_to_string(DESKTOP) {
        for l in t.lines() {
            if let Some(e) = l.strip_prefix("Exec=") {
                let v: Vec<String> = e.split_whitespace().map(String::from).collect();
                if !v.is_empty() {
                    return v;
                }
            }
        }
    }
    FALLBACK_CMD.iter().map(|s| s.to_string()).collect()
}

fn main() {
    let mut user = String::from("leandro");
    let mut sock = None;
    let mut cmd = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--user" => user = args.next().unwrap_or_else(|| die(2, "--user needs a value")),
            "--sock" => sock = args.next(),
            "--cmd" => {
                let c = args.next().unwrap_or_else(|| die(2, "--cmd needs a value"));
                cmd = Some(c.split_whitespace().map(String::from).collect::<Vec<_>>());
            }
            "-h" | "--help" => {
                println!("usage: greetd-login [--user NAME] [--sock PATH] [--cmd 'ARGV...'] < password");
                return;
            }
            _ => die(2, &format!("unknown argument {a}")),
        }
    }
    let mut pw = String::new();
    if io::stdin().lock().read_line(&mut pw).is_err() {
        die(2, "cannot read the password from stdin");
    }
    let pw = pw.trim_end_matches(['\n', '\r']).to_string();
    let sock = sock.unwrap_or_else(find_sock);
    let cmd = cmd.unwrap_or_else(session_cmd);
    let s = UnixStream::connect(&sock).unwrap_or_else(|e| die(2, &format!("connect {sock}: {e}")));
    let mut g = Greetd(s);

    // A previous half-created session would make create_session fail.
    let _ = g.call(r#"{"type":"cancel_session"}"#);
    let mut r = g.call(&format!(r#"{{"type":"create_session","username":{}}}"#, json_str(&user)));
    let mut answered = 0;
    loop {
        match field(&r, "type").as_deref() {
            Some("success") => break,
            Some("auth_message") => {
                let kind = field(&r, "auth_message_type").unwrap_or_default();
                let resp = match kind.as_str() {
                    "secret" | "visible" => {
                        answered += 1;
                        if answered > 1 {
                            let _ = g.call(r#"{"type":"cancel_session"}"#);
                            die(1, "greetd asked for a second secret (wrong password?)");
                        }
                        json_str(&pw)
                    }
                    // info / error: acknowledge with no response.
                    _ => {
                        eprintln!("greetd-login: {kind}: {}", field(&r, "auth_message").unwrap_or_default());
                        "null".into()
                    }
                };
                r = g.call(&format!(r#"{{"type":"post_auth_message_response","response":{resp}}}"#));
            }
            Some("error") => {
                let _ = g.call(r#"{"type":"cancel_session"}"#);
                die(1, &format!(
                    "{}: {}",
                    field(&r, "error_type").unwrap_or_default(),
                    field(&r, "description").unwrap_or_default()
                ));
            }
            _ => die(2, "unexpected greetd reply"),
        }
    }
    let env: Vec<String> = SESSION_ENV.iter().map(|s| s.to_string()).collect();
    let r = g.call(&format!(
        r#"{{"type":"start_session","cmd":{},"env":{}}}"#,
        json_list(&cmd),
        json_list(&env)
    ));
    match field(&r, "type").as_deref() {
        Some("success") => println!("greetd-login: session for {user} started: {}", cmd.join(" ")),
        _ => die(1, &format!(
            "start_session failed: {}",
            field(&r, "description").unwrap_or_else(|| "unexpected reply".into())
        )),
    }
}
