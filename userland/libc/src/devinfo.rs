//! Device-enumeration syscall wrappers backing lsblk/lspci/lsusb.
//! No POSIX equivalent — mirrors the fixed-layout structs the kernel writes
//! in kernel/src/syscall.rs (sys_blkdev_info/sys_pcidev_info/sys_usbdev_info).

use crate::syscall::{nr, syscall1, syscall2};

#[derive(Debug, Clone, Copy, Default)]
pub struct BlkDevInfo {
    pub total_blocks: u64,
    pub block_size: u32,
    pub fstype: Option<[u8; 8]>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PciDevInfo {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UsbDevInfo {
    pub bus: u8,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub class: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct MountInfo {
    pub mountpoint: [u8; 32],
    pub mountpoint_len: usize,
    pub device: [u8; 16],
    pub device_len: usize,
    pub fstype: [u8; 8],
    pub fstype_len: usize,
}

impl MountInfo {
    pub fn mountpoint(&self) -> &[u8] { &self.mountpoint[..self.mountpoint_len] }
    pub fn device(&self) -> &[u8] { &self.device[..self.device_len] }
    pub fn fstype(&self) -> &[u8] { &self.fstype[..self.fstype_len] }
}

pub unsafe fn blkdev_count() -> isize {
    syscall1(nr::BLKDEV_COUNT, 0)
}

pub unsafe fn blkdev_info(index: usize) -> Option<BlkDevInfo> {
    let mut buf = [0u8; 24];
    let r = syscall2(nr::BLKDEV_INFO, index, buf.as_mut_ptr() as usize);
    if r < 0 { return None; }
    let total_blocks = u64::from_ne_bytes(buf[0..8].try_into().unwrap());
    let block_size = u32::from_ne_bytes(buf[8..12].try_into().unwrap());
    let has_fstype = buf[12] != 0;
    let mut name = [0u8; 8];
    name.copy_from_slice(&buf[13..21]);
    Some(BlkDevInfo { total_blocks, block_size, fstype: if has_fstype { Some(name) } else { None } })
}

pub unsafe fn pcidev_count() -> isize {
    syscall1(nr::PCIDEV_COUNT, 0)
}

pub unsafe fn pcidev_info(index: usize) -> Option<PciDevInfo> {
    let mut buf = [0u8; 12];
    let r = syscall2(nr::PCIDEV_INFO, index, buf.as_mut_ptr() as usize);
    if r < 0 { return None; }
    Some(PciDevInfo {
        bus: buf[0], dev: buf[1], func: buf[2],
        vendor_id: u16::from_ne_bytes(buf[4..6].try_into().unwrap()),
        device_id: u16::from_ne_bytes(buf[6..8].try_into().unwrap()),
        class: buf[8], subclass: buf[9], prog_if: buf[10],
    })
}

pub unsafe fn usbdev_count() -> isize {
    syscall1(nr::USBDEV_COUNT, 0)
}

pub unsafe fn usbdev_info(index: usize) -> Option<UsbDevInfo> {
    let mut buf = [0u8; 12];
    let r = syscall2(nr::USBDEV_INFO, index, buf.as_mut_ptr() as usize);
    if r < 0 { return None; }
    Some(UsbDevInfo {
        bus: buf[0], address: buf[1],
        vendor_id: u16::from_ne_bytes(buf[4..6].try_into().unwrap()),
        product_id: u16::from_ne_bytes(buf[6..8].try_into().unwrap()),
        class: buf[8],
    })
}

fn nul_len(buf: &[u8]) -> usize {
    buf.iter().position(|&b| b == 0).unwrap_or(buf.len())
}

pub unsafe fn mounts_count() -> isize {
    syscall1(nr::MOUNTS_COUNT, 0)
}

pub unsafe fn mounts_info(index: usize) -> Option<MountInfo> {
    let mut buf = [0u8; 56];
    let r = syscall2(nr::MOUNTS_INFO, index, buf.as_mut_ptr() as usize);
    if r < 0 { return None; }
    let mut mountpoint = [0u8; 32];
    mountpoint.copy_from_slice(&buf[0..32]);
    let mut device = [0u8; 16];
    device.copy_from_slice(&buf[32..48]);
    let mut fstype = [0u8; 8];
    fstype.copy_from_slice(&buf[48..56]);
    Some(MountInfo {
        mountpoint_len: nul_len(&mountpoint), mountpoint,
        device_len: nul_len(&device), device,
        fstype_len: nul_len(&fstype), fstype,
    })
}
