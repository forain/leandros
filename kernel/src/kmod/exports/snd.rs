//! kapi exports: sound backend
use super::*;

/// The attached module backend (cleared again by `snd_release` on unload).
static MOD_SND: AtomicUsize = AtomicUsize::new(0);

/// Attach a sound backend from a module. The pipewire server adopts it as its `Backend::Module`,
/// registers `/dev/pipewire` on first attach when no built-in device did, and the audio port is
/// published. Returns 0, -22 (null ops), -16 (a backend is already attached) or -12.
#[no_mangle]
pub extern "C" fn kapi_snd_register(owner: KmodId, ops: *const KSndOps, ctx: usize) -> i32 {
    if ops.is_null() { return -22; }
    if MOD_SND.load(Ordering::Acquire) != 0 { return -16; }
    let ops: &'static KSndOps = Box::leak(Box::new(unsafe { *ops }));
    match pipewire_server::attach(ops, ctx) {
        Ok(port) => {
            MOD_SND.store(ops as *const KSndOps as usize, Ordering::Release);
            crate::syscall::set_audio_server_port(port);
            crate::kmod::registry::note_registration(owner, RegKind::SND, 0);
            0
        }
        Err(e) => e,
    }
}

/// Unload: drop the module's backend from the pipewire server (idempotent; runs before the module's exit).
pub fn snd_release() -> bool {
    pipewire_server::detach_module();
    MOD_SND.store(0, Ordering::Release);
    true
}

/// D6: an open sound fd (writer, capture reader or non-blocking open) pins the module like video.
pub fn snd_busy() -> bool { pipewire_server::module_busy() }

/// Unload: stop new opens and re-check busy (see `pipewire_server::seal_module`). False = a client holds it.
pub fn snd_seal() -> bool { pipewire_server::seal_module() }
pub fn snd_unseal() { pipewire_server::unseal_module() }
