//! C math entry points used by the SoundFont synthesizer.
//! Pure Rust libm keeps these available on both freestanding targets.
//!
//! x86_64: this crate is built for `x86_64-unknown-none`, whose Rust ABI is
//! soft-float (f64 travels in rdi/rsi/rax), while the C callers (DOOM's TSF
//! synth, built by clang for the same triple) use the SysV hard-float ABI
//! (xmm0/xmm1 in, xmm0 out). Calling the Rust functions directly made every
//! `pow`/`exp`/`sqrt` read a stale integer register and return garbage in rax,
//! so the synth's gains were random and DOOM's music clipped on x86_64 only.
//! In that soft-float build the exported C symbols are small asm trampolines
//! that move the arguments from SSE to integer registers and the result back.
//! The archive C programs actually link on x86_64 is built for the hard-float
//! leandros target (`target_feature = "sse2"`, build-userland.sh), which fixes
//! the whole class (compiler-rt float builtins included) and uses the plain
//! functions below; the trampolines only guard the soft-float archive.

#[cfg(not(all(target_arch = "x86_64", not(target_feature = "sse2"))))]
#[no_mangle]
pub extern "C" fn pow(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}

#[cfg(not(all(target_arch = "x86_64", not(target_feature = "sse2"))))]
macro_rules! unary {
    ($($name:ident),+ $(,)?) => {
        $(
            #[no_mangle]
            pub extern "C" fn $name(x: f64) -> f64 {
                libm::$name(x)
            }
        )+
    };
}

#[cfg(not(all(target_arch = "x86_64", not(target_feature = "sse2"))))]
unary!(exp, log, log10, tan, sqrt);

#[cfg(all(target_arch = "x86_64", not(target_feature = "sse2")))]
mod x86 {
    // Soft-float ABI bodies; only the trampolines below call them.
    #[no_mangle]
    extern "C" fn __leandros_sf_pow(x: f64, y: f64) -> f64 {
        libm::pow(x, y)
    }

    macro_rules! unary_sf {
        ($($sf:ident => $name:ident),+ $(,)?) => {
            $(
                #[no_mangle]
                extern "C" fn $sf(x: f64) -> f64 {
                    libm::$name(x)
                }
            )+
        };
    }
    unary_sf!(
        __leandros_sf_exp => exp,
        __leandros_sf_log => log,
        __leandros_sf_log10 => log10,
        __leandros_sf_tan => tan,
        __leandros_sf_sqrt => sqrt,
    );

    macro_rules! trampoline {
        ($name:literal, $sf:literal) => {
            core::arch::global_asm!(
                concat!(".globl ", $name),
                concat!(".type ", $name, ",@function"),
                concat!(".section .text.", $name, ",\"ax\",@progbits"),
                concat!($name, ":"),
                "sub rsp, 8",
                "movq rdi, xmm0",
                "movq rsi, xmm1",
                concat!("call ", $sf),
                "movq xmm0, rax",
                "add rsp, 8",
                "ret",
                concat!(".size ", $name, ", . - ", $name),
                ".text",
            );
        };
    }
    trampoline!("pow", "__leandros_sf_pow");
    trampoline!("exp", "__leandros_sf_exp");
    trampoline!("log", "__leandros_sf_log");
    trampoline!("log10", "__leandros_sf_log10");
    trampoline!("tan", "__leandros_sf_tan");
    trampoline!("sqrt", "__leandros_sf_sqrt");
}
