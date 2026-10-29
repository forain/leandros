//! System sleep (ACPI S3) support for the xHCI host controller (lane s3xhci).
//!
//! Suspend (Linux `xhci_suspend`): enabled ports to U3, controller halted,
//! operational and interrupter-0 registers saved, then Controller Save State.
//! Resume tries Controller Restore State; when the controller lost its state
//! (the common case: S3 cuts its power, and QEMU resets every device on wake)
//! it is reset and re-programmed on the SAME controller object, and every
//! device that had a [`Persist`] record is re-addressed IN PLACE under its old
//! slot id (USB persist): port reset, Enable Slot until the old id comes back,
//! Address Device, descriptors compared with the saved copy, Configure
//! Endpoint, SET_CONFIGURATION. The class driver's transport (`XhciBulk`,
//! slot + endpoint numbers) therefore stays valid and the block device, the
//! mounted file system and the root stay where they are. Devices without a
//! record (hubs and everything behind them, HID, audio, video) are marked gone;
//! the hot-plug scan re-enumerates them from the connect-change latch.

use super::*;
use alloc::format;

const CMD_CSS: u32 = 1 << 8;
const CMD_CRS: u32 = 1 << 9;
const STS_SSS: u32 = 1 << 8;
const STS_RSS: u32 = 1 << 9;
const STS_SRE: u32 = 1 << 10;

/// Everything needed to re-address a device after the controller lost it.
#[derive(Default)]
pub(super) struct Persist {
    pub ok: bool,
    pub speed: u8,
    pub config: u8,
    pub alt: u8,
    pub iface: u8,
    pub dd: [u8; 18],
    pub serial: String,
    pub lang: u16,
}

/// Registers saved at suspend.
pub(super) struct Saved {
    dnctrl: u32,
    dcbaap: u64,
    config: u32,
    iman: u32,
    imod: u32,
    erstsz: u32,
    erstba: u64,
    erdp: u64,
    cmd: u32,
    sre: bool,
    u3: Vec<u8>,
}

fn r64(r: &Regs, off: usize) -> u64 { r.op(off) as u64 | (r.op(off + 4) as u64) << 32 }
fn ir64(r: &Regs, off: usize) -> u64 { r.ir(off) as u64 | (r.ir(off + 4) as u64) << 32 }
fn pls(sc: u32) -> u32 { (sc >> PORTSC_PLS_SHIFT) & 0xF }
const WAKE: u32 = PORTSC_WCE | PORTSC_WDE | PORTSC_WOE;

/// Halt one controller and save its state.
pub fn suspend(hc: &Hc) -> Result<String, String> {
    let mut x = hc.lock();
    if x.dead { return Ok(String::from("dead, skipped")); }
    usb_log!("[XHCI] bus {}: S3 suspend: draining events, ports to U3\n", x.bus);
    x.poll_events();
    let r = x.regs;
    // No USB wake in phase 1; enabled ports go to U3 (LWS lets PLS be written).
    let mut u3 = Vec::new();
    for p in 1..=x.max_ports {
        let sc = r.portsc(p);
        let keep = sc & PORTSC_PRESERVE & !WAKE;
        if sc & PORTSC_CCS != 0 && sc & PORTSC_PED != 0 && pls(sc) == 0 {
            r.set_portsc(p, keep | (3 << PORTSC_PLS_SHIFT) | PORTSC_LWS);
            u3.push(p);
        } else {
            r.set_portsc(p, keep);
        }
    }
    for &p in &u3 {
        if !wait_until(10, || pls(r.portsc(p)) == 3) {
            usb_log!("[XHCI] bus {}: port {} did not enter U3 (PORTSC {:#x})\n", x.bus, p, r.portsc(p));
        }
    }
    usb_log!("[XHCI] bus {}: S3 suspend: {} port(s) in U3, halting (<=16 ms)\n", x.bus, u3.len());
    let cmd = r.op(OP_USBCMD);
    r.set_op(OP_USBCMD, cmd & !(CMD_RUN | CMD_INTE));
    if !wait_until(16, || r.op(OP_USBSTS) & STS_HCH != 0) {
        r.set_op(OP_USBCMD, cmd);
        return Err(format!("xHCI bus {} did not halt in 16 ms", x.bus));
    }
    let mut s = Saved {
        dnctrl: r.op(OP_DNCTRL), dcbaap: r64(&r, OP_DCBAAP), config: r.op(OP_CONFIG),
        iman: r.ir(IR_IMAN), imod: r.ir(IR_IMOD), erstsz: r.ir(IR_ERSTSZ),
        erstba: ir64(&r, IR_ERSTBA), erdp: ir64(&r, IR_ERDP), cmd: cmd & !CMD_INTE, sre: false, u3,
    };
    usb_log!("[XHCI] bus {}: S3 suspend: halted, saving state (<=20 ms)\n", x.bus);
    r.set_op(OP_USBCMD, (cmd & !(CMD_RUN | CMD_INTE)) | CMD_CSS);
    if !wait_until(20, || r.op(OP_USBSTS) & STS_SSS == 0) {
        usb_log!("[XHCI] bus {}: Save State did not complete\n", x.bus);
        s.sre = true;
    }
    if r.op(OP_USBSTS) & STS_SRE != 0 {
        usb_log!("[XHCI] bus {}: Save/Restore Error at suspend, will reinit\n", x.bus);
        s.sre = true;
    }
    let n = s.u3.len();
    usb_log!("[XHCI] bus {}: S3 suspend: done (USBSTS {:#x})\n", x.bus, r.op(OP_USBSTS));
    x.pm = Some(Box::new(s));
    Ok(format!("bus {} halted, {} port(s) in U3", x.bus, n))
}

/// Controller Restore State; false = the controller lost its state.
fn try_restore(hc: &Arc<Hc>) -> Result<(), &'static str> {
    {
        let mut x = hc.lock();
        let Some(s) = x.pm.take() else { return Err("nothing saved") };
        if s.sre { return Err("SRE at suspend"); }
        let r = x.regs;
        if r.op(OP_USBSTS) & (STS_HCE | STS_CNR) != 0 || r.op(OP_USBSTS) == !0 { return Err("HCE/CNR set"); }
        // An intact controller still holds DCBAAP; a reset or powered-off one reads 0.
        if r64(&r, OP_DCBAAP) != s.dcbaap || s.dcbaap == 0 { return Err("DCBAAP lost"); }
        r.set_op(OP_DNCTRL, s.dnctrl);
        r.set_op64(OP_DCBAAP, s.dcbaap);
        r.set_op(OP_CONFIG, s.config);
        r.set_ir(IR_ERSTSZ, s.erstsz);
        r.set_ir64(IR_ERDP, s.erdp);
        r.set_ir64(IR_ERSTBA, s.erstba);
        r.set_ir(IR_IMOD, s.imod);
        r.set_ir(IR_IMAN, s.iman);
        r.set_op64(OP_CRCR, x.cmd.dequeue_ptr());
        r.set_op(OP_USBCMD, r.op(OP_USBCMD) | CMD_CRS);
        if !wait_until(10, || r.op(OP_USBSTS) & STS_RSS == 0) { return Err("restore did not finish"); }
        if r.op(OP_USBSTS) & STS_SRE != 0 { return Err("SRE after restore"); }
        r.set_op(OP_USBCMD, s.cmd | CMD_RUN);
        if !wait_until(100, || r.op(OP_USBSTS) & STS_HCH == 0) { return Err("did not start"); }
        for &p in &s.u3 {
            let sc = r.portsc(p);
            if sc & PORTSC_CCS == 0 { return Err("port lost its device"); }
            if pls(sc) == 3 {
                r.set_portsc(p, (sc & PORTSC_PRESERVE & !WAKE) | PORTSC_LWS);
                if !wait_until(20, || pls(r.portsc(p)) == 0) { return Err("port stuck in U3"); }
                sleep_ms(10); // TRSMRCY
            }
            let sc = r.portsc(p);
            if sc & PORTSC_CSC != 0 || sc & PORTSC_CCS == 0 || sc & PORTSC_PED == 0 { return Err("port link changed"); }
            r.set_portsc(p, (sc & PORTSC_PRESERVE) | (sc & PORTSC_CHANGE));
        }
    }
    // The command ring must answer before the restore is trusted.
    if matches!(run_cmd(hc, Trb::new(TRB_NOOP_CMD, 0, 0, 0)), Ok((CC_SUCCESS, _))) { Ok(()) } else { Err("command ring dead") }
}

fn gone(hc: &Hc, id: u8, why: &str) {
    usb_log!("[XHCI] resume: slot {} lost ({})\n", id, why);
    mark_slot_gone(hc, id);
}

/// Re-address `id` in place and check it is still the same device.
fn readdress(hc: &Arc<Hc>, id: u8, port: u8) -> Result<(), &'static str> {
    let (csz, p_speed, p_cfg, p_alt, p_iface, p_dd, p_serial, p_lang) = {
        let x = hc.lock();
        let s = x.slot(id).ok_or("slot vanished")?;
        let p = &s.persist;
        (x.csz, p.speed, p.config, p.alt, p.iface, p.dd, p.serial.clone(), p.lang)
    };
    let in_phys = {
        let mut x = hc.lock();
        let dcbaa_phys = {
            let s = x.slot_mut(id).ok_or("slot vanished")?;
            s.out_ctx.zero(0, s.out_ctx.len());
            s.out_ctx.sync_for_device();
            s.gone = false;
            s.out_ctx.phys()
        };
        x.dcbaa.write64(id as usize * 8, dcbaa_phys);
        crate::dma::dma_wmb();
        let s = x.slot_mut(id).ok_or("slot vanished")?;
        let ep0 = s.eps[1].as_mut().ok_or("no EP0")?;
        ep0.ring.reset();
        ep0.events.clear();
        let (mps, deq) = (ep0.mps, ep0.ring.dequeue_ptr());
        let inp = Ctx::input(&s.in_ctx, csz);
        inp.clear();
        inp.set_add(0b11);
        inp.set_slot(0, slot_dw0(0, p_speed, 1));
        inp.set_slot(1, slot_dw1(port));
        inp.set_slot(2, 0);
        inp.write_ep(1, EP_TYPE_CTRL, mps, 0, 0, 8, 0, deq);
        s.in_ctx.sync_for_device();
        s.in_ctx.phys()
    };
    let (cc, _) = run_cmd(hc, Trb::new(TRB_ADDRESS_DEVICE, in_phys, 0, (id as u32) << 24))
        .map_err(|_| "Address Device timed out")?;
    if cc != CC_SUCCESS { return Err("Address Device failed"); }
    sleep_ms(2);
    let mut dd = [0u8; 18];
    let n = control(hc, id, 0x80, 6, 0x0100, 0, CtrlData::In(&mut dd), ENUM_CTRL_TIMEOUT_MS)
        .map_err(|_| "GET_DESCRIPTOR(device) failed")?;
    if n < 18 || dd != p_dd { return Err("device descriptor changed"); }
    if get_string(hc, id, dd[16], p_lang) != p_serial { return Err("serial number changed"); }
    // Endpoints other than EP0: fresh rings, then Configure Endpoint.
    let in_phys = {
        let mut x = hc.lock();
        let mut mask = 0u32;
        {
            let s = x.slot_mut(id).ok_or("slot vanished")?;
            for dci in 2..32usize {
                if let Some(e) = s.eps[dci].as_mut() {
                    e.ring.reset();
                    e.events.clear();
                    mask |= 1 << dci;
                }
            }
        }
        x.build_configure_input(id, 0, mask).ok_or("slot vanished")?
    };
    let (cc, _) = run_cmd(hc, Trb::new(TRB_CONFIGURE_EP, in_phys, 0, (id as u32) << 24))
        .map_err(|_| "Configure Endpoint timed out")?;
    if cc != CC_SUCCESS { return Err("Configure Endpoint failed"); }
    control(hc, id, 0x00, 9, p_cfg as u16, 0, CtrlData::None, ENUM_CTRL_TIMEOUT_MS)
        .map_err(|_| "SET_CONFIGURATION failed")?;
    if p_alt != 0 {
        control(hc, id, 0x01, 11, p_alt as u16, p_iface as u16, CtrlData::None, ENUM_CTRL_TIMEOUT_MS)
            .map_err(|_| "SET_INTERFACE failed")?;
    }
    Ok(())
}

/// Reset the controller, re-program it and bring the persisted devices back.
fn reinit(hc: &Arc<Hc>) -> Result<String, String> {
    let (ids, bus) = {
        let mut x = hc.lock();
        x.pm = None;
        let r = x.regs;
        if r.op(OP_USBSTS) & STS_HCH == 0 {
            r.set_op(OP_USBCMD, r.op(OP_USBCMD) & !(CMD_RUN | CMD_INTE | CMD_HSEE));
            if !wait_until(100, || r.op(OP_USBSTS) & STS_HCH != 0) {
                x.dead = true;
                return Err(format!("xHCI bus {}: would not halt for the reset", x.bus));
            }
        }
        if !wait_until(1000, || r.op(OP_USBSTS) & STS_CNR == 0) {
            x.dead = true;
            return Err(format!("xHCI bus {}: not ready before the reset", x.bus));
        }
        r.set_op(OP_USBCMD, CMD_HCRST);
        sleep_ms(1);
        if !wait_until(1000, || r.op(OP_USBCMD) & CMD_HCRST == 0 && r.op(OP_USBSTS) & STS_CNR == 0) {
            x.dead = true;
            return Err(format!("xHCI bus {}: reset timed out", x.bus));
        }
        let max_slots = (x.slots.len() - 1) as u8;
        r.set_op(OP_CONFIG, (r.op(OP_CONFIG) & !0xFF) | max_slots as u32);
        x.dcbaa.zero(0, x.dcbaa.len());
        if let Some(a) = x.scratch_array.as_ref() { let p = a.phys(); x.dcbaa.write64(0, p); }
        for p in x.scratch.iter() { p.zero(0, p.len()); }
        x.cmd.reset();
        x.evt.reset();
        x.cmd_done.clear();
        crate::dma::dma_wmb();
        r.set_op64(OP_DCBAAP, x.dcbaa.phys());
        r.set_op64(OP_CRCR, x.cmd.dequeue_ptr());
        r.set_ir(IR_IMAN, IMAN_IP);
        r.set_ir(IR_IMOD, 0);
        r.set_ir(IR_ERSTSZ, 1);
        r.set_ir64(IR_ERDP, x.evt.erdp());
        r.set_ir64(IR_ERSTBA, x.evt.erst_phys());
        r.set_op(OP_DNCTRL, 0);
        r.set_op(OP_USBSTS, STS_HSE | STS_EINT | STS_PCD);
        r.set_op(OP_USBCMD, CMD_RUN);
        if !wait_until(100, || r.op(OP_USBSTS) & STS_HCH == 0) {
            x.dead = true;
            return Err(format!("xHCI bus {}: did not start after the reset", x.bus));
        }
        if r.cap(CAP_HCCPARAMS1) & HCCPARAMS1_PPC != 0 {
            for p in 1..=x.max_ports {
                let sc = r.portsc(p);
                if sc & PORTSC_PP == 0 { r.set_portsc(p, (sc & PORTSC_PRESERVE) | PORTSC_PP); }
            }
        }
        let ids: Vec<u8> = x.slots.iter().enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|_| i as u8)).collect();
        (ids, x.bus)
    };
    sleep_ms(100); // port power-up and debounce (USB 2.0 7.1.7.3)
    // Get the old slot ids back: Enable Slot hands out the lowest free id.
    let maxid = ids.iter().copied().max().unwrap_or(0);
    let stype = {
        let x = hc.lock();
        ids.first().and_then(|&i| x.slot(i)).map(|s| x.port_slot_type[s.port as usize - 1]).unwrap_or(0)
    };
    let mut got = Vec::new();
    for _ in 0..maxid {
        match run_cmd(hc, Trb::new(TRB_ENABLE_SLOT, 0, 0, (stype as u32) << 16)) {
            Ok((CC_SUCCESS, id)) if id != 0 => got.push(id),
            _ => break,
        }
    }
    for &g in &got {
        if !ids.contains(&g) { disable_slot_raw(hc, g); }
    }
    let mut back = 0usize;
    let mut lost = 0usize;
    for &id in &ids {
        let (port, rev, ok) = {
            let x = hc.lock();
            let s = x.slot(id).unwrap();
            (s.port, x.port_rev[s.port as usize - 1], s.persist.ok)
        };
        if !got.contains(&id) { gone(hc, id, "slot id not returned"); lost += 1; continue; }
        if !ok { gone(hc, id, "class not persisted"); lost += 1; continue; }
        let regs = hc.lock().regs;
        let r = reset_port(&regs, port, rev).and_then(|speed| {
            let want = hc.lock().slot(id).map(|s| s.persist.speed).unwrap_or(0);
            if speed != want { Err("speed changed") } else { readdress(hc, id, port) }
        });
        match r {
            Ok(()) => back += 1,
            Err(why) => { gone(hc, id, why); lost += 1; }
        }
    }
    Ok(format!("bus {} reinit: {} device(s) re-addressed in place, {} lost", bus, back, lost))
}

fn disable_slot_raw(hc: &Hc, id: u8) {
    let _ = run_cmd(hc, Trb::new(TRB_DISABLE_SLOT, 0, 0, (id as u32) << 24));
}

/// Resume one controller. `mode`: 0 auto (restore, else reinit), 1 restore
/// only, 2 reinit only.
pub fn resume(hc: &Arc<Hc>, mode: u8) -> Result<String, String> {
    if hc.lock().dead { return Ok(String::from("dead, skipped")); }
    usb_log!("[XHCI] S3 resume: mode {}\n", mode);
    if mode != 2 {
        match try_restore(hc) {
            Ok(()) => return Ok(format!("bus {} restored", hc.lock().bus)),
            Err(why) => {
                if mode == 1 { return Err(format!("bus {} restore failed: {}", hc.lock().bus, why)); }
                usb_log!("[XHCI] bus {}: restore failed ({}), reinit\n", hc.lock().bus, why);
            }
        }
    }
    reinit(hc)
}
