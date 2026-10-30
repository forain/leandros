# lane/gstport 2026-10-30 (PARTIAL, HANDOFF)

Stage 1 DONE: ports/gstreamer (Alpine 3.21 gstreamer 1.24.11, plugins-base/good, gst-libav, gst-plugin-pipewire 1.2.7 = our PipeWire).
Image +46 MiB aarch64 (staged tree 119 MiB; glib/ffmpeg shared with firefox/pipewire trees). Sysroot -dev merged (gst*, glib, gio, util-linux mount/blkid, pcre2, intl, ssp).
Stage 2 DONE: gst-inspect 53 plugins/1007 features; videotestsrc, mkv demux + avdec_mpeg4 + vorbisdec chains OK; audio-only pipeline -> pipewiresink streams into leandros_output (cycles=424/7 s, underruns 0).
Stage 3 PARTIAL: cosmic-player (epoch-1.3.0, default features, no patch) links, stays alive >30 s, no panic, window on GLES (wgpu Vulkan enumerate fails -> GLES fallback, as the other apps), plays video to EOS when no audio sink is available.
OPEN: with pipewiresink (needs GST_PLUGIN_FEATURE_RANK=pipewiresink:PRIMARY; autoaudiosink finds nothing), playbin stalls in PAUSED: pipewiresink logs
"waiting for pool to become active", PipeWire nodes stay suspended, cycles do not advance. Same sink works in a hand-built audio pipeline. Not yet isolated (playbin audio chain vs video preroll vs pipewire thread-loop).
Gaps logged: serial shell PATH lacks /usr/bin; `pkill`/`timeout` missing in the guest shell; grep is ripgrep.
Not done: x86_64, regress, merge.

## Update gstport2 (root cause FOUND + FIXED, S3 goal met)
Stall = upstream gst-plugin-pipewire 1.2.7 deadlock, not our OS: on_param_changed (runs on the PW loop thread holding the thread-loop lock) waits for pool activation; the streaming thread in setcaps() needs that lock back after its timed wait, and only a buffer reaching render() activates the pool. Whoever wins the lock after the "paused" signal decides; audio-only/uridecodebin pipelines happened to win, playbin and cosmic-player's first launch lost (second launch won). Also do_start() signals the cond before the pool active flag is set (lost wake-up).
Fix: libgstpipewire is rebuilt in ports/gstreamer/build-in-alpine.sh from vendored sources ports/gstreamer/data/gst-pipewire-1.2.7 (patched gstpipewiresink.c: drop the loop lock while waiting, poll 5 ms). No COSMIC source touched.
Evidence (fresh boot, first cosmic-player launch): sink cycles 1 -> 471 -> 941 -> 1880 over ~30 s, process alive, 0 panics, clip played to EOS.
Isolation record: playbin+fakesink video stalled in default flags; flags=0x3/0x12/0x217/0x417 and uridecodebin+two sinks passed (timing); pipewire*:5 debug changes the race.
gstsession.py: fixed rg-incompatible grep -h, added --no-s3, S3 now samples 4 times.
