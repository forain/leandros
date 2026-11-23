//! shadowfix: pad short /etc/shadow records to the nine shadow(5) fields.
//!
//! musl's getspnam() (which Alpine's unix_chkpwd, and so real PAM, uses)
//! rejects any record with fewer than nine fields, so a `root:HASH:` line made
//! by an older LeandrOS image locks the account out once PAM replaces
//! /bin/login. This tool appends the missing (empty) fields to such lines and
//! changes nothing else: hashes, field values, owner and mode are kept, valid
//! lines are copied byte for byte, and a second run is a no-op.
//!
//! The rewrite is crash safe: temp file in the same directory, fsync, rename,
//! fsync of the directory. Before the rename the new text is checked line by
//! line against the old (same count, same name and hash), else nothing is
//! written.
//!
//! usage: shadowfix [FILE]    (default /etc/shadow)

use std::fs;
use std::io::Write;
use std::os::unix::fs::{chown, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Number of ':' separators in a complete record (nine fields).
const FULL_COLONS: usize = 8;

/// Pad one line (without its newline). `None` = already fine or not a record.
fn pad_line(line: &[u8]) -> Option<Vec<u8>> {
    if line.is_empty() || line[0] == b'#' || line[0] == b':' {
        return None;
    }
    let colons = line.iter().filter(|&&b| b == b':').count();
    if colons == 0 || colons >= FULL_COLONS {
        return None;
    }
    let mut v = line.to_vec();
    v.extend(std::iter::repeat(b':').take(FULL_COLONS - colons));
    Some(v)
}

/// Rewritten file text, or `None` when no line needed padding.
pub fn migrate(src: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(src.len() + 64);
    let mut changed = false;
    let mut rest = src;
    while !rest.is_empty() {
        let (line, nl, next) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => (&rest[..i], true, &rest[i + 1..]),
            None => (rest, false, &rest[rest.len()..]),
        };
        match pad_line(line) {
            Some(p) => {
                out.extend_from_slice(&p);
                out.push(b'\n');
                changed = true;
            }
            None => {
                out.extend_from_slice(line);
                if nl {
                    out.push(b'\n');
                }
            }
        }
        rest = next;
    }
    changed.then_some(out)
}

/// The first two fields of every record, in order: they must survive intact.
fn identity(text: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    text.split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut f = l.split(|&b| b == b':');
            (f.next().unwrap_or(&[]).to_vec(), f.next().unwrap_or(&[]).to_vec())
        })
        .collect()
}

fn run(path: &str) -> Result<usize, String> {
    let src = match fs::read(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("read {path}: {e}")),
    };
    let Some(new) = migrate(&src) else { return Ok(0) };
    if identity(&src) != identity(&new) {
        return Err("internal check failed: names or hashes would change; not writing".into());
    }
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
    Ok(src.split(|&b| b == b'\n').filter(|l| pad_line(l).is_some()).count())
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/etc/shadow".to_string());
    match run(&path) {
        Ok(0) => {}
        Ok(n) => println!("shadowfix: padded {n} short record(s) in {path}"),
        Err(e) => {
            eprintln!("shadowfix: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pads_short_and_keeps_hash() {
        let src = b"root:$6$s$h:\nleandro:$6$x$y\nbin:*:20000:0:99999:7:::\n";
        let out = migrate(src).unwrap();
        assert_eq!(
            out,
            b"root:$6$s$h:::::::\nleandro:$6$x$y:::::::\nbin:*:20000:0:99999:7:::\n".to_vec()
        );
        assert_eq!(identity(src), identity(&out));
    }

    #[test]
    fn idempotent_and_noop_on_valid() {
        let valid = b"root:H:20000:0:99999:7:::\n";
        assert!(migrate(valid).is_none());
        let out = migrate(b"root:H:\n").unwrap();
        assert!(migrate(&out).is_none());
    }

    #[test]
    fn partial_lines_and_missing_newline() {
        let out = migrate(b"a:H:1:0:99999:7\nb:H:").unwrap();
        assert_eq!(out, b"a:H:1:0:99999:7:::\nb:H:::::::\n".to_vec());
    }

    #[test]
    fn leaves_comments_blank_and_garbage() {
        assert!(migrate(b"# c\n\nnocolon\n").is_none());
    }

    #[test]
    fn file_rewrite_keeps_mode_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("shadowfix-t-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("shadow");
        fs::write(&p, b"root:HASH:\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(run(p.to_str().unwrap()).unwrap(), 1);
        assert_eq!(fs::read(&p).unwrap(), b"root:HASH:::::::\n");
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o7777, 0o640);
        assert_eq!(run(p.to_str().unwrap()).unwrap(), 0);
        assert!(!dir.join("shadow.shadowfix-new").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
