# Containers: podman, crun and pasta

The image ships Alpine's container stack (`podman`, `crun`, `conmon`, `netavark`, `aardvark-dns`, `passt`/`pasta`, `fuse-overlayfs`, `slirp4netns`, `containers-common`, and `shadow-uidmap` for `newuidmap`/`newgidmap`) as unmodified binaries from the vanilla Alpine package set (see [ports.md](ports.md#in-guest-builds-apk-without-containers)). Everything that makes them work is in the kernel and servers; no port is patched. `podman run --rm alpine:3.21 echo hi` runs both as root (bridge network) and rootless as the login user (`pasta` network, or `--network=none`), on aarch64 and x86-64.

## What a `podman run` asks of the kernel

| Step | Kernel feature | Where it is described |
|---|---|---|
| Rootless start | `newuidmap`/`newgidmap` write a real non-identity map (`0 <uid> 1; 1 100000 65536` from `/etc/subuid`, `/etc/subgid`) | [namespaces.md](../kernel/namespaces.md#id-maps) |
| Layer apply / container root | `unshare(CLONE_NEWNS)` in a user namespace, rbind and tmpfs mounts, mounts onto `/proc/self/fd/N` | [namespaces.md](../kernel/namespaces.md#mount-namespaces) |
| Container root switch | `pivot_root(".", ".")`, `umount2(".", MNT_DETACH)`, `open_tree`/`move_mount`/`mount_setattr`; `readonlyPaths` as per-mount `ro` | same |
| `/proc`, `/sys`, `/dev` | procfs/sysfs/devpts/mqueue/cgroup2 mounts per namespace; private devpts instance; `/dev/full`; `memfd` + `fexecve` for `crun`'s own copy | same, [vfs.md](../servers/vfs.md) |
| Isolation | PID, UTS, IPC, cgroup namespaces; `setns` into a running container (`exec`); subreaper and ambient capabilities | [namespaces.md](../kernel/namespaces.md) |
| Resource control | cgroup v2 in `user-<uid>.slice`, `pids.max`, `cgroup.kill`, device-controller eBPF | [namespaces.md](../kernel/namespaces.md#cgroup-v2) |
| Network | `/dev/net/tun`, veth, bridge, netlink writes, NAT, `pasta` | [networking.md](../servers/networking.md#network-namespaces) |
| Monitoring | `conmon` pidfd/`ppoll`, `inotify`, `SCM_CREDENTIALS`, `SOCK_SEQPACKET` attach sockets | [vfs.md](../servers/vfs.md#inotify) |

## Configuration shipped

- [`ports/podman/storage.conf`](../../ports/podman/storage.conf) selects `driver = "vfs"`, `runroot = "/run/containers/storage"`, `graphroot = "/var/lib/containers/storage"`. The overlay driver is not available: overlayfs cannot be mounted yet and `fsopen`-family mounts are `ENOSYS`, so each layer is copied.
- [`ports/podman/containers.conf`](../../ports/podman/containers.conf) sets `firewall_driver = "nftables"` (netavark programs the net server's nf_tables store through the vanilla `nft` binary over NETLINK_NETFILTER; the net server's datapath enforces the resulting ruleset) and `pasta_options = ["--no-map-gw"]` (IPv4 and IPv6 are both forwarded, and `--no-map-gw` lets a container reach the LAN gateway instead of having it mapped to the host's loopback).
- `newuidmap` and `newgidmap` are installed setuid; Alpine ships them with file capabilities, which the image's filesystem does not carry.
- `/etc/subuid` and `/etc/subgid` contain `leandro:100000:65536`; `/var/tmp` is mode 1777.

## Networking modes

- **Root, default bridge.** `netavark` creates a bridge (`podman0`) and a veth pair per container through netlink; the net server's L2 switch and NAT give the container its own address and outward reachability, and a published port (`-p`, TCP and UDP, ranges, `127.0.0.1:` bound) is DNATed by the netavark nftables rules, reachable from the host (loopback and NIC address), other containers and external peers (inbound NIC path). A container on a different bridge network can also reach a published port at the host NIC address: the net server DNATs it without masquerade (the client's source address is kept, `podman_publish_crossnet` in podbridge). The first SYN of such a cross-network connection is dropped while ARP learns the target, so it connects after the ~1 s TCP retransmit. The bridge is created per run, so listeners bound on `0.0.0.0` before it exists are adopted by its stack.
- **Rootless, `pasta`.** `pasta` runs in the new network namespace with a tap device; because it connects to the container's own address from inside the namespace, tap/tun/veth stacks answer ARP and loop frames for their own address locally, as Linux's local routing table does. DNS goes through pasta's forwarder; `-p host:container` forwards work. Rootful `--network=pasta -p` does NOT work: podman 5.8.9 itself rejects pasta as root ("only supported for rootless mode or when inside a nested userns"; guarded by `podman_pasta_root_rejected` in podbridge). Published ports are therefore available rootless via pasta today; rootful bridge `-p` awaits the nf_tables work (design-podnat). pasta runs dual-stack (IPv4 and IPv6): the container gets the host's global IPv6 address and RA default route, and `ping -6` to the default gateway works (the unprivileged ping socket is allowed by the container's `ping_group_range`, read through the netns owner's user namespace). A host ping to its own global IPv6 address is delivered locally.
- **`--internal`.** netavark writes `conf/<bridge>/forwarding=0`; the net server honours it, and a later `ip_forward=1` no longer overwrites it (Linux fans out only when the value changes). Netavark installs no firewall rules for internal networks, so isolation is exactly this forwarding switch; a container on it cannot reach outward even with a hand-added default route (`podbridgetest` `podman_internal_unreachable`).
- **nftables limits.** `nft` rules are stored and listed faithfully and evaluated by the net server's rule VM (see [networking.md](../servers/networking.md#nftables)). Filter chains run on new flows, then established and `ct state related` flows follow the FORWARD hook; once a nat postrouting chain exists a bridge subnet without a masquerade rule is not forwarded out (netavark's `--internal` relies on this). With no postrouting chain at all, a route to a NIC subnet keeps the source address, and with no nftables at all the older implicit SNAT remains. Routed (un-NATed) bridge-to-bridge forwarding works, with the first packet parked while ARP or NDP resolves instead of lost. `podman rm -f` removes its rules (checked over 5 create/remove cycles: the ruleset returns to its baseline).
- **IPv6.** A rootful bridge network gets a v6 subnet and NAT66 outbound: TCP, UDP and ICMPv6 echo through the net server's v6 conntrack, neighbour cache and the `nft ip6` masquerade rule. Published ports (`-p`) are DNAT66'd too, reachable on `[::1]`, the NIC address and from another container. Rootless pasta learns its v6 address and route from the router advertisement the net server relays, and `ping -6` through it works. Bridges route v6 between each other without NAT, and a container `traceroute6` works: Time Exceeded for a forwarded flow is generated by the bridge and delivered to the container's raw ICMPv6 socket (see [networking.md](../servers/networking.md#ipv6)).
- **Name resolution on a custom network.** `aardvark-dns` (netavark's resolver) listens on the bridge address (`10.89.0.1:53`, UDP and TCP): a UDP socket bound to a bridge address of the initial network namespace lives on the bridge's stack, and `access()` on the generated `/etc/resolv.conf` succeeds, which its inotify watch needs. (Earlier notes that it did not stay up are obsolete.)
- **`--network=none`** creates a namespace with only a loopback and leaks nothing.

## Behaviours worth knowing

- **Bind mounts onto generated `/proc` files** work (`stat` and the fd path both resolve), which `crun` needs for masked paths; `podman run` failed on it before lane procbind, and a `podsmoke` test is in `regress`.
- **Identity.** Container root is the login user's kuid; files created in the container show up owned by the mapped host ids. The kernel command-line `idmap=off` falls back to identity-only maps (any map with `upper != lower` is refused), which is an escape hatch for debugging, not a supported container configuration.
- **Ownership limits.** At most 32 namespaces per user and 64 overall are allowed in one ancestor chain; a leak therefore appears as `unshare: No space left on device` after a dozen runs. Leaks of this class were traced to leftover `pasta` processes (fixed by pid-namespace init signal semantics) and to socket-held namespaces (`nsreap` prunes dead netlink sockets).
- **Cleanup races.** `podman run --rm` once exited 127 with a SQLite "disk I/O error": the VFS keyed `fcntl`/`flock` locks by open-file slot, so two processes both obtained an exclusive lock on the same database (fixed, see [vfs.md](../servers/vfs.md#advisory-locks)). Anything multi-process on SQLite benefits.
- **Debug knobs.** `sched.errtrace=<name>` logs failing syscalls of processes matching the name; `net.nltrace` traces netlink requests; `sched.wakelog` and `sched.keyselftest` are the wake-key diagnostics.

## Tests

| Suite | What it covers |
|---|---|
| `nstest`, `idmaptest`, `mnttest`, `forktest`, `seccomptest`, `cgtest`, `bpftest`, `inotifytest`, `fusetest` | per-feature kernel behaviour; in the default regress |
| `netlinktest`, `tuntest`, `brtest` | netlink writes, tun/tap, veth, bridge, NAT (`brtest` includes forged-MAC and port-reservation cases) |
| `podbridge` | root `podman run` on the bridge: echo, host and bridge `wget`, DNS, 20x churn, no leaks (opt-in; needs a `LEANDROS_IMAGE_ARCHIVE` image) |
| `podpasta` | rootless `podman` on pasta: echo, host `wget`, `-p` forward, IPv6 address/route and `ping -6`, DNS, churn, no leaked namespaces (opt-in, same image) |

`LEANDROS_IMAGE_ARCHIVE=<tar>` embeds a local OCI archive in the image so no registry is needed in-guest.

## Limitations and open work

- No overlay storage driver; `fsopen`/`fsmount` unsupported.
- VLAN filtering and STP on the bridge are stored only; ICMP errors for NATed flows are not translated.
- Repeated rootless runs (20x churn) failed a few times in 20 before the lock-keying fix above; it had not been re-measured when this page was written.
- `keyctl` is `ENOSYS`.

## See also

- [../kernel/namespaces.md](../kernel/namespaces.md) — the kernel model
- [../servers/networking.md](../servers/networking.md#network-namespaces) — container networking
- [ports.md](ports.md) — how the Alpine packages enter the image
