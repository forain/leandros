//! C math entry points used by the SoundFont synthesizer.
//! Pure Rust libm keeps these available on both freestanding targets.

#[no_mangle]
pub extern "C" fn pow(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}

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

unary!(exp, log, log10, tan, sqrt);
