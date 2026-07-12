fn main() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let relibc_target = match arch.as_str() {
        "aarch64" => "aarch64-unknown-leandros",
        "x86_64" => "x86_64-unknown-leandros",
        _ => return,
    };
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search=native={}/../../../relibc/target/{}/release", manifest_dir, relibc_target);
    println!("cargo:rustc-link-lib=static=relibc");
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=muldefs");
}
