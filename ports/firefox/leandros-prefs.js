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
