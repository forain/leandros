# lane/smolpanic2 2026-11-14 - FIXED (hardened; podman-pull panic not reproduced)

Audit: heal_stale() (before each fd op) is a check-then-use across two stack-lock acquisitions, so a netns/veth
reap between the check and the get_mut still reached SocketSet::get_mut on a dead handle. ~50 get/get_mut sites
(lib.rs, listen.rs, ip6.rs slaac RAW handle, netwatch probe) cannot all be proven live at use time.
Fix (servers/net/vendor/smoltcp/.../socket_set.rs): get/get_mut on a stale handle no longer panic; they resolve to an
inert per-variant stand-in (closed/unbound, zero-capacity buffers, reset on every stale access) so ops fail like a
dead link; remove() on a stale handle is a no-op (returns Option). STALE_HITS counter exposed. Wrong-type on a live
socket still panics.
Test: brtest stale_race (3 drivers x 600 iterations of TCP/UDP/ICMP socket+connect+poll+send+recv+close against a
veth stack, parent DELLINKs/recreates the pair, 60 rounds). The unfixed build did NOT panic in 2 runs (3 s, 33 s,
aarch64 HVF), so fail-first was not obtained; the race window is tiny. brtest + nstest pass with the fix (aarch64).
