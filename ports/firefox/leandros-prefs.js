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

// Fewer disk writes (lane quietio). Root is F2FS on a USB 2.0 stick where a
// write command costs up to 1 s and stalls the whole system, so the profile
// should write as little as possible.
// Cache in RAM only; no disk cache.
pref("browser.cache.disk.enable", false);
pref("browser.cache.disk.smart_size.enabled", false);
pref("browser.cache.memory.enable", true);
pref("browser.cache.memory.capacity", 262144); // KB
// Session store: snapshot every 5 minutes instead of 15 s.
pref("browser.sessionstore.interval", 300000);
// No global SQLite sync pref: toolkit.storage.synchronous no longer exists in
// Firefox 136 (the string is absent from libxul), so it is not set here. Each
// store picks its own mode: IndexedDB runs WAL with synchronous=NORMAL
// (dom.indexedDB.fullSynchronous stays false, its default), so a commit does
// not fsync; only WAL checkpoints and Blob files do.
// No Safe Browsing list downloads (they write tens of MB of list files).
pref("browser.safebrowsing.malware.enabled", false);
pref("browser.safebrowsing.phishing.enabled", false);
pref("browser.safebrowsing.downloads.enabled", false);
pref("browser.safebrowsing.downloads.remote.enabled", false);
pref("browser.safebrowsing.blockedURIs.enabled", false);
pref("browser.safebrowsing.provider.google.updateURL", "");
pref("browser.safebrowsing.provider.google4.updateURL", "");
pref("browser.safebrowsing.provider.mozilla.updateURL", "");
pref("browser.safebrowsing.provider.google.gethashURL", "");
pref("browser.safebrowsing.provider.google4.gethashURL", "");
pref("browser.safebrowsing.provider.mozilla.gethashURL", "");
pref("privacy.trackingprotection.enabled", false);
pref("privacy.trackingprotection.pbmode.enabled", false);
pref("privacy.trackingprotection.annotate_channels", false);
pref("privacy.trackingprotection.socialtracking.enabled", false);
pref("urlclassifier.trackingTable", "");
pref("urlclassifier.trackingAnnotationTable", "");
pref("browser.urlbar.quicksuggest.enabled", false);
// Telemetry, health report, crash reports, studies: off.
pref("toolkit.telemetry.unified", false);
pref("toolkit.telemetry.archive.enabled", false);
pref("toolkit.telemetry.newProfilePing.enabled", false);
pref("toolkit.telemetry.shutdownPingSender.enabled", false);
pref("toolkit.telemetry.updatePing.enabled", false);
pref("toolkit.telemetry.bhrPing.enabled", false);
pref("toolkit.telemetry.firstShutdownPing.enabled", false);
pref("browser.ping-centre.telemetry", false);
pref("browser.newtabpage.activity-stream.feeds.telemetry", false);
pref("browser.newtabpage.activity-stream.telemetry", false);
pref("breakpad.reportURL", "");
pref("browser.tabs.crashReporting.sendReport", false);
pref("app.shield.optoutstudies.enabled", false);
pref("app.normandy.enabled", false);
// Background network and profile churn.
pref("browser.places.speculativeConnect.enabled", false);
pref("network.prefetch-next", false);
pref("network.dns.disablePrefetch", true);
pref("network.predictor.enabled", false);
pref("extensions.update.enabled", false);
pref("extensions.getAddons.cache.enabled", false);
pref("browser.newtabpage.activity-stream.feeds.section.topstories", false);
pref("browser.newtabpage.activity-stream.feeds.topsites", false);
pref("browser.topsites.contile.enabled", false);
pref("browser.search.update", false);
pref("media.gmp-manager.url", "");

// Remote Settings (lane ffchurn). services.settings.server is NOT set: on
// release builds Utils.sys.mjs ignores any override of it ("Ignoring
// preference override of remote settings server") unless
// MOZ_REMOTE_SETTINGS_DEVTOOLS=1, so an empty value never turned Remote
// Settings off. It syncs with the production server once a day and loads its
// packaged dumps as usual.
//
// Intermediate CA preloading off. The packaged security-state/intermediates
// dump has 1771 records, each with a certificate attachment. On a new profile,
// the first poll fetches every attachment, either as one zip bundle
// (Attachments.cacheAll) or one at a time (download), and caches each as a
// Blob in the remote-settings IndexedDB (storage/permanent/chrome/idb/
// 3870112724rsegmnoittet-es.files/N). IndexedDB writes and fsyncs one file per
// Blob: about 2000 files (4568 fsyncs vs 623 without it, in the first 6 min of a
// new profile on aarch64 QEMU). QEMU finishes that in 20 s; on the
// USB stick each fsync runs an F2FS checkpoint that takes seconds, so it
// trickles for hours and picks up again at the next poll (PC boot17: files/1..380, one
// every ~5 s). The certificates only help TLS servers that send an incomplete
// chain; correctly configured sites (YouTube, Google, ...) do not need them.
pref("security.remote_settings.intermediates.enabled", false);
