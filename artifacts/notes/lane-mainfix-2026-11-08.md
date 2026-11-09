# lane/mainfix 2026-11-08 - FIXED
Main 975d334f did not build: lane/nssockref replaced SockEntry.netns (u16) with ns: NsRef + netns() accessor;
lane/netsec (sock_inet_domain_in_netns) and the unix-bind check in servers/net/src/lib.rs still read the removed field.
Fix: use netns(). Verified: both arches build; nstest+nettest pass on aarch64 (HVF) and x86_64 (box KVM).
