# lane/webmff2 — L11.2 Firefox WebM "file is corrupt" at the end: REFUTED (not an OS bug)

## Verdict
REFUTED as a LeandrOS bug. The failure reproduces identically on a stock Linux
kernel with stock Alpine musl, using the same Firefox build LeandrOS stages
(Alpine 3.21 `firefox-136.0.4-r0`, ports/firefox). A newer Firefox plays the
same file to `ended`. The cause is in Firefox 136's WebM demuxer, not in the
kernel, relibc, the zig-built musl, FP state, madvise/mremap or shm.

## Evidence (no QEMU needed)
Clip: 55 KB ffmpeg VP9+Opus WebM, 12 s, 4 Clusters then Cues; page dumps media events.
Host: linux desktop (x86_64, stock kernel), `podman run --platform linux/amd64 alpine:<tag>`,
`firefox --headless`, MOZ_LOG=MediaDemuxer:5,Nestegg:5.

| Alpine | Firefox | Result |
|---|---|---|
| 3.21 (what LeandrOS ships) | 136.0.4-r0 | `error t=10.035 err=3:Failed to decode media`, stuck at 12.001, `ended` never fires — same as in the guest |
| edge | 154.0-r0 | `ended t=12.008`, demux ends with NS_ERROR_DOM_MEDIA_END_OF_STREAM |

The 136 log shows the same sequence as the guest: the last block (Opus BlockGroup,
pts 12.001), then `read_packet: unknown element 1c53bb6b` (Cues) -> DemuxPacket EOS ->
`WebMTrackDemuxer::NextSample: error` instead of end-of-stream.

## Left open
- The fix is a newer Firefox (bump ports/firefox off Alpine 3.21), a separate decision:
  the port pins 3.21 sonames/patchelf work. Real YouTube DASH WebM (no trailing Cues) is
  not affected, which is why the user's YouTube playback works.
- No guard test added: there is no OS behaviour to guard.
