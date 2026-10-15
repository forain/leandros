# lane/multiif — every network device is its own interface (2026-10-15)

Lane L1 of the network-management plan: `eth0..` for wired NICs, `wifi0` for
iwlwifi, one smoltcp stack + DHCP client per interface, routing by prefix and
metric, the DHCP-loss fix, per-interface /proc/net, and a two-NIC QEMU test.

## Commits

- `d9b02a18` net: one smoltcp stack per registered netif (behaviour-identical refactor)
  — registry + `STACKS[sid]` plumbing; exactly one netif (`eth0`) whose ops
  are the old backend dispatch, Wi-Fi fallback included. Bisect boundary.
- `bd3d0f22` net: every network device is its own interface (eth0.., wifi0)
  — fallback deleted, per-device netifs, routing/failover, virtio link
  state, /proc per interface, netcheck/wifiscan, LEANDROS_NIC2, multinic.py.

## Registry API (drivers/src/netdev.rs; module docs are authoritative)

- `count()`, `get(id)`, `iter()`, `by_name("wifi0")` → `Netif` (Copy):
  `id` (dense, stable), `kind` (Wired/Wifi), `name()`, `ifindex()` (= id+2,
  lo = 1), `driver()`, `mac()`, `link() -> (up, Mbit/s, full)`,
  `link_generation()`, `send/recv/rx_rearm/rx_irq_driven`, `info_lines()`.
- Registration only in `netdev::init` (write-once table, lock-free reads).
  Order: every virtio-net, r8169, e1000e, then iwlwifi. MAX_NETIF = 4.
- `set_link_hook(fn(id, LinkEvent::{Up{speed},Down}))`, called by the net
  daemon (no lock held) on carrier transitions.
- servers/net re-exports `iface_snapshot() -> Vec<IfSnapshot>` (name, kind,
  driver, mac, carrier, speed, addr/prefix, gateway, dns, metric,
  default_route, leased) and `set_iface_hook(fn(id))` (carrier, address,
  route or DNS changed). That is what lane L2 (/dev/netctl) should read.

## Behaviour

- Route selection for a new flow: 127/8 → lo; on-link longest prefix (tie:
  lower metric); else lowest-metric default route. Interfaces need an
  address and carrier. Metrics: wired 100+n (eth0 100, eth1 101), Wi-Fi 600.
- Established TCP stays on its stack. Address removed (lease lost, carrier
  gone > 4 s, or a new lease with a different address) → every TCP socket on
  that stack is aborted. Carrier blip < 4 s, or link back with the same lease
  within 10 s → connections survive (multinic `blip`).
- DHCP-loss bug (old lib.rs:2377): smoltcp `Deconfigured` used to leave the
  address and default route installed; now it deconfigures (except while
  rebinding after our own reset on link-up, where smoltcp also reports it).
- UDP not bound to an address migrates (same port) when its stack cannot
  reach the destination any more. UDP bound to INADDR_ANY receives only on
  the stack it was created on (the default route's) — pre-existing class of
  limitation (it never received on lo either).
- Only virtio-net dev 0 keeps the 10.0.2.15/24 pre-lease address (QEMU slirp).
- virtio-net negotiates `VIRTIO_NET_F_STATUS`: `set_link` is a real carrier
  change. Status byte read ≤ every 50 ms, plain `ldrb` (HVF ISV).
- Step 1 differs from main in one detail: an ICMP socket whose first
  destination is 127/8 now lives on lo (it used to be put on eth0's stack).

## Tests (release builds, both arches, Mac: aarch64 HVF, x86_64 TCG)

| | aarch64 | x86_64 |
|---|---|---|
| step 1 regress (21 suites + sshsmoke) + netcheck | PASS | PASS |
| step 2 regress + netcheck + wifiscan -v | PASS | PASS |
| step 2 `multinic.py` (2 NICs, slirp) | 10/10 | 10/10 |
| step 2 `LEANDROS_NIC=e1000e` netcheck + nettest | PASS | not run |

multinic steps: leases (10.0.2.15 + 10.0.3.15), route (eth0 100 first, eth1
101), resolv (10.0.2.3 first), onlink (10.0.3.2 via eth1, 10.0.2.2 via eth0),
default (1.1.1.1 via eth0), listen (sshd banner through both), blip (held
eth0 connection ALIVE across a 1.5 s `set_link net0 off/on`), failover (new
flow via eth1 at once, first default eth1, eth0 ipv4 removed, resolv.conf
10.0.3.3, held eth0 connection RESET after 7.7 s aarch64 / 13.7 s x86),
recover (lease back, default on eth0), eth1cut (default stays eth0).

## Throughput (netcheck), before → after

Internet (cachefly, vmnet) is too noisy to compare: step 2 aarch64 download4
gave 84, 175 and 506 Mbit/s on three runs of the same image (baseline 561).
The comparison that matters is a local HTTP server on the vmnet gateway
(`python3 -m http.server`, 256 MB file, `netcheck --get ... --secs 8`):

| Mbit/s | main 8cf70b02 | lane (bd3d0f22) |
|---|---|---|
| aarch64 x1 | 2194, 2296, 2316 | 2349, 2451, 2138 |
| aarch64 x4 | 2830, 2348, 2680 | 957*, 2599, 2712 |
| x86_64 (TCG) x1 | 469, 462, 441 | 223, 118, 440 |
| x86_64 (TCG) x4 | 133, 184, 346 | 422, 386, 386 |

(*) one stalled run, same shape as baseline's x86 x4 runs. No regression
beyond run-to-run noise. Internet numbers: base aarch64 634/561/418 (single/
x4/get), step 1 673/572/443, step 2 705/506/417 (best of 2); x86 base
353/293/363, step 1 397/416/381.

## Laptop checklist (cannot be tested here)

1. Boot with cable in: `[NET] netif eth0 = e1000e`, `[NET] netif wifi0 =
   iwlwifi` on the console; `/proc/net/nic` has `[eth0]` and `[wifi0]`;
   `/proc/net/route` eth0 metric 100 first, wifi0 600.
2. wifi.conf / `wifi.ssid=` autoconnect still happens (`wlan: link up ...
   (wifi0)`), wifi0 gets its own lease (`DHCP configured, address: X
   (wifi0)`), and SSH answers on BOTH addresses before login.
3. Pull the cable: `eth0: carrier down`; new flows leave via wifi0 at once
   (`netcheck --via 1.1.1.1 80` says wifi0); after ~4 s `eth0: address ...
   removed`; SSH via the wifi address keeps working; resolv.conf lists the
   Wi-Fi DNS. Plug back: lease again, default back on eth0.
4. Boot without cable: everything via wifi0; `netcheck` picks wifi0
   (`nic| checking wifi0`).
5. e1000e IRQ: `rx_irq on` in the netd line still (per-netif irq-driven).
6. `remote.py discover` / serial `DHCP configured, address: X (ifname)` —
   two lines now; the address regex still matches, but the LAST one wins
   (may be wifi0's): pick the one you want.

## Open risks

- iwlwifi registers at probe with a random MAC; the stack's MAC is updated
  on link-generation change (association). If the firmware MAC differs and
  the first association does not bump the generation, DHCP would use the
  wrong MAC (it does bump today: LINK_GEN += 1 on connect).
- A carrier-less wired NIC at boot (laptop, cable out) has no pre-lease
  address; anything that connected out before a lease used to get a
  blackholed SYN and now gets ENETUNREACH at once (Linux-like).
- Both interfaces on the same subnet (cable and Wi-Fi to the same router):
  on-link tie broken by metric; replies to wifi0's address may arrive on
  eth0 (weak host) — smoltcp drops packets for an address its interface does
  not hold, so inbound to the wifi address via the cable would be lost.
  Untested; real routers ARP per interface so this should not happen.
- UDP servers bound to INADDR_ANY serve one interface only.
- `/proc/net/nic` format: sections added; readers that take the first match
  get eth0 (unchanged), scripts that read the whole file see more lines.
