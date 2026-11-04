#!/usr/bin/env python3
"""videotest.py <arch> <tag> [--secs 30] [-- extra ffsession.py args]

Local 1080p AV1 playback regression test, run through ffsession.py (no
Marionette, no network). A looping 12 s AV1 webm is generated with the host
ffmpeg (libsvtav1; cached in $FFSESSION_OUT/hwvideo) and put into the guest
with a page whose script dump()s one sample per second to Firefox's stdout
(browser.dom.window.dump.enabled), i.e. /tmp/ff.log:

    HWV <wall_ms> <currentTime> <totalVideoFrames> <droppedVideoFrames>

PASS when, over >= --secs of wall time, the playhead (wrap-aware) advanced by
>= 1/3 of the wall time and totalVideoFrames grew by >= 1 frame per 2 s of
wall time. Failure to play at all (no HWV lines) also fails.
Do not grep Firefox's log for "Failed to copy image data": it logs on success.
"""
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
A = sys.argv
ARCH, TAG = A[1], A[2]
SECS = int(A[A.index("--secs") + 1]) if "--secs" in A else 30
EXTRA = A[A.index("--") + 1:] if "--" in A else []
OUTD = os.environ.get("FFSESSION_OUT", "/tmp/ffsession")
W = os.path.join(OUTD, "hwvideo")
os.makedirs(W, exist_ok=True)
webm = os.path.join(W, "av1-1080p.webm")
if not os.path.exists(webm):
    subprocess.run(["ffmpeg", "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=25:duration=12",
                    "-c:v", "libsvtav1", "-preset", "10", "-crf", "55", "-g", "50", "-pix_fmt", "yuv420p", webm],
                   check=True)
page = os.path.join(W, "hwv.html")
open(page, "w").write("""<html><title>hwv</title><body style="margin:0;background:#000">
<video id=v src="file:///tmp/hwv.webm" autoplay muted loop style="width:100%"></video>
<script>
var v = document.getElementById('v');
setInterval(function () {
  var q = v.getVideoPlaybackQuality();
  dump('HWV ' + Date.now() + ' ' + v.currentTime.toFixed(3) + ' ' + q.totalVideoFrames + ' ' + q.droppedVideoFrames + '\\n');
}, 1000);
v.play();
</script></body></html>""")
env = dict(os.environ, FFSESSION_OUT=OUTD)
cmd = [sys.executable, os.path.join(HERE, "ffsession.py"), ARCH, TAG,
       "--put", f"{webm}:/tmp/hwv.webm,{page}:/tmp/hwv.html", "--url", "file:///tmp/hwv.html",
       "--prefs", "browser.dom.window.dump.enabled=true;media.autoplay.default=0",
       "--wait", str(SECS + 10), "--shot-every", "10"] + EXTRA
rc = subprocess.run(cmd, env=env).returncode
log = os.path.join(OUTD, f"run-{TAG}", "ff.log")
rows = []
if os.path.exists(log):
    for l in open(log, errors="replace"):
        m = re.match(r"HWV (\d+) ([\d.]+) (\d+) (\d+)", l)
        if m:
            rows.append((int(m[1]) / 1000, float(m[2]), int(m[3]), int(m[4])))
if rc != 0 or len(rows) < 2:
    print(f"FAIL videotest {ARCH}: ffsession rc={rc}, {len(rows)} samples")
    sys.exit(1)
adv = 0.0
for a, b in zip(rows, rows[1:]):
    adv += b[1] - a[1] if b[1] >= a[1] else b[1]       # loop wrap
wall = rows[-1][0] - rows[0][0]
frames = rows[-1][2] - rows[0][2]
print(f"videotest {ARCH}: wall={wall:.1f}s playhead+={adv:.1f}s frames+={frames} dropped={rows[-1][3]} samples={len(rows)}")
if wall < SECS * 0.8:
    print(f"FAIL videotest {ARCH}: observed only {wall:.1f}s of {SECS}s"); sys.exit(1)
if adv < wall / 3 or frames < wall / 2:
    print(f"FAIL videotest {ARCH}: playback did not advance"); sys.exit(1)
print(f"PASS videotest {ARCH}")
