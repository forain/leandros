#!/usr/bin/env python3
# Split `git diff kernel/src/syscall.rs` into 3 per-concern patches by hunk:
#   flags   = fd creation-flag threading (eventfd/timerfd/signalfd + dispatch arm)
#   block   = wait4/waitid/nanosleep block-on-poll conversions
#   uxtrace = gated unix-socket exchange trace (helper + accept/connect/send/recv)
import subprocess, sys
diff = subprocess.run(["git","-C","/Users/forain/code/leandros","diff","kernel/src/syscall.rs"],
                      capture_output=True, text=True).stdout
lines = diff.splitlines(keepends=True)
# header = everything before the first @@
i = 0
while i < len(lines) and not lines[i].startswith("@@"): i += 1
header = "".join(lines[:i])
# collect hunks
hunks = []
cur = None
for ln in lines[i:]:
    if ln.startswith("@@"):
        if cur is not None: hunks.append(cur)
        cur = ln
    else:
        cur += ln
if cur is not None: hunks.append(cur)

def group(h):
    if "block_on_poll_prepare" in h: return "block"
    if "uxtrace" in h or "UXTRACE" in h: return "uxtrace"
    # flags: dispatch arm + the three creators
    if ("VFS_EVENTFD" in h or "VFS_SIGNALFD_CREATE" in h or "VFS_TIMERFD_CREATE" in h
            or "sys_timerfd_create(a0, a1)" in h): return "flags"
    return "UNKNOWN"

buckets = {"flags":[], "block":[], "uxtrace":[], "UNKNOWN":[]}
for h in hunks: buckets[group(h)].append(h)

if buckets["UNKNOWN"]:
    print("!! UNCLASSIFIED HUNKS:", file=sys.stderr)
    for h in buckets["UNKNOWN"]: print(h[:200], file=sys.stderr)
    sys.exit(1)

for name in ("flags","block","uxtrace"):
    out = header + "".join(buckets[name])
    path = f"/tmp/sc_{name}.patch"
    open(path,"w").write(out)
    print(f"{name}: {len(buckets[name])} hunks -> {path}")
