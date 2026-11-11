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
- [`ports/podman/containers.conf`](../../ports/podman/containers.conf) sets `firewall_driver = "none"` (there is no iptables or nftables; the net server masquerades the bridge itself) and `pasta_options = ["-4", "--no-map-gw"]` (there is no IPv6 stack, and `--no-map-gw` lets a container reach the LAN gateway instead of having it mapped to the host's loopback).
- `newuidmap` and `newgidmap` are installed setuid; Alpine ships them with file capabilities, which the image's filesystem does not carry.
- `/etc/subuid` and `/etc/subgid` contain `leandro:100000:65536`; `/var/tmp` is mode 1777.

## Networking modes

- **Root, default bridge.** `netavark` creates a bridge (`podman0`) and a veth pair per container through netlink; the net server's L2 switch and NAT give the container its own address and outward reachability, and a published port reaches a container listener. The bridge is created per run, so listeners bound on `0.0.0.0` before it exists are adopted by its stack.
- **Rootless, `pasta`.** `pasta` runs in the new network namespace with a tap device; because it connects to the container's own address from inside the namespace, tap/tun/veth stacks answer ARP and loop frames for their own address locally, as Linux's local routing table does. DNS goes through pasta's forwarder; `-p host:container` forwards work.
- **`--network=none`** creates a namespace with only a loopback and leaks nothing.

## Behaviours worth knowing

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
| `podpasta` | rootless `podman` on pasta: echo, host `wget`, `-p` forward, DNS, churn, no leaked namespaces (opt-in, same image) |

`LEANDROS_IMAGE_ARCHIVE=<tar>` embeds a local OCI archive in the image so no registry is needed in-guest.

## Limitations and open work

- No overlay storage driver; `fsopen`/`fsmount` unsupported.
- No IPv6 and no `netavark` firewall driver; VLAN filtering and STP on the bridge are stored only; ICMP errors for NATed flows are not translated.
- Repeated rootless runs (20x churn) failed a few times in 20 before the lock-keying fix above; it had not been re-measured when this page was written.
- `keyctl` is `ENOSYS`.

## See also

- [../kernel/namespaces.md](../kernel/namespaces.md) — the kernel model
- [../servers/networking.md](../servers/networking.md#network-namespaces) — container networking
- [ports.md](ports.md) — how the Alpine packages enter the image
