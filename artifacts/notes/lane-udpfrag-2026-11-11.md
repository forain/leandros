# lane/udpfrag 2026-11-11 — PARTIAL (fragmentation was already done; one real gap fixed)

- The TODO claim "no IP fragmentation/reassembly" was stale: smoltcp `proto-ipv4-fragmentation` + 16 reassembly buffers are
  enabled in servers/net (7d5b8c67), IP MTU 1500, 30 s reassembly timeout; `peer_frag_echo` (udppeertest, 4000/20000/65507 to a
  raw-frame host peer over the NIC) already guards it, `udp_large` guards 8000 over loopback.
- New udptest `udp_huge`: 4000/60000/65507 round-trip over loopback (all passed on unfixed code) and 65508 must be EMSGSIZE.
  Red before: plain sendto of 65508 succeeded (only the sendmsg/GSO paths checked UDP_MAX_PAYLOAD). Fixed in handle_send (lib.rs).
- Left open in TODO: PMTUD (IP_MTU_DISCOVER, IP_MTU, DF handling of Frag-Needed) and IGMPv3/SSM.
- Env note: ports/busd/build.sh failed to link in this tree (rust-lld "unable to find -lc"); worked with
  RUSTFLAGS="-C link-self-contained=yes -C relocation-model=static" for that script only.
