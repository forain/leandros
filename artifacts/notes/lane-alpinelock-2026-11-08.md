# lane/alpinelock 2026-11-08 - stale Alpine locks (box 404)

FIXED. Box build failed with HTTP 404 on apk-tools-static-3.0.8-r0.apk: the v3.24 mirror had superseded the locked packages.
Latest stable is still v3.24 (3.24.4); no release bump needed. Refreshed with `scripts/alpine-lock.py refresh <arch>` (both arches).

Version deltas (identical on aarch64 and x86_64; everything else unchanged):
- apk-tools-static 3.0.8-r0 -> 3.0.9-r0
- openssh-sftp-server 10.3_p1-r1 -> 10.3_p1-r2
- zlib 1.3.2-r1 -> 1.4-r0
The `generated` tar hash is unchanged (no --generated rerun; trigger outputs not affected by these three).

Second finding: box/regress.sh did not ship the generated-<sha>.tar from the Mac apk cache, so a box build failed with
"generated tar ... missing". regress.sh now rsyncs ~/.cache/leandros/apk/<arch>/generated-*.tar to the box before building.

Evidence: aarch64 build ok; default regress aarch64 overall PASS (0 boots failed); ffsandbox aarch64 --mode off: Firefox
processes up, test video currentTime advancing (SBV samples 0.02..2.98+). Box x86_64 result: see merge report.
