//! The ns-0 alias/bind table now lives in [`crate::mnt::ns0`]; this shim keeps
//! the historical `vfs::alias::*` path for existing callers.
pub use crate::mnt::ns0::*;
