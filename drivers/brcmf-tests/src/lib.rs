//! Host tests for the brcmf chip/protocol layer (lane pibrcmf-core).
extern crate alloc;

#[allow(dead_code)]
pub mod brcmf;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
