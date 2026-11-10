# lane/macforge - FIXED (idflip review A, M1)

- fwd.rs: conntrack entries carry the ingress bridge port sid (`Ct.port`); flow lookup includes it. `forget_macs` removed; reaping is by sid (`l2::forget` -> `fwd::forget` drops entries whose bridge or port died), called from free_vifs/reap/release.
- Ingress port identity: `Vif.rxsrc` (bridge rxq, parallel) filled by `l2::push`, handed to `fwd::divert`; misaligned or unknown port fails closed.
- Return path: `l2::deliver(br, port, frame)` sends un-NAT frames to the owning port, bypassing the FDB (no MAC hijack).
- brtest: fwd_mac_forge, fwd_mac_steal. Fail-first (aarch64, test-only c1acfaa2 vs old net server): both FAIL; with fix brtest 140/0.
- Open: B in a separate user namespace not exercised (netns only).
