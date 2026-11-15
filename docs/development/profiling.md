# Profiling user code in the guest (pcsample + pcprof)

The guest has no `perf`. Instead every CPU's timer tick records the interrupted PC
(`sched/src/pcsample.rs`, always on, ~100 Hz per CPU) and `/proc/pcsample` returns the
samples taken since the previous read: `cpu tgid pid sc mode pc` (mode `u` user, `k` kernel,
`i` idle, `sc` = syscall number or 4095). Kernel time is under-sampled (syscalls run with IRQs
masked): use `/proc/sysstat` for the user/kernel split and pcsample for where user time goes.

`scripts/pcprof.py` symbolises a capture on the host from the binaries in the build output
(`ports/firefox/out/<arch>` etc. mirrors the guest root) and the `/proc/<pid>/maps` captured at
the same time:

```sh
# in the guest (serial root shell, or ffsession.py --post "..."):
cat /proc/pcsample >/dev/null; sleep 15; cat /proc/pcsample > /tmp/pcs.txt
for p in $(ls /proc); do [ -r /proc/$p/maps ] && { echo "=== MAPS $p"; cat /proc/$p/maps; }; done >> /tmp/pcs.txt
cat /tmp/pcs.txt
# on the host:
scripts/pcprof.py post.txt --root ports/firefox/out/aarch64 --tgid <content pid> --bucket 0x1000 --disasm 4
```

It prints samples per thread and mode, per mapped file, the hottest symbols and addresses and
the hottest N-byte blocks. Stripped binaries (Alpine's `libxul.so`) have no symbols, so use
`--bucket` to find the hot function and `--disasm`/`objdump -d` to identify it from its
instructions (xmm = SSE, ymm = AVX2, NEON, scalar). No stack unwinding: the samples are leaf PCs.

## Canvas-2D benchmark

`.claude/skills/run-leandros/canvasperf.html`: 1500 gradient arcs + 40 text lines at 1900x1000
per `requestAnimationFrame`, `console.log("CANVASPERF fps=N")` each second. Run it with
`ffsession.py <arch> <tag> --put .claude/skills/run-leandros/canvasperf.html:/tmp/canvasperf.html
--url file:///tmp/canvasperf.html --prefs 'devtools.console.stdout.content=true' --post "<capture above>"`;
the FPS lines land in `ff.log`.
