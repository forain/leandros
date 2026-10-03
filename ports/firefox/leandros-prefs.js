// LeandrOS default prefs for Alpine's Firefox (ports/firefox). Staged by
// scripts/mkfs-f2fs-populated.py as /usr/lib/firefox/defaults/pref/leandros-prefs.js,
// next to Alpine's channel-prefs.js; Firefox reads every .js file there as a
// default pref, so a profile can still override any of these.

// GPU rendering only: hardware WebRender on our Mesa (zink or virgl), forced
// past the GPU blocklist, with the software-WebRender fallback disabled.
pref("gfx.webrender.all", true);
pref("gfx.webrender.software", false);
pref("gfx.webrender.fallback.software", false);
pref("layers.acceleration.force-enabled", true);
pref("media.hardware-video-decoding.enabled", false);
pref("media.ffmpeg.vaapi.enabled", false);

// No seccomp or namespaces in the kernel (see /bin/firefox).
pref("security.sandbox.content.level", 0);

// JIT code W^X in content processes too. The kernel refuses a mapping that is
// writable and executable at once, and with this off a content process's JIT
// commits its code pages RWX, fails, and dies at startup in MOZ_CRASH(OOM).
// The parent process always writes code W^X.
pref("javascript.options.content_process_write_protect_code", true);

// Bring-up: as few processes as Firefox allows.
pref("fission.autostart", false);
pref("dom.ipc.processCount", 1);
pref("dom.ipc.processPrelaunch.enabled", false);

// Nothing on startup that needs the network or a service we do not have.
pref("browser.startup.page", 0);
pref("browser.startup.homepage_override.mstone", "ignore");
pref("browser.aboutwelcome.enabled", false);
pref("browser.shell.checkDefaultBrowser", false);
pref("browser.sessionstore.resume_from_crash", false);
pref("app.update.auto", false);
pref("app.update.enabled", false);
pref("datareporting.policy.dataSubmissionEnabled", false);
pref("datareporting.healthreport.uploadEnabled", false);
pref("toolkit.telemetry.enabled", false);
pref("network.captive-portal-service.enabled", false);
pref("network.connectivity-service.enabled", false);
pref("toolkit.startup.max_resumed_crashes", -1);

// Media plugins (GMP). Firefox downloads Cisco's OpenH264 (and, for DRM
// sites, Google's Widevine CDM) at runtime. Both are glibc builds (DT_NEEDED
// libc.so.6, libpthread.so.0, ld-linux-*.so.1) that a musl process cannot
// dlopen, so the GMP child dies in GMPLoader's
// MOZ_CRASH("Cannot load plugin as library") and the page shows "The
// gmpopenh264 plugin crashed". Never download or load them: H.264/AAC decode
// through the system FFmpeg (libavcodec, staged by dlopen-in-alpine.sh) like
// every other codec.
pref("media.gmp-gmpopenh264.enabled", false);
pref("media.gmp-gmpopenh264.autoupdate", false);
pref("media.gmp-gmpopenh264.visible", false);
pref("media.gmp-widevinecdm.enabled", false);
pref("media.gmp-widevinecdm.autoupdate", false);
pref("media.gmp-widevinecdm.visible", false);
