# Lane ffgmp — "The gmpopenh264 plugin crashed" on 9gag (2026-10-02)

Branch `lane/ffgmp`, base `73b55e9`. Firefox 136.0.4 (Alpine 3.21, musl).

## Result

| | aarch64 (HVF) | x86_64 (TCG) |
|---|---|---|
| `runtests.py --suite regress` | 14/14 PASS (new `clock_monotonic_el0_cntvct` PASS, cntfrq 24 MHz, measured 24.000036 MHz) | 14/14 PASS |
| 9gag video post (`/gag/aXP8OYd`), 2G, ~4 min alive | no banner, video frames change every 30 s screenshot (59.5k/60k px of the video box), H.264 via `ffmpeg video decoder (RDD remote)`, magcount 0 | no banner, video plays (97k/98k px change per shot, AV1 picked by the site), H264 SW broadcast by RDD, magcount 0 |
| Local H.264 page (`h264t.mp4`, Constrained Baseline) | plays, `canPlayType(avc1)=probably`, `ffmpeg video decoder ... codec: h264` | same |
| Wikipedia | renders, magcount 0 | renders, magcount 0 |
| OpenH264 downloaded | never (0 `gmpopenh264` log lines, fresh profile) | never |

Screenshots: `lane-ffgmp-files/` (`*-9gag-video-t*.png` are consecutive
frames of one session, `*-h264-local.png`, `*-wikipedia.png`), plus the test
media (`h264t.mp4`, `h264.html`).

## Root cause 1: a glibc plugin in a musl process (port)

Reproduced on aarch64 with the openh264 prefs re-enabled: the GMP child
(`pid=837`) faulted at the SAME pc as the user report:

    [PF] SEGV pid=837 ... (/usr/lib/firefox/firefox) addr=0x0 W pc=0x14036143e0 pc_in[0x1400915000-... file+0x283a000]
    [EXC] EL0 Fault! PID=837 ESR=0000000092000046 FAR=0 ... x0=0x293C50

libxul file offset 0x283a000 + 0x2CFF3E0 = 0x55393E0 → vaddr 0x55493E0
(vaddr = off + 0x10000): `PR_LoadLibraryWithFlags` returned NULL, then
`MOZ_CrashPrintf("Cannot load plugin as library %d %d", PR_GetError(),
PR_GetOSError())`, line 123 — GMPLoader. MOZ_LOG `GMP:4` shows the child was
`Init pluginPath=.../gmp-gmpopenh264/2.6.0`, sent preload-libs
`libdl.so.2, libpthread.so.0, librt.so.1`, then "Failed to send start".

Cisco's `openh264-linux64-aarch64` 2.6.0 (what aus5 serves to this build) is
a glibc build: `DT_NEEDED libpthread.so.0 libstdc++.so.6 libm.so.6
libgcc_s.so.1 libc.so.6 ld-linux-aarch64.so.1`, `GLIBC_2.17/2.27` versions.
None exist here, so dlopen can only fail. Same failure on stock Alpine
without gcompat.

Why the plugin was used at all: Firefox has no H.264/AAC decoder of its own
(libmozavcodec = ffvpx: VP8/VP9/AV1/Opus/Vorbis/FLAC/MP3). Without a system
libavcodec the RDD broadcast `H264 NONE / AAC NONE / HEVC NONE`, and
`media.gmp.decoder.enabled` (default true) makes OpenH264 the H.264
fallback (content/RDD request a GMP for H.264 — on 9gag via its video and ad
players). Alpine's `firefox` package depends on `ffmpeg-libavcodec` for
exactly this; `build-in-alpine.sh` never staged it because it is
`dlopen()`ed, not `DT_NEEDED`.

Fix (ports/firefox):
- `ffmpeg-in-alpine.sh`: stages `libavcodec.so.60`/`libavutil.so.58`
  (ffmpeg 6.1.2-r1) and their DT_NEEDED closure (52 sonames, 33/34 new,
  +~50/70 MB). Called by `build-in-alpine.sh` (its fix-ups + audit cover the
  new files; audit: 0 unresolved on both arches). Standalone mode
  `build.sh <arch> --ffmpeg-only` adds it to an existing `out/<arch>` from a
  container of the HOST arch (`apk --root --arch`, nothing foreign executed),
  so an x86_64 podman box without binfmt can update the aarch64 tree.
- `leandros-prefs.js`: `media.gmp-gmpopenh264.{enabled,autoupdate,visible}`
  and `media.gmp-widevinecdm.*` = false (Widevine is a glibc build too).
- Decode is software FFmpeg in RDD; VA-API stays off
  (`media.ffmpeg.vaapi.enabled=false`), compositing stays GPU WebRender.

## Root cause 2: EL0 could not read CNTVCT_EL0 (kernel, aarch64)

Exposed by the fix: with libavcodec loaded, every RDD/utility process died
at startup ("Couldn't start RDD process", VideoBridgeParent AbnormalShutdown
loop, 40 processes):

    [EXC] EL0 Fault! PID=607 ESR=000000006234F841 EC=0x18 ELR=...728

EC 0x18 ISS = MRS Op0=3 Op1=3 CRn=14 CRm=0 Op2=2 = `mrs x2, cntvct_el0`;
the only staged ELF with that word at page offset 0x728 is `libhwy.so.1`
(+0x5728; Highway's timer, pulled in by libjxl <- libavcodec). The kernel never
wrote CNTKCTL_EL1, so EL0 counter reads trapped and the process got SIGILL.
Linux sets EL0VCTEN on every CPU (`arch_counter_set_user_access`).

Fix: `arch/aarch64/src/timer.rs` `init()` (BSP and every AP) sets
CNTKCTL_EL1.EL0VCTEN, clears EL0PCTEN/EL0VTEN/EL0PTEN — Linux's policy.
Regression: `timertest` `clock_monotonic_el0_cntvct` (aarch64 variant of the
x86 TSC case): reads CNTFRQ_EL0/CNTVCT_EL0 from EL0, checks monotonic and rate
vs CLOCK_MONOTONIC to 1 %. On the old kernel it dies with SIGILL.

## The other log lines

- `[EPOLL] ctl err=2 op=2` (DEL → ENOENT): not crash fallout. Seen in a run
  with no GMP crash at all (content process, tgid 643). The kernel keys an
  item by (fd, open description) like Linux's (file, fd) and drops it when
  the description is released; DEL of an fd that was closed and whose number
  was reused is ENOENT on Linux too, and libevent ignores ENOENT/EBADF on DEL.
- `[RDD] pipe error: Bad file descriptor` (ipc_channel_posix.cc:664 =
  sendmsg failing with EBADF, not EPIPE): not reproduced. In the repro the GMP
  was requested by the content process, not RDD. With a system FFmpeg the RDD
  never talks to a GMP any more. If it shows up again: the net server's
  sendmsg returns EBADF only for an invalid socket fd or an SCM_RIGHTS fd it
  cannot export (`xfer_export`), peer-gone is EPIPE.

## Tooling

`ffsession.py --put HOST:GUEST[,...]` copies small files (test page, media)
into the guest over the serial shell (base64 chunks).

## Follow-ups

- The main checkout's `ports/firefox/out` has no FFmpeg; restage after merge
  (`ports/firefox/build.sh all`, or `--ffmpeg-only` without arm64 emulation).
  Without it the image still boots, but H.264 sites fall back to nothing
  (no crash: OpenH264 is now disabled).
- Image grows ~50 MB/arch (x265, SVT-AV1, aom encoders come with Alpine's
  libavcodec). A decoder-only FFmpeg build would be ~5 MB if space matters.
