// Shared build script of the no_std programs linked against static musl
// (<arch>-unknown-linux-musl, zig linker wrapper, see scripts/build-userland.sh).
// rustc passes -nodefaultlibs for no_std crates, so libc is requested here, and
// the stack size comes from PT_GNU_STACK (musl's default thread stack is only
// 128 KiB).
fn main() {
    println!("cargo:rustc-link-lib=c");
    println!("cargo:rustc-link-arg=-Wl,-z,stack-size=1048576");
}
