//! Link libnvrm.a (NVIDIA OpenRM + LeandrOS C glue) produced by
//! ports/openrm/build.sh. The archive lives in the shared, gitignored OpenRM
//! cache: $NVRM_LIB_DIR, else $OPENRM_CACHE/build-x86_64, else
//! ~/.cache/leandros/openrm/build-x86_64.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=NVRM_LIB_DIR");
    println!("cargo:rerun-if-env-changed=OPENRM_CACHE");

    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if arch != "x86_64" {
        // OpenRM builds for arm64 on Linux too, but LeandrOS only wires x86_64.
        println!("cargo:warning=nvidia-rm: no libnvrm.a for {arch}; RM symbols left unresolved");
        return;
    }

    let dir = env::var_os("NVRM_LIB_DIR").map(PathBuf::from).unwrap_or_else(|| {
        let cache = env::var_os("OPENRM_CACHE").map(PathBuf::from).unwrap_or_else(|| {
            PathBuf::from(env::var_os("HOME").expect("HOME unset")).join(".cache/leandros/openrm")
        });
        cache.join("build-x86_64")
    });
    let lib = dir.join("libnvrm.a");
    if !lib.exists() {
        panic!(
            "nvidia-rm: {} not found -- run ports/openrm/build.sh (or set NVRM_LIB_DIR)",
            lib.display()
        );
    }
    println!("cargo:rerun-if-changed={}", lib.display());
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=nvrm");
}
