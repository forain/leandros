//! Kernel command-line parsing for the root filesystem policy (pure, host-testable:
//! `rustc --edition 2021 --test userland/init/src/rootspec.rs`).

/// Default time to wait for a named root device, seconds.
pub const DEFAULT_ROOTWAIT_SECS: u32 = 30;
/// Upper bound for `rootwait=`, seconds.
pub const MAX_ROOTWAIT_SECS: u32 = 86_400;

/// The value of the last `key=value` token whose key is exactly `key`
/// (`key` includes no `=`). Tokens split on space, tab, newline and NUL.
pub fn token_value<'a>(cmdline: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut found = None;
    for t in cmdline.split(|&c| c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' || c == 0) {
        if t.len() > key.len() && t[key.len()] == b'=' && &t[..key.len()] == key {
            found = Some(&t[key.len() + 1..]);
        }
    }
    found
}

/// The `root=` value; None when absent or empty. Like the kernel, empty
/// `root=` tokens are dropped before the last one wins, so a trailing
/// `root=` does not hide an earlier value.
pub fn root_spec(cmdline: &[u8]) -> Option<&[u8]> {
    let mut found = None;
    for t in cmdline.split(|&c| c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' || c == 0) {
        if t.len() > 5 && &t[..5] == b"root=" {
            found = Some(&t[5..]);
        }
    }
    found
}

/// `rootwait=<seconds>`; the default when absent, empty or not a plain decimal number.
pub fn rootwait_secs(cmdline: &[u8]) -> u32 {
    let v = match token_value(cmdline, b"rootwait") { Some(v) if !v.is_empty() => v, _ => return DEFAULT_ROOTWAIT_SECS };
    let mut n: u32 = 0;
    for &c in v {
        if !c.is_ascii_digit() { return DEFAULT_ROOTWAIT_SECS; }
        n = n.saturating_mul(10).saturating_add((c - b'0') as u32);
    }
    n.min(MAX_ROOTWAIT_SECS)
}

/// `root.fail=reboot`: restart instead of parking when the root cannot be mounted.
pub fn fail_reboot(cmdline: &[u8]) -> bool {
    token_value(cmdline, b"root.fail") == Some(b"reboot")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_exact_token() {
        assert_eq!(root_spec(b"quiet root=PARTUUID=ab-02 rootwait=5"), Some(&b"PARTUUID=ab-02"[..]));
        assert_eq!(root_spec(b"xroot=/dev/sda"), None);
        assert_eq!(root_spec(b"root.fail=reboot"), None);
        assert_eq!(root_spec(b"rootfstype=f2fs"), None);
        assert_eq!(root_spec(b"foo=root=/dev/sda"), None);
        assert_eq!(root_spec(b"root=LABEL=a=b"), Some(&b"LABEL=a=b"[..]));
    }
    #[test]
    fn root_empty_and_unset() {
        assert_eq!(root_spec(b""), None);
        assert_eq!(root_spec(b"\0\0\0"), None);
        assert_eq!(root_spec(b"root="), None);
        assert_eq!(root_spec(b"root"), None);
        assert_eq!(root_spec(b"   \n"), None);
    }
    #[test]
    fn root_last_wins_and_separators() {
        assert_eq!(root_spec(b"root=/dev/a root=/dev/b"), Some(&b"/dev/b"[..]));
        assert_eq!(root_spec(b"a\troot=/dev/a\n"), Some(&b"/dev/a"[..]));
        assert_eq!(root_spec(b"x root=/dev/a\0junk"), Some(&b"/dev/a"[..]));
    }
    #[test]
    fn trailing_empty_root_does_not_hide_earlier() {
        assert_eq!(root_spec(b"root=/dev/a root="), Some(&b"/dev/a"[..]));
        assert_eq!(root_spec(b"root= root=/dev/a root=/dev/b root="), Some(&b"/dev/b"[..]));
        assert_eq!(root_spec(b"root=PARTUUID=x\nroot=\0"), Some(&b"PARTUUID=x"[..]));
    }
    #[test]
    fn rootwait() {
        assert_eq!(rootwait_secs(b""), 30);
        assert_eq!(rootwait_secs(b"rootwait"), 30);
        assert_eq!(rootwait_secs(b"rootwait=12"), 12);
        assert_eq!(rootwait_secs(b"rootwait=0"), 0);
        assert_eq!(rootwait_secs(b"rootwait="), 30);
        assert_eq!(rootwait_secs(b"rootwait=abc"), 30);
        assert_eq!(rootwait_secs(b"rootwait=-3"), 30);
        assert_eq!(rootwait_secs(b"rootwait=5s"), 30);
        assert_eq!(rootwait_secs(b"xrootwait=5"), 30);
        assert_eq!(rootwait_secs(b"rootwait=99999999999999"), MAX_ROOTWAIT_SECS);
        assert_eq!(rootwait_secs(b"rootwait=3 rootwait=7"), 7);
    }
    #[test]
    fn fail_policy() {
        assert!(fail_reboot(b"root.fail=reboot"));
        assert!(fail_reboot(b"a root.fail=reboot b"));
        assert!(!fail_reboot(b""));
        assert!(!fail_reboot(b"root.fail=park"));
        assert!(!fail_reboot(b"xroot.fail=reboot"));
        assert!(!fail_reboot(b"root.fail=rebooting"));
        assert!(!fail_reboot(b"root.fail=reboot root.fail="));
    }
}
