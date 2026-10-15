#!/usr/bin/env bash
# Set (or clear) the kernel command line of a built boot image in place:
# rewrites the `cmdline:` line of /limine.conf on the image's ESP (the FAT
# partition build-all.sh puts at sector 2048).
#
#   scripts/set-image-cmdline.sh leandros-limine-x86_64.img "i915.fake=2"
#   scripts/set-image-cmdline.sh leandros-limine-x86_64.img ""     # clear
#
# Needs mtools (mcopy). The next build-all.sh run rewrites the image anyway.
set -euo pipefail
img="${1:?image}"
line="${2-}"
off=$((2048 * 512))
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mcopy -n -i "$img@@$off" ::/limine.conf "$tmp/limine.conf"
python3 - "$tmp/limine.conf" "$line" <<'EOF'
import sys
p, line = sys.argv[1], sys.argv[2]
out, inserted = [], False
for l in open(p).read().splitlines():
    if l.strip().startswith("cmdline:"):
        continue
    out.append(l)
    if not inserted and line and l.strip().startswith("path:") and "kernel" in l:
        out.append("\tcmdline: " + line)
        inserted = True
open(p, "w").write("\n".join(out) + "\n")
EOF
mcopy -o -i "$img@@$off" "$tmp/limine.conf" ::/limine.conf
echo "$img: cmdline = '${line}'"
