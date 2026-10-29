# lane/udpself 2026-10-29: UDP to our own interface IP, and head-of-line blocking (Meet bug C)

Status: FIXED (QEMU-verified on aarch64; laptop untouched).

## Causes
1. smoltcp never delivers a packet addressed to its own IP: it ARPs for it,
   nobody answers, the datagram waits forever in the socket's TX queue.
2. A UDP socket's head datagram whose next hop never resolves stayed at the
   head of that socket's queue for good, so every later send on the socket
   (a STUN request to Google) was stuck behind it.

## Fix
- `servers/net/src/lib.rs` `local_delivery()` (called from `TxToken::consume`
  after the Output nftables hook): an ARP request for an address a local
  netif holds is answered in-stack (reply queued with `inject_frame` for the
  asking stack); an IPv4 frame whose destination MAC is a local netif's MAC
  is queued for that netif's stack instead of being sent. Works across
  interfaces (eth0 -> wifi0 address) and for connected, unconnected and
  wildcard-bound senders.
- `servers/net/vendor/smoltcp/src/socket/udp.rs`: `tx_stall_since`; a head
  datagram that cannot be dispatched for 3 s (`TX_STALL_MS`) is dropped and
  the socket moves on (Linux drops after ~3 s on an unresolved neighbour).
  Within the 3 s later sends to other destinations still wait behind it.
- TCP (item 3): a SYN to an unresolvable host sits in its own socket's
  state, not a shared queue, so it blocks nothing else; connect() times out
  through the normal path. No change.

## Tests
- New `/bin/udpselftest` (userland/udpselftest), in the regress list: send to
  own IP + reply, wildcard sender, connected-UDP to own IP, dead on-link
  (.254) then DNS query from the same socket gets a reply.
- Unfixed code: 5 of 10 checks FAIL (own_delivered, own_reply, wildcard_to_own,
  send_connected_own, reply_after_dead). Fixed: 10/10 (4.6 s).
- nettest, udptest pass on aarch64.

## Open
- The dead-peer datagram delays later sends from that socket by up to 3 s
  (not zero as on Linux). Re-run `ffprobe/loop.html` on the laptop.
