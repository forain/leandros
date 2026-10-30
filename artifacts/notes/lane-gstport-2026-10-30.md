# lane/gstport 2026-10-30 (PARTIAL, HANDOFF)

Stage 1 DONE: ports/gstreamer (Alpine 3.21 gstreamer 1.24.11, plugins-base/good, gst-libav, gst-plugin-pipewire 1.2.7 = our PipeWire).
Image +46 MiB aarch64 (staged tree 119 MiB; glib/ffmpeg shared with firefox/pipewire trees). Sysroot -dev merged (gst*, glib, gio, util-linux mount/blkid, pcre2, intl, ssp).
Stage 2 DONE: gst-inspect 53 plugins/1007 features; videotestsrc, mkv demux + avdec_mpeg4 + vorbisdec chains OK; audio-only pipeline -> pipewiresink streams into leandros_output (cycles=424/7 s, underruns 0).
Stage 3 PARTIAL: cosmic-player (epoch-1.3.0, default features, no patch) links, stays alive >30 s, no panic, window on GLES (wgpu Vulkan enumerate fails -> GLES fallback, as the other apps), plays video to EOS when no audio sink is available.
OPEN: with pipewiresink (needs GST_PLUGIN_FEATURE_RANK=pipewiresink:PRIMARY; autoaudiosink finds nothing), playbin stalls in PAUSED: pipewiresink logs
"waiting for pool to become active", PipeWire nodes stay suspended, cycles do not advance. Same sink works in a hand-built audio pipeline. Not yet isolated (playbin audio chain vs video preroll vs pipewire thread-loop).
Gaps logged: serial shell PATH lacks /usr/bin; `pkill`/`timeout` missing in the guest shell; grep is ripgrep.
Not done: x86_64, regress, merge.
