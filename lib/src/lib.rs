//! Shared kernel library — types, traits, and utilities used across crates.

#![no_std]

pub mod ring_buffer;
pub mod bitmap;

/// Align `value` up to the nearest multiple of `align` (must be power of two).
#[inline(always)]
pub const fn align_up(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

/// Align `value` down to the nearest multiple of `align`.
#[inline(always)]
pub const fn align_down(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    value & !(align - 1)
}
