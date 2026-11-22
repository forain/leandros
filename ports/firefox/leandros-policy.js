// LeandrOS distro policy for Alpine's Firefox (ports/firefox). Staged by
// scripts/mkfs-f2fs-populated.py as /usr/lib/firefox/defaults/pref/leandros-policy.js.
// Everything else (WebRender, Fission, VA-API, process counts) is Firefox's
// stock behaviour.

// Updates are the package manager's job; telemetry and health reports are off.
pref("app.update.auto", false);
pref("app.update.enabled", false);
pref("datareporting.policy.dataSubmissionEnabled", false);
pref("datareporting.healthreport.uploadEnabled", false);
pref("toolkit.telemetry.enabled", false);
pref("toolkit.telemetry.unified", false);

// THE ONE REMAINING OVERRIDE (kernel W^X policy, decided by design): JIT code is
// written W^X in content processes too. The LeandrOS kernel refuses a mapping
// that is writable and executable at once (kernel/src/syscall.rs, "W^X
// enforcement"), deliberately stricter than Linux, which allows RWX. With this
// pref off, a content process's JIT commits its code pages RWX, the
// mmap/mprotect fails, and the process dies at startup in MOZ_CRASH(OOM).
pref("javascript.options.content_process_write_protect_code", true);
