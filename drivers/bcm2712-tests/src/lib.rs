//! Host tests for the BCM2712 GPIO/pinctrl/l2 logic and the OTP helpers. The
//! driver sources are included verbatim; their `rpi5` halves stay cfg'd out.
#![allow(dead_code)]

#[path = "../../src/bcm2712_id.rs"]
pub mod bcm2712_id;
#[path = "../../src/bcm2712_gpio.rs"]
pub mod bcm2712_gpio;
#[path = "../../src/bcm2712_otp.rs"]
pub mod bcm2712_otp;

#[cfg(test)]
mod tests;
