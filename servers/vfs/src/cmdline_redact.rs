//! Redaction of secret-looking kernel command-line knobs.
//!
//! The command line can carry credentials (`wifi.psk=` is the documented
//! way to give the Wi-Fi driver a passphrase without a wifi.conf). The
//! kernel echoes the command line to the console, and the console is also
//! copied into the persistent log partition (klog), so an unredacted echo
//! writes the passphrase to the boot disk in plaintext.
//!
//! Rule: a knob is secret when the last dot-separated component of its key
//! (lower-cased) is one of [`EXACT`] or ends with one of [`SUFFIX`]
//! (`wifi.psk`, `foo.password`, `x.api_token`, `y.key`; not `keymap`,
//! `iommu.passthrough`). Its value is replaced by `<redacted len=N>`; the
//! key and N stay visible so a wrong-length passphrase can still be told
//! apart from a missing one. No bytes of the value are emitted.
//!
//! Users: the kernel's console echoes of the command line (always
//! redacted) and `/proc/cmdline` for non-root readers (root gets the real
//! line: `kexec --reuse-cmdline` must hand the next kernel the real PSK).

const EXACT: &[&str] = &["psk", "pmk", "key", "pass", "passwd", "password", "passphrase", "secret", "token"];
const SUFFIX: &[&str] = &["psk", "passwd", "password", "passphrase", "secret", "token"];

/// Is `key` (the part of a `key=value` token before `=`) a secret knob?
pub fn is_secret_key(key: &str) -> bool {
    let last = key.rsplit('.').next().unwrap_or(key).as_bytes();
    let mut lc = [0u8; 48];
    if last.is_empty() || last.len() > lc.len() { return false; }
    for (d, s) in lc.iter_mut().zip(last) { *d = s.to_ascii_lowercase(); }
    let lc = &lc[..last.len()];
    EXACT.iter().any(|e| lc == e.as_bytes()) || SUFFIX.iter().any(|s| lc.ends_with(s.as_bytes()))
}

/// Does `cmdline` contain any secret knob with a non-empty value?
pub fn has_secret(cmdline: &str) -> bool {
    cmdline.split_ascii_whitespace().any(|t| matches!(t.split_once('='), Some((k, v)) if !v.is_empty() && is_secret_key(k)))
}

/// Emit `cmdline` in pieces with every secret knob's value replaced by
/// `<redacted len=N>`. Whitespace runs are kept as they are.
pub fn redact(cmdline: &str, mut out: impl FnMut(&str)) {
    let mut rest = cmdline;
    while !rest.is_empty() {
        let ws = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_whitespace()).len();
        if ws > 0 { out(&rest[..ws]); rest = &rest[ws..]; continue; }
        let end = rest.find(|c: char| c.is_ascii_whitespace()).unwrap_or(rest.len());
        let tok = &rest[..end];
        rest = &rest[end..];
        match tok.split_once('=') {
            Some((k, v)) if !v.is_empty() && is_secret_key(k) => {
                out(k);
                out("=<redacted len=");
                let mut num = [0u8; 20];
                let mut i = num.len();
                let mut n = v.len();
                loop { i -= 1; num[i] = b'0' + (n % 10) as u8; n /= 10; if n == 0 { break; } }
                out(core::str::from_utf8(&num[i..]).unwrap_or("?"));
                out(">");
            }
            _ => out(tok),
        }
    }
}
