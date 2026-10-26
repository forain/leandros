//! V3D register access seam. Every piece of driver logic goes through this trait so it can be
//! exercised on the host against a mock register file.

pub trait V3dRegs { fn hub_r(&self, off: u32) -> u32; fn hub_w(&self, off: u32, v: u32);
                    fn core_r(&self, off: u32) -> u32; fn core_w(&self, off: u32, v: u32);
                    fn sms_r(&self, off: u32) -> u32;  fn sms_w(&self, off: u32, v: u32); }
