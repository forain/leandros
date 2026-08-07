// Host-side link harness: forces the full cosmic_pipewire::run reference chain so
// every pw_*/spa_* symbol the daemon uses is retained (not GC'd), proving the stub
// libpipewire-0.3.so.0 actually resolves the pipewire closure (DT_NEEDED present).
fn main() {
    cosmic_pipewire::run(|_event| {}, |_sender| {});
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::process::exit(0);
}
