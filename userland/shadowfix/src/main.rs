//! shadowfix: make /etc/shadow readable by musl getspnam() (real PAM).
//!
//! musl's getspnam_r() -> __parsespent() (musl 1.2.5 src/passwd/getspnam_r.c;
//! Alpine's unix_chkpwd, and so Linux-PAM, uses it) accepts a record only if
//! it has EXACTLY nine fields, every numeric field is empty or digits, and the
//! line ends in '\n'. It rejects fewer fields, more fields, a missing final
//! newline and a trailing '\r'. Older LeandrOS images wrote `root:HASH:`, and
//! PAM then reports "Authentication service cannot retrieve authentication
//! info". This tool repairs, and changes nothing else:
//!
//!  * fewer than nine fields: empty fields are appended. An old lstchg of `0`
//!    on such a short record (`root:H:0`) is blanked, because `0` means "must
//!    change the password at next login" and the short record never said that;
//!  * a missing final newline and a trailing CR are fixed;
//!  * more than nine fields: trailing EMPTY extras are dropped (provably no
//!    information); non-empty extras, or any other record musl would still
//!    reject, are reported loudly (exit 2) and left byte for byte;
//!  * a legacy `$sha256$` hash that verifies against the account's own name
//!    (the image's dev passwords were root/root and leandro/leandro) is replaced
//!    by a fresh-salted `$6$` hash of the same password, since PAM cannot verify
//!    `$sha256$`. Every other hash is untouched;
//!  * a /etc/passwd account (password field `x`) without a shadow record gets a
//!    locked `name:!::0:99999:7:::` record. Linux does the same: pwconv(8)
//!    creates missing shadow entries and useradd always writes one, and Alpine
//!    ships `!` records for every system account; PAM's unix_chkpwd otherwise
//!    answers "no such user" for them.
//!
//! The rewrite is crash safe: temp file in the same directory, fsync, rename,
//! fsync of the directory; owner and mode are kept. Before the rename the new
//! text is checked record by record against the old (same names, same hashes
//! except a verified migration, extra records only locked `!` ones for passwd
//! accounts), else nothing is written. A second run is a no-op.
//!
//! usage: shadowfix [FILE]            (default /etc/shadow; passwd = sibling)
//!        shadowfix --check [FILE]    read only; exit 0 only if PAM can use FILE

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{chown, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

#[allow(dead_code)]
#[path = "../../../lib/shadowhash/shadowhash.rs"]
mod shadowhash;

const FULL_COLONS: usize = 8;

fn xatol(s: &[u8], i: &mut usize) {
    if matches!(s.get(*i), Some(b':') | Some(b'\n')) {
        return;
    }
    while matches!(s.get(*i), Some(c) if c.is_ascii_digit()) {
        *i += 1;
    }
}

/// Would musl's __parsespent() accept `line` + '\n'?
pub fn musl_ok(line: &[u8]) -> bool {
    let mut s = line.to_vec();
    s.push(b'\n');
    let Some(a) = s.iter().position(|&b| b == b':') else { return false };
    let Some(b) = s[a + 1..].iter().position(|&b| b == b':') else { return false };
    let mut i = a + 1 + b + 1;
    for _ in 0..6 {
        xatol(&s, &mut i);
        if s.get(i) != Some(&b':') {
            return false;
        }
        i += 1;
    }
    xatol(&s, &mut i);
    s.get(i) == Some(&b'\n')
}

#[derive(Default, Debug, PartialEq)]
pub struct Report {
    pub padded: usize,
    pub newline: usize,
    pub cr: usize,
    pub trimmed: usize,
    pub migrated: Vec<String>,
    pub added: Vec<String>,
    pub unrepairable: Vec<String>,
}

impl Report {
    fn changed(&self) -> bool {
        self.padded + self.newline + self.cr + self.trimmed > 0 || !self.migrated.is_empty() || !self.added.is_empty()
    }
}

fn passwd_names(passwd: &[u8]) -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    for l in passwd.split(|&b| b == b'\n') {
        let mut f = l.split(|&b| b == b':');
        let (n, p) = (f.next().unwrap_or(&[]), f.next().unwrap_or(&[]));
        if !n.is_empty() && n[0] != b'#' && p == b"x" {
            v.push(n.to_vec());
        }
    }
    v
}

fn name_of(line: &[u8]) -> &[u8] {
    line.split(|&b| b == b':').next().unwrap_or(&[])
}

/// Repair `src`. `passwd` is the sibling /etc/passwd text, `rnd` supplies 16
/// random bytes per migrated hash. Returns the new text (None when nothing
/// changed) and the report.
pub fn fix(src: &[u8], passwd: &[u8], rnd: &mut dyn FnMut() -> Option<[u8; 16]>) -> (Option<Vec<u8>>, Report) {
    let mut rep = Report::default();
    let mut out: Vec<u8> = Vec::with_capacity(src.len() + 128);
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut rest = src;
    while !rest.is_empty() {
        let (raw, had_nl, next) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => (&rest[..i], true, &rest[i + 1..]),
            None => (rest, false, &rest[rest.len()..]),
        };
        rest = next;
        let is_record = !raw.is_empty() && raw[0] != b'#' && raw[0] != b':' && raw.contains(&b':');
        if !is_record {
            out.extend_from_slice(raw);
            if had_nl {
                out.push(b'\n');
            }
            continue;
        }
        names.push(name_of(raw).to_vec());
        let mut line = raw.to_vec();
        let had_cr = line.last() == Some(&b'\r');
        if had_cr {
            line.pop();
        }
        let mut f: Vec<Vec<u8>> = line.split(|&b| b == b':').map(|x| x.to_vec()).collect();
        let (mut padded, mut trimmed) = (false, false);
        if f.len() < 9 {
            if f.len() >= 3 && f[2] == b"0" {
                f[2].clear();
            }
            while f.len() < 9 {
                f.push(Vec::new());
            }
            padded = true;
        } else if f.len() > 9 {
            if f[9..].iter().all(|x| x.is_empty()) {
                f.truncate(9);
                trimmed = true;
            }
        }
        let mut cand = f.join(&b':');
        if !musl_ok(&cand) {
            // Still unusable by musl (extra data, odd numbers): leave exactly as found.
            rep.unrepairable.push(String::from_utf8_lossy(name_of(raw)).into_owned());
            out.extend_from_slice(raw);
            if had_nl {
                out.push(b'\n');
            }
            continue;
        }
        if padded {
            rep.padded += 1;
        }
        if trimmed {
            rep.trimmed += 1;
        }
        if had_cr {
            rep.cr += 1;
        }
        if !had_nl {
            rep.newline += 1;
        }
        let name = f[0].clone();
        if f[1].starts_with(b"$sha256$") && !name.is_empty() {
            let (old, nm) = (String::from_utf8_lossy(&f[1]).into_owned(), name.as_slice());
            if shadowhash::verify(&old, nm) {
                if let Some(r) = rnd() {
                    let mut buf = [0u8; 128];
                    if let Some(n) = shadowhash::upgrade_field(nm, &r, &mut buf) {
                        f[1] = buf[..n].to_vec();
                        cand = f.join(&b':');
                        rep.migrated.push(String::from_utf8_lossy(nm).into_owned());
                    }
                }
            }
        }
        out.extend_from_slice(&cand);
        out.push(b'\n');
    }
    for n in passwd_names(passwd) {
        if !names.contains(&n) {
            out.extend_from_slice(&n);
            out.extend_from_slice(b":!::0:99999:7:::\n");
            rep.added.push(String::from_utf8_lossy(&n).into_owned());
            names.push(n);
        }
    }
    let changed = rep.changed();
    (changed.then_some(out), rep)
}

fn records(text: &[u8]) -> Vec<Vec<u8>> {
    text.split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .filter(|l| !l.is_empty())
        .map(|l| l.to_vec())
        .collect()
}

/// The strict guard: names and hashes survive, except a verified migration;
/// extra records are only locked `!` ones for passwd accounts.
fn guard(old: &[u8], new: &[u8], passwd: &[u8]) -> Result<(), String> {
    let (o, n) = (records(old), records(new));
    if n.len() < o.len() {
        return Err("record count shrank".into());
    }
    let hash = |l: &[u8]| l.split(|&b| b == b':').nth(1).unwrap_or(&[]).to_vec();
    for (i, ol) in o.iter().enumerate() {
        let (nl, name) = (&n[i], name_of(ol));
        if name_of(nl) != name {
            return Err(format!("record {i} name would change"));
        }
        let (oh, nh) = (hash(ol), hash(nl));
        if oh == nh {
            continue;
        }
        let (os, ns) = (String::from_utf8_lossy(&oh).into_owned(), String::from_utf8_lossy(&nh).into_owned());
        let ok = os.starts_with("$sha256$") && ns.starts_with("$6$") && shadowhash::verify(&os, name) && shadowhash::verify(&ns, name);
        if !ok {
            return Err(format!("hash of {} would change", String::from_utf8_lossy(name)));
        }
    }
    let pn = passwd_names(passwd);
    for l in &n[o.len()..] {
        if hash(l) != b"!" || !pn.contains(&name_of(l).to_vec()) || o.iter().any(|x| name_of(x) == name_of(l)) {
            return Err("unexpected added record".into());
        }
    }
    Ok(())
}

/// Problems that make real PAM fail (errors) and things worth knowing (warnings).
pub fn check(src: &[u8], passwd: &[u8]) -> (Vec<String>, Vec<String>) {
    let (mut bad, mut warn) = (Vec::new(), Vec::new());
    let mut names = Vec::new();
    let mut rest = src;
    while !rest.is_empty() {
        let (raw, had_nl, next) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => (&rest[..i], true, &rest[i + 1..]),
            None => (rest, false, &rest[rest.len()..]),
        };
        rest = next;
        if raw.is_empty() || raw[0] == b'#' || !raw.contains(&b':') {
            continue;
        }
        let name = String::from_utf8_lossy(name_of(raw)).into_owned();
        names.push(name_of(raw).to_vec());
        if !had_nl || !musl_ok(raw) {
            bad.push(format!("{name}: record is not nine clean fields + newline (musl getspnam rejects it)"));
            continue;
        }
        let h = raw.split(|&b| b == b':').nth(1).unwrap_or(&[]);
        if h.starts_with(b"$sha256$") {
            if name == "root" {
                bad.push("root: legacy $sha256$ hash (PAM cannot verify it)".into());
            } else {
                warn.push(format!("{name}: legacy $sha256$ hash; PAM refuses it until the password is reset (login or ssh rehashes it)"));
            }
        }
    }
    if !names.contains(&b"root".to_vec()) {
        bad.push("root: no shadow record".into());
    }
    for n in passwd_names(passwd) {
        if !names.contains(&n) {
            bad.push(format!("{}: in passwd but no shadow record", String::from_utf8_lossy(&n)));
        }
    }
    (bad, warn)
}

fn urandom() -> Option<[u8; 16]> {
    let mut b = [0u8; 16];
    fs::File::open("/dev/urandom").ok()?.read_exact(&mut b).ok()?;
    Some(b)
}

fn passwd_of(path: &str) -> Vec<u8> {
    let dir = Path::new(path).parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::read(dir.join("passwd")).unwrap_or_default()
}

fn run(path: &str, rnd: &mut dyn FnMut() -> Option<[u8; 16]>) -> Result<Report, String> {
    let src = match fs::read(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Report::default()),
        Err(e) => return Err(format!("read {path}: {e}")),
    };
    let passwd = passwd_of(path);
    let (new, rep) = fix(&src, &passwd, rnd);
    let Some(new) = new else { return Ok(rep) };
    guard(&src, &new, &passwd).map_err(|e| format!("internal check failed: {e}; not writing"))?;
    let meta = fs::metadata(path).map_err(|e| format!("stat {path}: {e}"))?;
    let tmp = format!("{path}.shadowfix-new");
    let _ = fs::remove_file(&tmp);
    let res = (|| -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(meta.mode() & 0o7777))?;
        chown(&tmp, Some(meta.uid()), Some(meta.gid()))?;
        f.write_all(&new)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        if let Some(dir) = Path::new(path).parent() {
            if let Ok(d) = fs::File::open(if dir.as_os_str().is_empty() { Path::new(".") } else { dir }) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    })();
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(format!("rewrite {path}: {e}"));
    }
    Ok(rep)
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let is_check = args.first().map(|a| a == "--check").unwrap_or(false);
    if is_check {
        args.remove(0);
    }
    let path = args.first().cloned().unwrap_or_else(|| "/etc/shadow".to_string());
    if is_check {
        let src = fs::read(&path).unwrap_or_default();
        let (bad, warn) = check(&src, &passwd_of(&path));
        for w in &warn {
            eprintln!("shadowfix: warning: {w}");
        }
        for b in &bad {
            eprintln!("shadowfix: PROBLEM: {b}");
        }
        if bad.is_empty() {
            println!("shadowfix: {path} usable by PAM");
        }
        std::process::exit(if bad.is_empty() { 0 } else { 3 });
    }
    match run(&path, &mut urandom) {
        Ok(r) => {
            if r.changed() {
                println!(
                    "shadowfix: {path}: padded {}, newline {}, CR {}, extra fields {}, hash migrated {:?}, locked records added {:?}",
                    r.padded, r.newline, r.cr, r.trimmed, r.migrated, r.added
                );
            }
            if !r.unrepairable.is_empty() {
                eprintln!("shadowfix: UNREPAIRABLE records left as found (PAM will refuse them): {:?}", r.unrepairable);
                std::process::exit(2);
            }
        }
        Err(e) => {
            eprintln!("shadowfix: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rnd() -> Option<[u8; 16]> {
        Some([5u8; 16])
    }
    fn fixed(src: &[u8], passwd: &[u8]) -> (Option<Vec<u8>>, Report) {
        fix(src, passwd, &mut rnd)
    }
    // sha256(salt ++ password) for the legacy format, computed with the verifier's own hash
    fn legacy(salt: &str, pw: &str) -> String {
        let d = shadowhash::sha256_for_test(salt.as_bytes(), pw.as_bytes());
        format!("$sha256${salt}${d}")
    }

    #[test]
    fn pads_short_and_keeps_hash() {
        let src = b"root:$6$s$h:\nleandro:$6$x$y\nbin:*:20000:0:99999:7:::\n";
        let out = fixed(src, b"").0.unwrap();
        assert_eq!(out, b"root:$6$s$h:::::::\nleandro:$6$x$y:::::::\nbin:*:20000:0:99999:7:::\n".to_vec());
        guard(src, &out, b"").unwrap();
    }

    #[test]
    fn idempotent_and_noop_on_valid() {
        assert!(fixed(b"root:H:20000:0:99999:7:::\n", b"").0.is_none());
        let out = fixed(b"root:H:\n", b"").0.unwrap();
        assert!(fixed(&out, b"").0.is_none());
    }

    #[test]
    fn missing_final_newline_on_nine_field_line() {
        let (o, r) = fixed(b"root:H:20000:0:99999:7:::", b"");
        assert_eq!(o.unwrap(), b"root:H:20000:0:99999:7:::\n".to_vec());
        assert_eq!(r.newline, 1);
        assert_eq!(fixed(b"a:H:1:0:99999:7\nb:H:", b"").0.unwrap(), b"a:H:1:0:99999:7:::\nb:H:::::::\n".to_vec());
    }

    #[test]
    fn crlf_repaired() {
        let o = fixed(b"root:H:20000:0:99999:7:::\r\nb:H:\r\n", b"").0.unwrap();
        assert_eq!(o, b"root:H:20000:0:99999:7:::\nb:H:::::::\n".to_vec());
    }

    #[test]
    fn root_h_0_is_not_a_forced_change() {
        let o = fixed(b"root:H:0\n", b"").0.unwrap();
        assert_eq!(o, b"root:H:::::::\n".to_vec());
        // an explicit nine-field 0 is the admin's choice and is kept
        assert!(fixed(b"root:H:0:0:99999:7:::\n", b"").0.is_none());
    }

    #[test]
    fn extra_fields() {
        assert_eq!(fixed(b"root:H:1:0:99999:7::::\n", b"").0.unwrap(), b"root:H:1:0:99999:7:::\n".to_vec());
        let (o, r) = fixed(b"root:H:1:0:99999:7:::junk\n", b"");
        assert!(o.is_none());
        assert_eq!(r.unrepairable, vec!["root".to_string()]);
    }

    #[test]
    fn musl_rules() {
        assert!(musl_ok(b"root:H:20000:0:99999:7:::"));
        assert!(!musl_ok(b"root:H:20000:0:99999:7::"));
        assert!(!musl_ok(b"root:H:20000:0:99999:7:::\r"));
        assert!(!musl_ok(b"root:H:x:0:99999:7:::"));
    }

    #[test]
    fn legacy_hash_migrates_only_for_dev_password() {
        let (l, r) = (legacy("salt", "leandro"), legacy("salt", "SECRET"));
        let src = format!("root:{r}:20000:0:99999:7:::\nleandro:{l}:20000:0:99999:7:::\n");
        let (o, rep) = fixed(src.as_bytes(), b"");
        let o = o.unwrap();
        let t = String::from_utf8(o.clone()).unwrap();
        assert_eq!(rep.migrated, vec!["leandro".to_string()]);
        assert!(t.starts_with(&format!("root:{r}:20000:0:99999:7:::\nleandro:$6$rounds=65536$")));
        assert!(t.ends_with(":20000:0:99999:7:::\n"));
        guard(src.as_bytes(), &o, b"").unwrap();
        assert!(fixed(&o, b"").0.is_none());
    }

    #[test]
    fn missing_shadow_user_gets_locked_record() {
        let passwd = b"root:x:0:0::/root:/bin/sh\ncosmic-greeter:x:990:990::/h:/bin/false\nnobody:x:65534:65534::/:/bin/false\nold:!:1:1::/:/bin/sh\n";
        let src = b"root:H:20000:0:99999:7:::\n";
        let (o, rep) = fixed(src, passwd);
        let o = o.unwrap();
        assert_eq!(
            o,
            b"root:H:20000:0:99999:7:::\ncosmic-greeter:!::0:99999:7:::\nnobody:!::0:99999:7:::\n".to_vec()
        );
        assert_eq!(rep.added.len(), 2);
        guard(src, &o, passwd).unwrap();
        assert!(fixed(&o, passwd).0.is_none());
        assert!(check(&o, passwd).0.is_empty());
        assert!(!check(src, passwd).0.is_empty());
    }

    #[test]
    fn guard_rejects_tampering() {
        assert!(guard(b"a:H:::::::\n", b"a:G:::::::\n", b"").is_err());
        assert!(guard(b"a:H:::::::\n", b"b:H:::::::\n", b"").is_err());
        assert!(guard(b"a:H:::::::\n", b"a:H:::::::\nz:!::0:99999:7:::\n", b"").is_err());
    }

    #[test]
    fn leaves_comments_blank_and_garbage() {
        assert!(fixed(b"# c\n\nnocolon\n", b"").0.is_none());
    }

    #[test]
    fn check_flags_root_legacy() {
        let l = legacy("s", "root");
        let src = format!("root:{l}:20000:0:99999:7:::\nleandro:{l}:20000:0:99999:7:::\n");
        let (bad, warn) = check(src.as_bytes(), b"");
        assert_eq!(bad.len(), 1);
        assert_eq!(warn.len(), 1);
    }

    #[test]
    fn file_rewrite_keeps_mode_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("shadowfix-t-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("shadow");
        fs::write(&p, b"root:HASH:\n").unwrap();
        fs::write(dir.join("passwd"), b"root:x:0:0::/root:/bin/sh\nnobody:x:1:1::/:/bin/false\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        let r = run(p.to_str().unwrap(), &mut rnd).unwrap();
        assert_eq!((r.padded, r.added.len()), (1, 1));
        assert_eq!(fs::read(&p).unwrap(), b"root:HASH:::::::\nnobody:!::0:99999:7:::\n");
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o7777, 0o640);
        assert!(!run(p.to_str().unwrap(), &mut rnd).unwrap().changed());
        assert!(!dir.join("shadow.shadowfix-new").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
