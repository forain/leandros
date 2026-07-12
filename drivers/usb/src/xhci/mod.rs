//! xHCI (eXtensible Host Controller Interface) driver
//!
//! Ported from Linux drivers/usb/host/xhci.c and xhci.h.
//! Implements USB 3.x (SuperSpeed) host control with backward compat for USB 2/1.
//!
//! Register layout mirrors the xHCI 1.2 specification §5.

pub mod ring;
pub mod context;
pub mod regs;

use crate::hcd::{HostControllerDriver, HcdError, HcdState};
use crate::descriptor::{DeviceDescriptor, ConfigDescriptor};
use crate::device::UsbDevice;
use crate::transfer::Urb;
use ring::{Ring, RingType, Trb, TrbType};
use context::DeviceContext;
use regs::XhciRegs;
use mm::buddy;

// ── xHCI constants ────────────────────────────────────────────────────────────

pub const XHCI_MAX_SLOTS:    usize = 256;
pub const XHCI_MAX_PORTS:    usize = 127;
pub const XHCI_MAX_INTRS:    usize = 128;
pub const TRBS_PER_SEGMENT:  usize = 256;
pub const TRB_SEGMENT_SIZE:  usize = TRBS_PER_SEGMENT * 16; // 16 bytes per TRB
pub const TRB_MAX_BUFF_SIZE: usize = 65536;
pub const EP_CTX_PER_DEV:    usize = 31;

// ── Slot state ────────────────────────────────────────────────────────────────

/// Periodic bandwidth budget per microframe (bytes).
/// xHCI spec §4.14.2: Full-/High-speed frames can carry at most 47 250 bytes
/// of periodic traffic per microframe.
const PERIODIC_BW_BUDGET: u32 = 47_250;

#[allow(dead_code)]
struct SlotData {
    device_context: DeviceContext,
    /// Transfer rings, one per endpoint (index = ep context index 1..=30).
    transfer_rings: [Option<Ring>; EP_CTX_PER_DEV + 1],
    /// Total accumulated periodic bandwidth for this slot (bytes/µframe).
    bandwidth_used: u32,
    /// Per-endpoint bandwidth allocation (index = ep context index).
    bandwidth_per_ep: [u32; EP_CTX_PER_DEV + 1],
}

// ── xHCI driver ───────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct Xhci {
    /// MMIO base address of xHCI capability registers.
    mmio_base: usize,
    regs: XhciRegs,
    state: HcdState,

    /// Command ring.
    cmd_ring: Ring,
    /// Event ring (interrupter 0).
    event_ring: Ring,

    /// Slot data indexed by slot ID (1-based; slot 0 unused).
    slots: [Option<SlotData>; XHCI_MAX_SLOTS],

    /// Device Context Base Address Array (DCBAA) physical address.
    /// Points to an array of 64-bit pointers to each slot's output device context.
    dcbaa_phys: u64,

    /// Event Ring Segment Table (ERST) physical address — NOT the event
    /// ring's own TRB array (see the field's constructor comment).
    erst_phys: u64,

    /// Cycle State Bit for the command ring (alternates each pass).
    cmd_ccs: bool,
}

impl Xhci {
    /// Construct an xHCI driver for an HC at `mmio_base`.
    ///
    /// # Safety
    /// `mmio_base` must point to a valid, mapped xHCI MMIO region.
    pub unsafe fn new(mmio_base: usize) -> Self {
        // Allocate one 4 KiB page for the Device Context Base Address Array
        // (DCBAA).  The DCBAA holds 256 × 8-byte pointers (one per slot plus
        // slot 0 which is reserved / scratchpad pointer).  Must be zeroed so
        // all unused slot pointers read as 0.
        let dcbaa_phys: u64 = match buddy::alloc(0) {
            Some(pa) => {
                // `pa` is a physical address; the CPU must write through its
                // HHDM virtual alias, not `pa` itself (see ring.rs doc comment
                // on why this driver can't assume identity mapping).
                (mm::phys_to_virt(pa) as *mut u8).write_bytes(0, buddy::PAGE_SIZE);
                pa as u64
            }
            // If the allocator is not yet initialised (very early probe), leave
            // the pointer at 0; start() will BUG because init_rings() writes
            // it to DCBAAP and the HC will fault on any slot operation.
            None => 0,
        };

        let event_ring = Ring::new(RingType::Event);

        // ERSTBA must point at an Event Ring Segment Table — an array of
        // {segment base addr: u64, segment size: u16, reserved: 6 bytes}
        // entries — NOT at the event ring's own TRB array. Writing the
        // ring's address directly into ERSTBA (the original bug here) means
        // the HC reads the ring's first (zeroed) TRB as a segment table
        // entry: base=0, size=0. Once running, any event the HC tries to
        // post computes an address from that bogus entry and DMA-writes
        // into arbitrary guest physical memory — this silently corrupted
        // unrelated kernel state (traced via bisection to a delayed
        // divide-by-zero panic in mm/src/slab.rs, present only once CMD_RUN
        // was set — reset()/init_rings() alone, before this fix, were safe
        // precisely because the HC hadn't started using ERSTBA yet).
        let erst_phys: u64 = match buddy::alloc(0) {
            Some(pa) => {
                let virt = mm::phys_to_virt(pa) as *mut u8;
                virt.write_bytes(0, buddy::PAGE_SIZE);
                (virt as *mut u64).write_volatile(event_ring.phys_base());
                (virt.add(8) as *mut u16).write_volatile(TRBS_PER_SEGMENT as u16);
                pa as u64
            }
            None => 0,
        };

        Self {
            mmio_base,
            regs: XhciRegs::new(mmio_base),
            state: HcdState::Halt,
            cmd_ring:   Ring::new(RingType::Command),
            event_ring,
            slots: core::array::from_fn(|_| None),
            dcbaa_phys,
            erst_phys,
            cmd_ccs: true,
        }
    }

    /// Maximum number of root-hub ports, from HCSPARAMS1 bits [31:24].
    /// Used by the caller to drive `HubDriver::power_on_ports`/
    /// `handle_port_change` over the right port range (see drivers/src/usb_hcd.rs).
    pub fn max_ports(&self) -> u8 {
        unsafe {
            let hcsparams1 = (self.mmio_base as *const u32)
                .add(regs::CAP_HCSPARAMS1 / 4)
                .read_volatile();
            ((hcsparams1 & regs::HCSPARAMS1_MAX_PORTS_MASK) >> 24) as u8
        }
    }

    // ── Initialisation sequence (xHCI spec §4.2) ─────────────────────────────

    unsafe fn reset(&mut self) -> Result<(), HcdError> {
        use regs::*;
        // Set CMD_RESET; wait for it to clear (HC sets it back to 0 when done).
        let cmd = self.regs.op_read32(OP_USBCMD);
        self.regs.op_write32(OP_USBCMD, cmd | CMD_RESET);
        // Spin until CNR (Controller Not Ready) clears and RESET clears.
        let mut spins = 0usize;
        loop {
            let sts = self.regs.op_read32(OP_USBSTS);
            let cmd2 = self.regs.op_read32(OP_USBCMD);
            if sts & STS_CNR == 0 && cmd2 & CMD_RESET == 0 { break; }
            spins += 1;
            if spins > 1_000_000 { return Err(HcdError::HardwareError); }
            core::hint::spin_loop();
        }
        Ok(())
    }

    unsafe fn init_rings(&mut self) {
        use regs::*;
        // Write DCBAA pointer.
        self.regs.op_write64(OP_DCBAAP, self.dcbaa_phys);

        // Set up command ring: write CRCR with ring ptr + CCS bit.
        let crcr = self.cmd_ring.phys_base() | CMD_RING_CYCLE as u64;
        self.regs.op_write64(OP_CRCR, crcr);

        // Set up event ring segment table for interrupter 0. ERSTBA points at
        // the ERST (see `erst_phys`'s constructor comment) — ERDP points at
        // the ring itself, since it's a direct TRB dequeue pointer, not a
        // segment-table reference.
        let ir = &mut self.regs;
        ir.ir_write32(0, IR_ERSTSZ, 1); // 1-entry ERST
        ir.ir_write64(0, IR_ERSTBA, self.erst_phys);
        ir.ir_write64(0, IR_ERDP, self.event_ring.phys_base());
    }

    // ── Command ring helpers ──────────────────────────────────────────────────

    /// Enqueue a command TRB and ring the command doorbell.
    #[allow(dead_code)]
    unsafe fn send_command(&mut self, trb: Trb) {
        self.cmd_ring.enqueue(trb);
        // Ring HC command doorbell (doorbell 0, target = 0).
        let db_offset = self.regs.db_offset();
        let db_base = self.mmio_base + db_offset as usize;
        (db_base as *mut u32).write_volatile(0);
    }

    /// Issue ENABLE_SLOT and return the allocated slot ID.
    ///
    /// Polls the event ring until a `CmdCompletion` event arrives, then
    /// extracts the slot ID from `control[31:24]` and updates ERDP.
    #[allow(dead_code)]
    unsafe fn enable_slot(&mut self) -> Result<u8, HcdError> {
        let trb = Trb::command(TrbType::EnableSlot, 0, 0, 0);
        self.send_command(trb);

        // Poll event ring for the Command Completion event.
        let mut spins: usize = 0;
        loop {
            if let Some(evt) = self.event_ring.dequeue_event() {
                if evt.trb_type() == TrbType::CmdCompletion as u8 {
                    // Slot ID is in control bits [31:24].
                    let slot_id = (evt.control >> 24) as u8;
                    // Update ERDP so the HC knows we consumed the event.
                    let dp = self.event_ring.deq_phys();
                    self.regs.ir_write64(0, regs::IR_ERDP, dp | regs::ERDP_EHB);
                    if slot_id == 0 {
                        return Err(HcdError::HardwareError);
                    }
                    return Ok(slot_id);
                }
            }
            spins += 1;
            if spins > 1_000_000 { return Err(HcdError::Timeout); }
            core::hint::spin_loop();
        }
    }

    /// Issue a Control-IN transfer on EP0 of `slot`.
    ///
    /// Sends the 8-byte SETUP packet, reads `buf.len()` bytes into `buf` (DATA
    /// stage), then sends the OUT STATUS packet.  Returns `true` on success.
    ///
    /// # Safety
    /// Caller must ensure `slot` is valid and has an EP0 transfer ring.
    unsafe fn ep0_control_in(
        &mut self,
        slot: usize,
        setup: [u8; 8],
        buf:   &mut [u8],
    ) -> bool {
        let buf_phys = buf.as_ptr() as u64;
        let buf_len  = buf.len() as u32;

        let ring = match self.slots[slot]
            .as_mut()
            .and_then(|s| s.transfer_rings[1].as_mut())
        {
            Some(r) => r,
            None    => return false,
        };

        ring.enqueue(Trb::setup(setup, 3, false));  // TT=3: IN data stage
        ring.enqueue(Trb::data(buf_phys, buf_len, true, false));
        ring.enqueue(Trb::status(true, false));     // STATUS OUT

        let db_addr = self.mmio_base
            + self.regs.db_offset() as usize
            + slot * 4;
        (db_addr as *mut u32).write_volatile(1); // EP0 doorbell
        self.wait_transfer_event().is_ok()
    }

    /// Poll the event ring for a `TransferEvent` and update ERDP.
    unsafe fn wait_transfer_event(&mut self) -> Result<u32, HcdError> {
        let mut spins: usize = 0;
        loop {
            if let Some(evt) = self.event_ring.dequeue_event() {
                if evt.trb_type() == TrbType::TransferEvent as u8 {
                    let dp = self.event_ring.deq_phys();
                    self.regs.ir_write64(0, regs::IR_ERDP, dp | regs::ERDP_EHB);
                    // Completion code is in status[31:24]; 1 = success.
                    let cc = (evt.status >> 24) & 0xFF;
                    return if cc == 1 { Ok(evt.status & 0x00FF_FFFF) } else { Err(HcdError::HardwareError) };
                }
            }
            spins += 1;
            if spins > 2_000_000 { return Err(HcdError::Timeout); }
            core::hint::spin_loop();
        }
    }
}

impl HostControllerDriver for Xhci {
    fn start(&mut self) -> Result<(), HcdError> {
        unsafe {
            self.reset()?;
            self.init_rings();

            use regs::*;
            // Run only — no interrupt enable. This driver is entirely
            // polling-based (every wait_transfer_event/enable_slot/etc. spins
            // on dequeue_event()); nothing registers an IRQ handler for this
            // device's line/MSI-X vector. Setting CMD_EIE caused the HC to
            // raise an interrupt this kernel has no handler for, which
            // corrupted unrelated kernel memory (traced via bisection: a
            // divide-by-zero panic in mm/src/slab.rs appearing shortly after
            // boot, present only when CMD_EIE was set — reset()/init_rings()
            // alone are safe).
            let cmd = self.regs.op_read32(OP_USBCMD);
            self.regs.op_write32(OP_USBCMD, cmd | CMD_RUN);
            self.state = HcdState::Running;
        }
        Ok(())
    }

    fn stop(&mut self) {
        unsafe {
            use regs::*;
            let cmd = self.regs.op_read32(OP_USBCMD);
            self.regs.op_write32(OP_USBCMD, cmd & !CMD_RUN);
            self.state = HcdState::Halt;
        }
    }

    fn state(&self) -> HcdState { self.state }

    fn submit_urb(&mut self, urb: Urb) -> Result<(), HcdError> {
        // Derive endpoint context index from the pipe.
        //   EP0:     ctx 1 (bidirectional control)
        //   EPn OUT: ctx 2n
        //   EPn IN:  ctx 2n+1
        let ep_num  = (urb.pipe & 0x0F) as usize;
        let dir_in  = urb.transfer_flags.contains(crate::transfer::TransferFlags::DIR_IN);
        let ep_ctx  = if ep_num == 0 { 1 } else { ep_num * 2 + (dir_in as usize) };

        // Use device address as slot index (simplified: assumes 1:1 mapping).
        let slot = urb.dev_address as usize;
        if slot == 0 || slot >= XHCI_MAX_SLOTS {
            return Err(HcdError::Invalid);
        }

        let ring = self.slots[slot]
            .as_mut()
            .and_then(|s| s.transfer_rings[ep_ctx].as_mut())
            .ok_or(HcdError::Invalid)?;

        ring.enqueue(Trb::normal(
            urb.transfer_buffer_phys,
            urb.transfer_buffer_length,
            true,  // IOC — interrupt on completion
            false, // not chained
            false, // cycle: enqueue() overrides this
        ));

        // Ring the endpoint doorbell: register at db_base + slot*4, value = ep_ctx_idx.
        unsafe {
            let db_addr = self.mmio_base
                + self.regs.db_offset() as usize
                + slot * 4;
            (db_addr as *mut u32).write_volatile(ep_ctx as u32);
        }

        Ok(())
    }

    fn kill_urb(&mut self, urb_context: u64) {
        // urb_context encoding (set by the submit side):
        //   bits [31:8]  = slot ID
        //   bits  [7:0]  = endpoint context index (1–30)
        let slot   = ((urb_context >> 8) & 0xFFFFFF) as usize;
        let ep_ctx = (urb_context & 0xFF) as u32;

        if slot == 0 || slot >= XHCI_MAX_SLOTS || ep_ctx == 0 { return; }

        // STOP_ENDPOINT command (xHCI spec §6.4.3.5):
        //   control[31:24] = slot ID
        //   control[20:16] = endpoint context index
        //   control[23]    = Suspend (TSP) — 0 here
        let ctrl_flags = ((slot as u32) << 24) | (ep_ctx << 16);
        unsafe {
            let trb = Trb::command(TrbType::StopRing, 0, 0, ctrl_flags);
            self.send_command(trb);

            // Poll for the CmdCompletion event and update ERDP.
            let mut spins: usize = 0;
            loop {
                if let Some(evt) = self.event_ring.dequeue_event() {
                    if evt.trb_type() == TrbType::CmdCompletion as u8 {
                        let dp = self.event_ring.deq_phys();
                        self.regs.ir_write64(0, regs::IR_ERDP, dp | regs::ERDP_EHB);
                        break;
                    }
                }
                spins += 1;
                if spins > 1_000_000 { break; }
                core::hint::spin_loop();
            }
        }
    }

    fn get_device_descriptor(&mut self, dev: &UsbDevice) -> Option<DeviceDescriptor> {
        // Build a synchronous control transfer to fetch the 18-byte device descriptor.
        //
        // USB GET_DESCRIPTOR request (device descriptor):
        //   bmRequestType=0x80, bRequest=0x06, wValue=0x0100, wIndex=0, wLength=18
        let setup: [u8; 8] = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0x00];

        // EP0 (bidirectional control) uses context index 1.
        let slot = dev.devnum as usize;
        if slot == 0 || slot >= XHCI_MAX_SLOTS { return None; }

        // Allocate a 4 KiB DMA page for the descriptor buffer. `dma_phys` is
        // what goes into the TRB (hardware needs a real physical address);
        // the CPU reads/writes it through its HHDM virtual alias
        // (`mm::phys_to_virt`) below. Using a dedicated DMA page avoids the
        // stack-buffer-as-DMA-target anti-pattern: the HC would otherwise DMA
        // into whatever a stack virtual address happens to translate to.
        let dma_phys = buddy::alloc(0)?;
        unsafe { (mm::phys_to_virt(dma_phys) as *mut u8).write_bytes(0, 18); }

        let ring = match self.slots[slot]
            .as_mut()
            .and_then(|s| s.transfer_rings[1].as_mut())
        {
            Some(r) => r,
            None    => { buddy::free(dma_phys, 0); return None; }
        };

        // SETUP stage (transfer type 3 = control IN).
        ring.enqueue(Trb::setup(setup, 3, false));
        // DATA stage (IN, 18 bytes into DMA page).
        ring.enqueue(Trb::data(dma_phys as u64, 18, true, false));
        // STATUS stage (OUT).
        ring.enqueue(Trb::status(true, false));
        // ring borrow ends here (NLL — no further use).

        // Ring EP0 doorbell.
        unsafe {
            let db_addr = self.mmio_base
                + self.regs.db_offset() as usize
                + slot * 4;
            (db_addr as *mut u32).write_volatile(1); // EP0 ctx = 1
        }

        // Wait for the transfer event (polls the event ring).
        let ok = unsafe { self.wait_transfer_event().is_ok() };

        // Read the descriptor out of the DMA page before freeing it.
        let result = if ok {
            let buf = unsafe { core::slice::from_raw_parts(mm::phys_to_virt(dma_phys) as *const u8, 18) };
            if buf[0] >= 18 && buf[1] == crate::descriptor::DT_DEVICE {
                Some(DeviceDescriptor {
                    b_length:             buf[0],
                    b_descriptor_type:    buf[1],
                    bcd_usb:              crate::descriptor::Le16(u16::from_le_bytes([buf[2], buf[3]])),
                    b_device_class:       buf[4],
                    b_device_sub_class:   buf[5],
                    b_device_protocol:    buf[6],
                    b_max_packet_size0:   buf[7],
                    id_vendor:            crate::descriptor::Le16(u16::from_le_bytes([buf[8], buf[9]])),
                    id_product:           crate::descriptor::Le16(u16::from_le_bytes([buf[10], buf[11]])),
                    bcd_device:           crate::descriptor::Le16(u16::from_le_bytes([buf[12], buf[13]])),
                    i_manufacturer:       buf[14],
                    i_product:            buf[15],
                    i_serial_number:      buf[16],
                    b_num_configurations: buf[17],
                })
            } else {
                None
            }
        } else {
            None
        };

        buddy::free(dma_phys, 0);
        result
    }

    fn get_config_descriptor(&mut self, dev: &UsbDevice, cfg_idx: u8)
        -> Option<ConfigDescriptor>
    {
        // GET_DESCRIPTOR(config): bmRequestType=0x80, bRequest=0x06,
        // wValue = (DT_CONFIG<<8)|cfg_idx, wIndex=0, wLength=9
        let setup: [u8; 8] = [
            0x80, 0x06, cfg_idx, crate::descriptor::DT_CONFIG,
            0x00, 0x00, 9, 0x00,
        ];

        let slot = dev.devnum as usize;
        if slot == 0 || slot >= XHCI_MAX_SLOTS { return None; }

        // Allocate a 4 KiB DMA page for the descriptor buffer; see the
        // `mm::phys_to_virt` note in `get_device_descriptor` above.
        let dma_phys = buddy::alloc(0)?;
        unsafe { (mm::phys_to_virt(dma_phys) as *mut u8).write_bytes(0, 9); }

        let ring = match self.slots[slot]
            .as_mut()
            .and_then(|s| s.transfer_rings[1].as_mut())
        {
            Some(r) => r,
            None    => { buddy::free(dma_phys, 0); return None; }
        };

        // Control IN transfer: SETUP → DATA (IN) → STATUS (OUT).
        ring.enqueue(Trb::setup(setup, 3, false));
        ring.enqueue(Trb::data(dma_phys as u64, 9, true, false));
        ring.enqueue(Trb::status(true, false));
        // ring borrow ends here (NLL).

        unsafe {
            let db_addr = self.mmio_base
                + self.regs.db_offset() as usize
                + slot * 4;
            (db_addr as *mut u32).write_volatile(1); // EP0 ctx = 1
        }

        let ok = unsafe { self.wait_transfer_event().is_ok() };

        let result = if ok {
            let buf = unsafe { core::slice::from_raw_parts(mm::phys_to_virt(dma_phys) as *const u8, 9) };
            if buf[0] >= 9 && buf[1] == crate::descriptor::DT_CONFIG {
                Some(ConfigDescriptor {
                    b_length:              buf[0],
                    b_descriptor_type:     buf[1],
                    w_total_length:        crate::descriptor::Le16(u16::from_le_bytes([buf[2], buf[3]])),
                    b_num_interfaces:      buf[4],
                    b_configuration_value: buf[5],
                    i_configuration:       buf[6],
                    bm_attributes:         buf[7],
                    b_max_power:           buf[8],
                })
            } else {
                None
            }
        } else {
            None
        };

        buddy::free(dma_phys, 0);
        result
    }

    fn set_address(&mut self, dev: &mut UsbDevice, _address: u8) {
        unsafe {
            // ── 1. Allocate an xHCI slot ──────────────────────────────────────
            let slot_id = match self.enable_slot() {
                Ok(id) => id,
                Err(_) => return,
            };

            // Use slot_id as devnum so all subsequent lookups (submit_urb,
            // get_device_descriptor, etc.) resolve to the right slot entry.
            dev.devnum = slot_id;

            // ── 2. Initialise slot data with an EP0 transfer ring ─────────────
            self.slots[slot_id as usize] = Some(SlotData {
                device_context: context::DeviceContext::default(),
                transfer_rings: core::array::from_fn(|i| {
                    if i == 1 { Some(Ring::new(RingType::Transfer)) } else { None }
                }),
                bandwidth_used:    0,
                bandwidth_per_ep:  [0u32; EP_CTX_PER_DEV + 1],
            });

            // ── 3. Build a proper InputContext ────────────────────────────────
            // Must be populated before ADDRESS_DEVICE so the HC can read slot
            // speed, root-hub port, and the EP0 transfer-ring dequeue pointer.
            let ep0_ring_phys = self.slots[slot_id as usize]
                .as_ref().unwrap()
                .transfer_rings[1].as_ref().unwrap()
                .phys_base();

            // The HC reads the InputContext via DMA, so it needs a page with a
            // real physical address — a stack local's address would be a
            // kernel virtual address the HC cannot dereference (see ring.rs
            // doc comment). Allocate one, write through its HHDM alias, pass
            // the physical address to the command TRB, and free it once the
            // HC has confirmed (CmdCompletion) that it finished reading it.
            let ic_page_phys = buddy::alloc(0).expect("xhci: oom allocating input context");
            let ic_ptr = mm::phys_to_virt(ic_page_phys) as *mut context::InputContext;
            core::ptr::write(ic_ptr, context::InputContext::default());

            // add_flags: bit 0 = slot context, bit 1 = EP0 (context index 1).
            (*ic_ptr).ctrl.add_flags = 0b11;

            // Slot context: High-speed device on root-hub port 1, EP0 only.
            (*ic_ptr).device.slot.dev_info  = context::SlotContext::build_dev_info(
                0,                          // route string (root hub = 0)
                context::SLOT_SPEED_HS,     // assume High-Speed (480 Mb/s)
                false,                      // not a hub
                1,                          // last_ctx = 1 (EP0 only)
            );
            // dev_info2 bits[23:16] = root-hub port number (1-based).
            (*ic_ptr).device.slot.dev_info2 = 1 << 16;

            // EP0 context: bidirectional control, cerr=3, max-packet=64.
            (*ic_ptr).device.ep[0].ep_info2 = context::EndpointContext::build_ep_info2(
                context::EP_TYPE_CTRL, // 4
                3,                     // CErr: 3 retries
                0,                     // max burst = 0
                64,                    // bMaxPacketSize0 for HS EP0
            );
            // Dequeue pointer: ring base, DCS=1 (matches initial PCS=1).
            (*ic_ptr).device.ep[0].deq_lo  = ep0_ring_phys as u32 | 1;
            (*ic_ptr).device.ep[0].deq_hi  = (ep0_ring_phys >> 32) as u32;
            // Average TRB length = 8 (SETUP packet size).
            (*ic_ptr).device.ep[0].tx_info = 8;

            // ── 4. Issue ADDRESS_DEVICE command ───────────────────────────────
            let ic_phys = ic_page_phys as u64;
            let trb = Trb::command(
                TrbType::AddressDevice,
                ic_phys as u32,
                (ic_phys >> 32) as u32,
                (slot_id as u32) << 24,
            );
            self.send_command(trb);

            // Poll for CmdCompletion so we know the HC has finished reading ic
            // before freeing its backing page.
            let mut spins: usize = 0;
            loop {
                if let Some(evt) = self.event_ring.dequeue_event() {
                    if evt.trb_type() == TrbType::CmdCompletion as u8 {
                        let dp = self.event_ring.deq_phys();
                        self.regs.ir_write64(0, regs::IR_ERDP, dp | regs::ERDP_EHB);
                        break;
                    }
                }
                spins += 1;
                if spins > 1_000_000 { break; }
                core::hint::spin_loop();
            }
            buddy::free(ic_page_phys, 0);
        }
    }

    fn set_configuration(&mut self, dev: &mut UsbDevice, config_value: u8) {
        let slot = dev.devnum as usize;
        if slot == 0 || slot >= XHCI_MAX_SLOTS { return; }

        // ── 1. Fetch full configuration descriptor ────────────────────────────
        // First 9 bytes to read wTotalLength.
        let mut hdr = [0u8; 9];
        let ok = unsafe {
            self.ep0_control_in(slot,
                [0x80, 0x06, 0, crate::descriptor::DT_CONFIG, 0, 0, 9, 0],
                &mut hdr)
        };
        if !ok || hdr[1] != crate::descriptor::DT_CONFIG { return; }
        let total = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
        let fetch  = total.min(512);

        // Fetch the full blob (cap at 512 bytes).
        let mut blob = [0u8; 512];
        let ok2 = unsafe {
            self.ep0_control_in(slot,
                [0x80, 0x06, 0, crate::descriptor::DT_CONFIG, 0, 0,
                 (fetch & 0xFF) as u8, (fetch >> 8) as u8],
                &mut blob[..fetch])
        };
        if !ok2 { return; }

        // ── 2. Walk descriptors; allocate a transfer ring per new endpoint ─────
        // Collect (ep_ctx_idx, xhci_type, mps, interval) for each endpoint found.
        // We need up to EP_CTX_PER_DEV = 30 data endpoints (not counting EP0).
        let mut ep_table = [(0usize, 0u8, 0u16, 0u8); 30];
        let mut n_eps     = 0usize;
        let mut last_ctx  = 1u8; // highest ep context index seen

        let mut off = 0usize;
        while off + 2 <= fetch {
            let blen = blob[off] as usize;
            let btyp = blob[off + 1];
            if blen < 2 || off + blen > fetch { break; }

            if btyp == crate::descriptor::DT_ENDPOINT && blen >= 7 {
                let ep_addr  = blob[off + 2];
                let bm_attr  = blob[off + 3];
                let mps      = u16::from_le_bytes([blob[off + 4], blob[off + 5]]) & 0x07FF;
                let interval = blob[off + 6];

                let ep_num   = (ep_addr & 0x0F) as usize;
                let dir_in   = ep_addr & 0x80 != 0;
                let xfer_typ = bm_attr & 0x03;

                // xHCI EP type code.
                let xhci_typ = match (xfer_typ, dir_in) {
                    (0, _)     => context::EP_TYPE_CTRL,
                    (1, false) => context::EP_TYPE_ISOCH_OUT,
                    (1, true)  => context::EP_TYPE_ISOCH_IN,
                    (2, false) => context::EP_TYPE_BULK_OUT,
                    (2, true)  => context::EP_TYPE_BULK_IN,
                    (3, false) => context::EP_TYPE_INTR_OUT,
                    (3, true)  => context::EP_TYPE_INTR_IN,
                    _          => { off += blen; continue; }
                };

                // xHCI context index: EP n OUT = 2n, EP n IN = 2n+1.
                let ctx_idx = if ep_num == 0 { 1 } else { ep_num * 2 + dir_in as usize };
                if ctx_idx > EP_CTX_PER_DEV { off += blen; continue; }

                // Allocate a transfer ring for this endpoint.
                if let Some(sd) = self.slots[slot].as_mut() {
                    if sd.transfer_rings[ctx_idx].is_none() {
                        sd.transfer_rings[ctx_idx] = Some(Ring::new(RingType::Transfer));
                    }
                }

                if n_eps < 30 {
                    ep_table[n_eps] = (ctx_idx, xhci_typ, mps, interval);
                    n_eps += 1;
                }
                if ctx_idx as u8 > last_ctx { last_ctx = ctx_idx as u8; }
            }
            off += blen;
        }

        // ── 3. USB SET_CONFIGURATION control transfer ─────────────────────────
        unsafe {
            let ring = match self.slots[slot]
                .as_mut()
                .and_then(|s| s.transfer_rings[1].as_mut())
            { Some(r) => r, None => return };

            let setup: [u8; 8] = [0x00, 0x09, config_value, 0x00, 0x00, 0x00, 0x00, 0x00];
            ring.enqueue(Trb::setup(setup, 0, false)); // TT=0: no data
            ring.enqueue(Trb::status(false, false));   // STATUS IN

            let db_addr = self.mmio_base + self.regs.db_offset() as usize + slot * 4;
            (db_addr as *mut u32).write_volatile(1);
            let _ = self.wait_transfer_event();
        }

        // ── 4. Build InputContext and issue CONFIGURE_ENDPOINT ────────────────
        // Same DMA-page requirement as set_address's InputContext above: the
        // HC reads this via DMA, so it must live at a real physical address,
        // not a stack virtual address.
        let ic_page_phys = match buddy::alloc(0) {
            Some(p) => p,
            None => return,
        };
        let ic_ptr = mm::phys_to_virt(ic_page_phys) as *mut context::InputContext;
        unsafe { core::ptr::write(ic_ptr, context::InputContext::default()); }

        // Bit 0 = slot context; one bit per ep context index (1-based → bit n).
        let mut add_flags: u32 = 0b01;

        for i in 0..n_eps {
            let (ctx_idx, xhci_typ, mps, interval) = ep_table[i];

            let ring_phys = self.slots[slot]
                .as_ref()
                .and_then(|sd| sd.transfer_rings[ctx_idx].as_ref())
                .map(|r| r.phys_base())
                .unwrap_or(0);

            unsafe {
                let ep = &mut (*ic_ptr).device.ep[ctx_idx - 1];
                ep.ep_info  = (interval as u32) << 16;   // bits[23:16] = Interval
                ep.ep_info2 = context::EndpointContext::build_ep_info2(xhci_typ, 3, 0, mps);
                ep.deq_lo   = ring_phys as u32 | 1;      // DCS = 1
                ep.deq_hi   = (ring_phys >> 32) as u32;
                ep.tx_info  = mps as u32;                 // average TRB length ≈ MPS
            }

            add_flags |= 1 << ctx_idx;
        }

        unsafe {
            (*ic_ptr).ctrl.add_flags  = add_flags;
            (*ic_ptr).device.slot.dev_info = context::SlotContext::build_dev_info(
                0, context::SLOT_SPEED_HS, false, last_ctx,
            );
        }

        let ic_phys = ic_page_phys as u64;

        unsafe {
            let trb = Trb::command(
                TrbType::ConfigureEp,
                ic_phys as u32,
                (ic_phys >> 32) as u32,
                (slot as u32) << 24,
            );
            self.send_command(trb);

            let mut spins: usize = 0;
            loop {
                if let Some(evt) = self.event_ring.dequeue_event() {
                    if evt.trb_type() == TrbType::CmdCompletion as u8 {
                        let dp = self.event_ring.deq_phys();
                        self.regs.ir_write64(0, regs::IR_ERDP, dp | regs::ERDP_EHB);
                        break;
                    }
                }
                spins += 1;
                if spins > 1_000_000 { break; }
                core::hint::spin_loop();
            }
            buddy::free(ic_page_phys, 0);
        }
    }

    fn get_port_status(&mut self, port: u8) -> Option<(u16, u16)> {
        unsafe {
            let offset = regs::OP_PORTSC + (port as usize - 1) * 0x10;
            let portsc = self.regs.op_read32(offset);
            // wPortStatus = bits 15:0, wPortChange = bits 31:16 (in USB hub terms).
            // xHCI PORTSC packs them differently — translate here.
            let status = portsc_to_port_status(portsc);
            let change = portsc_to_port_change(portsc);
            Some((status, change))
        }
    }

    fn port_reset(&mut self, port: u8) {
        unsafe {
            let offset = regs::OP_PORTSC + (port as usize - 1) * 0x10;
            let portsc = self.regs.op_read32(offset);
            // Set PR bit; preserve RW bits, clear RW1C bits.
            self.regs.op_write32(offset, (portsc & regs::PORTSC_RW_MASK) | regs::PORTSC_PR);
        }
    }

    fn set_port_feature(&mut self, port: u8, feature: u16) {
        // USB hub port feature selectors (ch11, Table 11-17).
        // We map to xHCI PORTSC bits; writing any RW1C change bit is a no-op for
        // SetPortFeature — only ClearPortFeature uses RW1C writes.
        use regs::*;

        // Port link state for Suspend (PLS = 3 = U3).
        const PLS_SUSPEND: u32 = 3 << 5;

        let portsc_bit: Option<u32> = match feature {
            1  => Some(PORTSC_PED),  // PORT_FEAT_ENABLE  → enable bit (write 1 re-enables)
            4  => Some(PORTSC_PR),   // PORT_FEAT_RESET   → initiate port reset
            8  => Some(PORTSC_PP),   // PORT_FEAT_POWER   → turn port power on
            22 => Some(PORTSC_PIC),  // PORT_FEAT_INDICATOR → port indicator on
            _  => None,
        };

        // Suspend is set via PLS field, not a single-bit toggle.
        let is_suspend = feature == 2;

        if portsc_bit.is_none() && !is_suspend { return; }

        unsafe {
            let offset = OP_PORTSC + (port as usize - 1) * 0x10;
            let portsc = self.regs.op_read32(offset);
            let rw = portsc & PORTSC_RW_MASK;

            let new_portsc = if is_suspend {
                (rw & !PORTSC_PLS) | PLS_SUSPEND | PORTSC_LWS
            } else {
                rw | portsc_bit.unwrap()
            };
            self.regs.op_write32(offset, new_portsc);
        }
    }

    fn clear_port_feature(&mut self, port: u8, feature: u16) {
        use regs::*;

        // RW1C change bits — write 1 to clear the status-change flag.
        let rw1c_bit: Option<u32> = match feature {
            16 => Some(PORTSC_CSC), // PORT_FEAT_C_CONNECTION
            17 => Some(PORTSC_PEC), // PORT_FEAT_C_ENABLE
            18 => Some(PORTSC_PLC), // PORT_FEAT_C_SUSPEND (link-state change)
            19 => Some(PORTSC_OCC), // PORT_FEAT_C_OVER_CURRENT
            20 => Some(PORTSC_PRC), // PORT_FEAT_C_RESET
            _  => None,
        };

        // Regular read-write bits to clear.
        let rw_clear_bit: Option<u32> = match feature {
            1  => Some(PORTSC_PED), // PORT_FEAT_ENABLE → disable port
            8  => Some(PORTSC_PP),  // PORT_FEAT_POWER  → turn port power off
            22 => Some(PORTSC_PIC), // PORT_FEAT_INDICATOR → indicator off
            _  => None,
        };

        // Suspend (feature 2): clear via PLS = U0.
        let is_unsuspend = feature == 2;

        if rw1c_bit.is_none() && rw_clear_bit.is_none() && !is_unsuspend { return; }

        unsafe {
            let offset = OP_PORTSC + (port as usize - 1) * 0x10;
            let portsc  = self.regs.op_read32(offset);
            let rw      = portsc & PORTSC_RW_MASK;

            let new_portsc = if let Some(bit) = rw1c_bit {
                // Write 1 to the RW1C bit; preserve RW fields; don't touch other RW1C.
                rw | bit
            } else if let Some(bit) = rw_clear_bit {
                rw & !bit
            } else {
                // Unsuspend: drive PLS to U0 with LWS strobe.
                (rw & !PORTSC_PLS) | PORTSC_LWS // PLS = 0 = U0
            };
            self.regs.op_write32(offset, new_portsc);
        }
    }

    fn alloc_bandwidth(&mut self, dev: &UsbDevice, ep_address: u8, bpf: u32)
        -> Result<u32, HcdError>
    {
        let slot    = dev.devnum as usize;
        if slot == 0 || slot >= XHCI_MAX_SLOTS { return Err(HcdError::Invalid); }

        // Derive context index from endpoint address (same mapping as submit_urb).
        let ep_num   = (ep_address & 0x0F) as usize;
        let dir_in   = ep_address & 0x80 != 0;
        let ep_ctx   = if ep_num == 0 { 1 } else { ep_num * 2 + dir_in as usize };
        if ep_ctx > EP_CTX_PER_DEV { return Err(HcdError::Invalid); }

        let sd = self.slots[slot].as_mut().ok_or(HcdError::Invalid)?;

        // Reject if adding would breach the periodic budget.
        let new_total = sd.bandwidth_used.saturating_add(bpf);
        if new_total > PERIODIC_BW_BUDGET { return Err(HcdError::Invalid); }

        sd.bandwidth_per_ep[ep_ctx]  = bpf;
        sd.bandwidth_used            = new_total;
        Ok(bpf)
    }

    fn free_bandwidth(&mut self, dev: &UsbDevice, ep_address: u8) {
        let slot = dev.devnum as usize;
        if slot == 0 || slot >= XHCI_MAX_SLOTS { return; }

        let ep_num = (ep_address & 0x0F) as usize;
        let dir_in = ep_address & 0x80 != 0;
        let ep_ctx = if ep_num == 0 { 1 } else { ep_num * 2 + dir_in as usize };
        if ep_ctx > EP_CTX_PER_DEV { return; }

        if let Some(sd) = self.slots[slot].as_mut() {
            let freed = sd.bandwidth_per_ep[ep_ctx];
            sd.bandwidth_used        = sd.bandwidth_used.saturating_sub(freed);
            sd.bandwidth_per_ep[ep_ctx] = 0;
        }
    }
}

// ── PORTSC translation helpers ────────────────────────────────────────────────

fn portsc_to_port_status(portsc: u32) -> u16 {
    use crate::descriptor::{
        PORT_STATUS_CONNECTION, PORT_STATUS_ENABLE,
        PORT_STATUS_RESET, PORT_STATUS_POWER,
    };
    use regs::*;
    let mut s: u16 = 0;
    if portsc & PORTSC_CCS  != 0 { s |= PORT_STATUS_CONNECTION; }
    if portsc & PORTSC_PED  != 0 { s |= PORT_STATUS_ENABLE; }
    if portsc & PORTSC_PR   != 0 { s |= PORT_STATUS_RESET; }
    if portsc & PORTSC_PP   != 0 { s |= PORT_STATUS_POWER; }
    s
}

fn portsc_to_port_change(portsc: u32) -> u16 {
    use crate::descriptor::{
        PORT_CHANGE_CONNECTION, PORT_CHANGE_RESET,
    };
    use regs::*;
    let mut c: u16 = 0;
    if portsc & PORTSC_CSC  != 0 { c |= PORT_CHANGE_CONNECTION; }
    if portsc & PORTSC_PRC  != 0 { c |= PORT_CHANGE_RESET; }
    c
}
