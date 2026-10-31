//! kapi exports: RP1 south-bridge interrupt sources (kmodpi M1). RP1 source `n` (Linux `rp1.h`
//! numbering) is wired through MSI-X -> MIP0 -> GIC by `drivers::rp1`. Boards without an RP1
//! answer -19. Handles are kept here so a module can free its source again.
use super::*;
use kmod_abi::KIrqHandler;

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
static HELD: spin::Mutex<alloc::vec::Vec<(u16, u32, drivers::irq::IrqHandle)>> = spin::Mutex::new(alloc::vec::Vec::new());

/// Attach `h(ctx)` to RP1 source `n`; `level` re-arms the source after each run. 0 ok, -16 busy,
/// -19 no RP1, -22 bad arguments, -95 unsupported source.
#[no_mangle]
pub extern "C" fn kapi_rp1_irq_request(owner: KmodId, n: u32, level: bool, h: KIrqHandler, ctx: usize) -> i32 {
    if owner.0 == 0 || n >= 64 { return -22; }
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    {
        return match drivers::rp1::request_irq(n, level, irq_handler(h), ctx) {
            Ok(hd) => {
                HELD.lock().push((owner.0, n, hd));
                crate::kmod::registry::note_registration(owner, RegKind::IRQ, n as u64);
                0
            }
            Err(e) => irq_errno(e) as i32,
        };
    }
    #[allow(unreachable_code)]
    { let _ = (level, h, ctx); -19 }
}

/// Detach source `n` that `owner` attached. 0 ok, -19 when it holds no such source.
#[no_mangle]
pub extern "C" fn kapi_rp1_irq_free(owner: KmodId, n: u32) -> i32 {
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    {
        let mut g = HELD.lock();
        let Some(i) = g.iter().position(|e| e.0 == owner.0 && e.1 == n) else { return -19 };
        let (_, _, hd) = g.remove(i);
        drop(g);
        drivers::rp1::free_irq(n, hd);
        return 0;
    }
    #[allow(unreachable_code)]
    { let _ = (owner, n); -19 }
}
