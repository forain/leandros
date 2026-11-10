# lane/podpasta (2026-11-10) - PARTIAL (rootless podman on pasta works; pasta never exits)

Method: user leandro, `idmap=test sched.errtrace=pasta`, aarch64 HVF, image built with LEANDROS_IMAGE_ARCHIVE.
Results (Mac aarch64):
1. `podman run --rm alpine:3.21 echo hi` -> `hi` (after the -4 option below; before: "Couldn't set IPv6 route(s) in guest: Address family not supported", no IPv6 stack).
2. `wget -qO- http://192.168.105.1:<port>/` from a container -> body from the Mac http.server (needs --no-map-gw).
3. `-p 8080:80` + container-side nc server: guest host `wget 127.0.0.1:8080` returns the body (pasta splice path). The Alpine image has no httpd applet; nc -l loop used.
4. DNS from the container: `nslookup example.com` works (169.254.1.1 forwarder).
5. 10x `run --rm true`: runs succeed but each run LEAVES one pasta process (+5 live namespaces, +1 sb_count); at about 12 runs crun fails "unshare (CLONE_NEWCGROUP): No space left on device" (MAX_NS_PER_UID 64). `--network=none` leaks nothing.
Fixed:
- ports/podman/containers.conf: `pasta_options = ["-4", "--no-map-gw"]` (config, no kernel change).
- servers/net/src/tunbe.rs `local_loop`: a tap/tun vif stack's ARP request for its own address is answered locally and an IPv4 frame to its own MAC is queued back to its rxq (Linux local table). pasta -t forwards into the container by connecting to the guest address from inside the netns; the SYN went out the tap and nothing answered. userland/tuntest `selfconn_*` (FAIL errno 115 before, PASS after).
- .claude/skills/run-leandros/podpastatest.py + runtests suite `podpasta` (WIP: must be run as leandro; the host harness logs in as root, not wired for that yet).
ROOT CAUSE of the leak (not fixed): inotify is a stub (events never fire: servers/vfs/src/lib.rs handle_inotify_*). pasta started with `--netns <path>` watches the netns dir with inotify IN_DELETE and exits when podman deletes the file; it never gets the event, so pasta (and the 5 namespaces and sb it pins) live forever. Next blocker: real inotify IN_DELETE/IN_CREATE on tmpfs dirs (design-inotify), or an interim pasta teardown.
Not tested: rootful --network=pasta.
