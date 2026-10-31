//! LeandrOS installer library. See README.md for the CLI and the progress
//! line format.
#![allow(dead_code)]

pub mod mkhome;

// Shared with userland/espctl (no fork): block cache, partition scan, limine.conf.
#[path = "../../espctl/src/conf.rs"]
pub mod conf;
#[path = "../../espctl/src/dev.rs"]
pub mod dev;
#[path = "../../espctl/src/disks.rs"]
pub mod disks;

pub mod copy;
pub mod dualboot;
pub mod esp;
pub mod gpt;
pub mod install;
pub mod plan;
pub mod progress;
pub mod sbguard;
pub mod shrink;
