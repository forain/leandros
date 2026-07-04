# LeandrOS Network Stack Implementation Plan

This plan details the architecture and implementation steps to construct a complete networking stack in LeandrOS. Currently, the OS features a stubbed `net-server` that implements local `AF_UNIX` loopback stream sockets but lacks real physical device drivers, protocol decoding, routing, dynamic network configurations, and packet filtering (firewalls/NAT/bridging). 

This plan addresses this gap by implementing a physical `virtio-net-pci` driver, integrating the `smoltcp` stack for complete TCP/UDP/IP support, extending socket operations, supporting static and DHCP configurations, and establishing a lightweight packet processing engine following the `nftables` pattern.

## User Review Required

> [!IMPORTANT]
> - **Limine Revision 6 Compliance**: Driver memory allocations and device DMA mappings must conform to Limine Revision 6 mandates. 
> - **Shared Memory & Polling**: The network daemon needs to process RX frames and socket events cooperatively. Since LeandrOS is cooperative, we will run the `smoltcp` polling engine in a dedicated kernel task to ensure asynchronous execution without blocking syscall handlers.
> - **In-Kernel Library Design**: The `net-server` runs inside the kernel context (invoked directly from `syscall.rs`). All user-space buffers are checked and mapped via `validate_user_buf` before copy operations.

## Open Questions

None. The existing driver framework and syscall interfaces are well-suited for these additions.

## Proposed Changes

---

### Component: Network Device Driver (`drivers/src/virtio_net.rs`)

Summary: Implement a new `virtio-net-pci` driver to interact with QEMU's transitional and modern network devices.

#### [NEW] [virtio_net.rs](file:///Users/forain/code/leandros/drivers/src/virtio_net.rs)
- Implement `VirtioNetDevice` representing the physical hardware interface.
  - Define transitional and modern PCI IDs:
    - `VIRTIO_PCI_VENDOR`: `0x1AF4`
    - `VIRTIO_NET_DEVICE_MODERN`: `0x1041`
    - `VIRTIO_NET_DEVICE_LEGACY`: `0x1000`
- Define the configuration registers layout based on VirtIO spec:
  ```rust
  #[repr(C, packed)]
  pub struct VirtioNetConfig {
      pub mac: [u8; 6],
      pub status: u16,
      pub max_virtqueue_pairs: u16,
      pub mtu: u16,
  }
  ```
- Implement `probe()` function to scan the PCI bus, map capabilities, and initialize the device:
  1. Walk PCI capabilities using `walk_caps()` from [virtio_blk.rs](file:///Users/forain/code/leandros/drivers/src/virtio_blk.rs#L232) to find `COMMON_CFG`, `NOTIFY_CFG`, and `DEVICE_CFG`.
  2. Enable PCI MMIO space and Bus Mastering (`enable_pci_mmio()` at [virtio_blk.rs](file:///Users/forain/code/leandros/drivers/src/virtio_blk.rs#L214)).
  3. Perform device initialization sequence: Acknowledge -> Driver -> Features -> Read/Write Features -> Features OK -> Driver OK.
- Set up Virtqueues:
  - Allocate and populate Queue 0 (Receive) and Queue 1 (Transmit) using `VirtQueue::alloc()` (refer to [virtio_blk.rs](file:///Users/forain/code/leandros/drivers/src/virtio_blk.rs#L87)).
  - Pre-allocate RX physical pages (`mm::buddy::alloc(0)`) and queue them as write-only descriptors in Queue 0 to receive incoming packets.
- Implement packet sending (`send_packet`):
  - Chain a read-only descriptor pointing to the packet payload with a write-only status descriptor.
  - Submit descriptors to Queue 1 and notify the host by writing to the notify config register offset.
- Implement packet polling (`poll_receive`):
  - Check Queue 0's used ring. If new frames have arrived, parse the virtio net header, extract the payload, replenish the queue with a fresh descriptor, and return the data.

#### [MODIFY] [lib.rs](file:///Users/forain/code/leandros/drivers/src/lib.rs)
- Declare `pub mod virtio_net;` to export the new module.
- Add initialization helper `pub fn init_net()` that scans for `virtio-net-pci` devices and registers them in a global `static VIRTIO_NET_DEVICES`.

---

### Component: Core Network Stack & Protocols (`servers/net`)

Summary: Integrate `smoltcp` to provide IPv4/IPv6, TCP, UDP, ARP, and DHCP processing. Expand the socket multiplexing layer.

#### [MODIFY] [Cargo.toml](file:///Users/forain/code/leandros/servers/net/Cargo.toml)
- Add `smoltcp` dependency with features:
  ```toml
  smoltcp = { version = "0.11", default-features = false, features = ["alloc", "medium-ethernet", "proto-ipv4", "proto-ipv6", "socket-tcp", "socket-udp", "socket-dhcpv4", "socket-raw"] }
  ```

#### [MODIFY] [lib.rs](file:///Users/forain/code/leandros/servers/net/src/lib.rs)
- Replace mock sockets and fake AF_INET loopback tables with `smoltcp` types.
- Add import of `smoltcp::iface::{Interface, SocketSet}` and `smoltcp::socket::{tcp, udp}`.
- Implement a `smoltcp::phy::Device` wrapper around the physical `VirtioNetDevice` driver:
  - Implement `receive()`: delegate to `virtio_net::poll_receive()`.
  - Implement `transmit()`: delegate to `virtio_net::send_packet()`.
- Update `SockState` (lines 207-222) to handle real protocol descriptors:
  ```rust
  enum SockState {
      None,
      UnixListening { bound_idx: usize },
      UnixConnected { conn_idx: usize, is_a: bool },
      UnixPendingAccept { conn_idx: usize },
      InetUnbound { domain: u8, sock_type: u8 },
      InetBound { domain: u8, sock_type: u8, local_endpoint: smoltcp::wire::IpEndpoint },
      InetListening { socket_handle: smoltcp::socket::SocketHandle },
      InetConnected { socket_handle: smoltcp::socket::SocketHandle },
  }
  ```
- Implement step-by-step POSIX socket mappings:
  - `handle_socket` (line 349): If AF_INET, allocate a `smoltcp` socket (TCP or UDP depending on stream vs datagram type), insert it into `SocketSet`, and return a new FD.
  - `handle_bind` (line 368): For AF_INET, parse the IP and Port, validate availability, and bind the endpoint.
  - `handle_listen` (line 430): Put the TCP socket in listening state.
  - `handle_accept` (line 456): Verify TCP socket state. If a connection is pending, spawn/allocate a new connected socket from the listening socket's queue and return its FD.
  - `handle_connect` (line 548): For AF_INET TCP, trigger the handshake. For UDP, save the remote endpoint.
  - `handle_send` (line 696) & `handle_recv` (line 725): Read/write to the `smoltcp` socket ring buffers using `validate_user_buf` to copy from/to user buffers.
- Enhance Unix Sockets support:
  - Extend `UnixConn` and `UnixRing` to support datagram sockets (`SOCK_DGRAM`) and auto-binding in `handle_bind` for anonymous sockets.
- Implement the network thread loop (`net_daemon`):
  - A background thread that repeatedly calls `interface.poll(&mut socket_set, timestamp)` to process incoming packets, handle TCP retries, and manage DHCP.

---

### Component: Firewall, NAT, and Bridging (`servers/net/src/nftables.rs`)

Summary: Implement a packet filtering and address translation engine that hooks into the routing pipeline.

#### [NEW] [nftables.rs](file:///Users/forain/code/leandros/servers/net/src/nftables.rs)
- Define a lightweight rule evaluation structure resembling Linux's `nftables`:
  ```rust
  pub enum Hook {
      Prerouting,
      Input,
      Forward,
      Output,
      Postrouting,
  }

  pub enum Verdict {
      Accept,
      Drop,
      Reject,
      Jump(String),
  }

  pub struct Rule {
      pub proto: Option<u8>,
      pub src_ip: Option<IpCidr>,
      pub dst_ip: Option<IpCidr>,
      pub src_port: Option<u16>,
      pub dst_port: Option<u16>,
      pub action: Action,
  }

  pub enum Action {
      Verdict(Verdict),
      Snat(IpAddress),
      Dnat(IpAddress),
      Masquerade,
  }
  ```
- Implement the rule engine evaluator:
  - At each hook, iterate over rules inside the active tables/chains.
  - For packet matching, inspect the IP header and transport header (TCP/UDP).
  - Implement Network Address Translation (NAT):
    - `Snat` / `Masquerade`: Modify source IP and allocate a new dynamic port, keeping track of translations in a `Conntrack` table.
    - `Dnat`: Rewrite destination IP of incoming packets.
- Implement Link-Layer Bridging:
  - Support virtual bridge interfaces that group multiple physical ports.
  - Maintain a forwarding database (FDB) mapping MAC addresses to ports with a lease/age-out timeout.
  - If a packet matches a bridged port, bypass local IP processing and forward it directly to the target port.

---

### Component: Kernel Initialization (`kernel/src`)

Summary: Register and start the network server and driver subsystem on boot.

#### [MODIFY] [init.rs](file:///Users/forain/code/leandros/kernel/src/init.rs)
- Call driver and server setup within the initialization task:
  - Add `drivers::virtio_net::init()` inside `init_task_main` (around line 56).
  - Call `net_server::init()` to bring up the network loop.
  - Spawn the `net_daemon` kernel task in the scheduler using `sched::spawn` to run the smoltcp loop.

#### [MODIFY] [syscall.rs](file:///Users/forain/code/leandros/kernel/src/syscall.rs)
- Retain the current routing logic (lines 3124-3240) forwarding socket and network requests directly to `net_server::handle`.
- Update error handling to convert negative Linux error codes correctly.

---

## Verification Plan

### Automated Tests
- Build for both targets:
  - `cargo check -p kernel --target=targets/x86_64-unknown-kernel.json -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec`
  - `cargo check -p kernel --target=targets=aarch64-unknown-kernel.json -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec`
- Run socket tests in the userspace suite under QEMU:
  - `./scripts/build-all.sh`
  - `./scripts/run-qemu.sh`

### Manual Verification
- Check kernel log messages during boot:
  - Verify `[PCI] Found dev 1AF4:1000` or `1AF4:1041` (virtio-net).
  - Verify initialization prints like `[NET] Interface configured with MAC: xx:xx:xx:xx:xx:xx`.
- DHCP validation:
  - Boot QEMU with a user-mode network stack (`-netdev user,id=n0,dhcpstart=10.0.2.15 -device virtio-net-pci,netdev=n0`).
  - Verify that the DHCP client successfully obtains IP address `10.0.2.15`.
- Sockets and Protocol validation:
  - Run network client/server smoke tests in userland using `relibc`'s network sockets.
  - Send packets from a local program inside LeandrOS to a host port using QEMU port forwarding.
- Firewall Validation:
  - Install a test rule (e.g. drop packets targeting port 80).
  - Verify that traffic is dropped and conntrack shows correct state.
