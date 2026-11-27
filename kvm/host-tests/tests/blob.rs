//! Checks the kvmtest guest words (userland/kvmtest/src/blob.rs) against
//! `llvm-mc -triple=aarch64 -show-encoding`; skips when llvm-mc is missing.
//! Each line is `    0xXXXXXXXX, // <asm>`; `b .` is the literal 0x14000000.

use std::io::Write;
use std::process::{Command, Stdio};

fn encode(asm: &str) -> Option<u32> {
    let mut c = Command::new("llvm-mc").args(["-triple=aarch64", "-show-encoding"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    c.stdin.take()?.write_all(format!("{asm}\n").as_bytes()).ok()?;
    let out = String::from_utf8(c.wait_with_output().ok()?.stdout).ok()?;
    let b = out.split("encoding: [").nth(1)?.split(']').next()?;
    let v: Vec<u32> = b.split(',').map(|x| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).unwrap()).collect();
    Some(v[0] | v[1] << 8 | v[2] << 16 | v[3] << 24)
}

#[test]
fn kvmtest_blob_matches_llvm_mc() {
    let src = include_str!("../../../userland/kvmtest/src/blob.rs");
    if Command::new("llvm-mc").arg("--version").output().is_err() {
        eprintln!("llvm-mc not installed: skipped");
        return;
    }
    let mut n = 0;
    for line in src.lines().map(str::trim).filter(|l| l.starts_with("0x")) {
        let (w, asm) = line.split_once(", // ").expect("word line shape");
        let want = u32::from_str_radix(&w[2..], 16).unwrap();
        let got = if asm == "b ." { 0x1400_0000 } else { encode(asm).unwrap_or_else(|| panic!("llvm-mc rejected {asm}")) };
        assert_eq!(want, got, "{asm}");
        n += 1;
    }
    assert!(n >= 40, "only {n} words parsed");
}
