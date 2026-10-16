# lane/netroam — Wi-Fi roaming: listeners, autoconnect, not-joinable networks (2026-10-16)

Branch `lane/netroam` off local main `aac2e1e0`. Three bugs from the real
laptop (EliteBook, `iwlwifi.mgr=1`, kernel main-aac2e1e0, boot10 klog): the
user switched networks in the COSMIC applet ('Anvara' 172.16.149.179/16 ->
'Anvara Starlink' 192.168.1.210/24 -> back), tried 'LEANDRO2026', and
autoconnect then roamed between Anvara / Anvara Starlink / Anvara 5G.

## Commits

| commit | what |
|---|---|
| `a0e89a54` | net: TCP listeners keep a LISTEN socket and an accept queue on every stack (`servers/net/src/listen.rs`) |
| `ea6cc7c5` | net: UDP servers bound to INADDR_ANY receive on every interface (`servers/net/src/udpany.rs`) |
| `a1fdc114` | wifi_sim: one data path per network, WPA2 variants (`t s m x`), a flaky AP (`d`), default AP SimTkip |
| `ec8a1470` | wifi_mgr: NetworkManager autoconnect rules, per-BSS back-off, exact not-joinable causes; legacy profile priority 0 |
| `834fb201` | nm shim: export only BSSs the kernel can join |
| `623ba1c5` | tests: netroamtest.py, `LEANDROS_NIC3=wifisim`, `netcheck --udp-echo`, net-e2e / netroam suites |

## Bug 1 — sshd's 0.0.0.0:22 refused on wifi0 after roaming

**Root cause.** smoltcp has no accept queue: a LISTEN socket takes the first
SYN and *becomes* the connection. The net server had exactly one smoltcp
socket per stack per listener, re-armed only inside `accept()`. Any time that
socket was not back in LISTEN, every SYN to the port on that stack got an RST
("Connection refused") while ICMP kept working, and sshd never saw anything.
Three ways it stayed that way for good:

1. **CLOSE-WAIT before accept** (the reproduced one): a connection completes
   while the server is not accepting, the client gives up (FIN) first. The
   listener's `poll` reported only ESTABLISHED, so tokio never called
   `accept()` again (accept itself would have taken CLOSE-WAIT). Port dead.
2. **SYN-RECEIVED to a vanished peer**: no timeout on the handshake (smoltcp
   has none unless set), so a SYN whose SYN-ACKs never got an answer (a peer
   that roamed away, a lossy link) held the port forever.
3. **Address loss mid-handshake**: `deconfigure` aborts every non-LISTEN TCP
   socket of the stack, including a listener in SYN-RECEIVED; the CLOSED
   socket was never re-armed.

The laptop's log shows "0 connection(s) reset" at every address removal, so
(3) did not fire there; the trigger was (1) or (2) on the final network: a
connection the client abandoned while sshd was slow to accept it (the boot
was fsync-heavy: profile writes, f2fs stick; sshd.log of that boot never
reached the disk), or a handshake whose peer went away during the roaming.
Either leaves exactly "ping works, ssh refused, sshd logs nothing".

**QEMU repro on the old kernel** (aarch64, `LEANDROS_NIC2=wifisim`, banner
through wifi0): `kill -STOP <sshd>`, client connects + sends + closes,
`kill -CONT` -> wifi0's port 22 refused for the whole 150 s observed (eth0's
listener, another stack, fine). Plain churn alone (A/B switches, address
change each time) did not reproduce it: the wifi0 listener survives address
changes; it is the transient states that killed it.

**Fix** (`listen.rs`): per stack, an *arm* in LISTEN plus a *queue*, both
owned by the stack (stack lock only). After every daemon poll `maintain`
moves an arm that took a SYN into the queue and arms a new one (backlog 16
per stack), aborts handshakes older than 60 s, drops connections reset
before accept, puts a socket that an RST returned to LISTEN back as the arm,
and re-arms whatever killed the arm. The arm is kept at a higher socket slot
than its queue (swap of socket contents): smoltcp gives a segment to the
first socket that accepts it and a LISTEN socket accepts any SYN, so a
retransmitted SYN must reach its own handshake, not start a second one for
the same 4-tuple. `accept` takes the oldest ESTABLISHED/CLOSE-WAIT; `poll`
reports both; `/proc/net/tcp` rx_queue of a listener = waiting connections.
Listener placement: wildcard -> every stack; 127/8 -> loopback; any other
address -> every NIC stack with that address as smoltcp's filter (follows
the address between interfaces). `SockState::InetListening` is now
`{ lid, local }` and never changes after `listen()`.

**UDP** (`udpany.rs`): a bind to 0.0.0.0 with a chosen port (a server) gets
a socket on every other stack, keyed by the open file description; recv /
recvmsg / poll / FIONREAD read them all, a reply leaves from the stack its
destination routes through; no migration. Before, such a server lived on the
default route's stack only (the multiif lane's documented limitation), so a
server started while eth0 had the default route never heard wifi0. Port-0
binds (clients), connected sockets and address binds stay single-stack.

## Bug 2 — autoconnect roamed away / ping-pong

Boot10: legacy 'Anvara' (cmdline `wifi.ssid`) had priority -1000, the
applet-saved networks 0. After 'LEANDRO2026' failed, autoconnect chose
'Anvara Starlink' (priority 0), whose AP deauthenticated during the
handshake (reason 6, lines ~1219-1225), then legacy Anvara, then after the
user's 'Anvara 5G' wrong-key attempt Starlink again, ...

Now (`wifi_mgr.rs`, docs/servers/netctl.md "Rules a client may rely on"):

- legacy profile priority **0** (netctl-abi `legacy_profile`);
- autoconnect only while disconnected (unchanged: a working connection is
  never left for autoconnect reasons);
- rank: priority, then **most recently connected** (this boot, then the
  stored `timestamp_s`), then the legacy network, then signal. After a failed
  activation (user or auto) it goes back to the last network it was on;
- **per-BSS back-off** 15 s / 60 s / 120 s / 300 s for failures of that AP
  (auth/assoc refused, deauth during the handshake, no answer; and a second
  link it drops within 30 s inside 2 min), kept across profile reloads,
  cleared on a successful connect. Profile back-off (10/30/60 s) only for
  failures that are not one AP's;
- the autoconnect log line says why: `autoconnect: 'NetA' (priority 0) via
  0a:57:53:a0:00:01 (last connected 15 s ago)` / `(the legacy network)`.

Not done: the NM shim does not write `timestamp_s` on activation (NM does),
so across reboots recency falls back to "legacy, then signal". At boot that
picks the legacy network over equally-prioritised saved ones — what keeps
SSH on the laptop's home network reachable before login.

## Bug 3 — 'LEANDRO2026' rejected with a contradictory message

`select_akm` (JOINABLE) refuses a WPA2-PSK BSS when: the AKM list has no
plain PSK and only PSK-SHA256 (AKM 6, the radio has no `PSK_SHA256` cap) or
only FT-PSK (AKM 4); MFPR is set (no `PMF` cap); or the pairwise or the
**group** cipher is not CCMP (TKIP group = WPA/WPA2 mixed mode). The old
message printed `security_summary`, which calls all of PSK / PSK-SHA256 /
FT-PSK "WPA2-PSK" — hence "offers WPA2-PSK which this interface cannot join".
MFPC without MFPR was already joinable. The boot10 log has no RSN details,
so which one 'LEANDRO2026' (14:23:33:fa:5b:e8, ch 4/8/10, dual-band with
LEANDRO20265G) uses is **not known yet**; the next laptop boot will say:

- every non-JOINABLE BSS logs once (per BSSID, again only when it changes):
  `[iwlwifi] not joinable: 'X' <bssid> ch N: RSN v1 group TKIP pairwise
  CCMP,TKIP akm PSK caps 0x0000 (MFPC 0 MFPR 0) +WPA1 element: <cause>`;
- a connection fails `unsupported-security (<cause>)`, e.g. "the network uses
  the TKIP group cipher (WPA/WPA2 mixed mode); only CCMP is supported: set the
  router to WPA2-AES (CCMP) only", "requires management frame protection
  (802.11w, MFPR set), which is not supported yet", "offers only PSK-SHA256
  key management (AKM 6), which is not supported yet", "offers only FT-PSK
  ...", SAE-only, 802.1X, WPA1, WEP, open/secured mismatch. The netctl reason
  stays `UNSUPPORTED_SECURITY` (`WIFI_STATE` reason 8; the shim maps it to
  NM device reason 1 as documented);
- the shim exported every non-SAE BSS, JOINABLE or not; it now exports only
  JOINABLE ones, so such a network is no longer offered in the applet.

**What is cheap to support** (decided, not built): none of the three is a
one-liner. PSK-SHA256 needs SHA-256, the SHA-256 KDF and AES-128-CMAC EAPOL
MICs (key descriptor version 3) — ~200 lines plus vectors; most AKM-6 APs
also set MFPC and many MFPR, i.e. PMF. TKIP group needs a TKIP GTK in the
firmware (key + Michael RX/TX keys) and Michael MIC checking/countermeasures
on received group frames; testable only on air. MFP-required is PMF
(IGTK/BIP), deferred with WPA3 by the user. If the laptop shows TKIP group,
the fix is the router (WPA2-AES); if PSK-SHA256 without MFPR, that is the
cheapest to add next.

## Tests (release builds, Mac: aarch64 HVF, x86_64 TCG)

`netroamtest.py` (new; `--suite netroam`, last in `net-e2e`): wifi_sim with
two data paths (`LEANDROS_NIC2=wifisim` 10.0.3.0/24 + `LEANDROS_NIC3=wifisim`
10.0.4.0/24), APs NetA (path 1, also legacy via cmdline), NetB (path 2,
stronger), Flaky (`d`), TkipNet/ShaNet/MfpNet/FtNet; sshd and a
`netcheck --udp-echo 7777` server started before the churn.

| | aarch64 | x86_64 |
|---|---|---|
| `--suite regress` (24) | 24/24 PASS (wifisim selftest 36/36) | 24/24 PASS (36/36) |
| `--suite net-e2e` (netctltest, nmtest mock full + ro, nmrealtest, netroamtest) | 5/5 PASS | 5/5 PASS |
| netroamtest | 14/14 PASS | 14/14 PASS |
| multinic.py | 10/10 PASS | not run |
| sshsmoke --full | PASS | not run |
| old kernel, same netroamtest `--only tcp` | listener checks PASS (churn alone is not the trigger); legacy_auto FAIL (priority -1000) | — |
| old kernel, sshd SIGSTOP + abandoned client | wifi0:22 refused 150+ s | — |

netroamtest steps: legacy_auto, a_listen, b_listen, a2_listen, churn
(4 more switches), eth0_listen, stall, stay, fail_return, noflap, backoff,
rsn_log, join_col, sec_msgs.

## Laptop kernel

`LEANDROS_KERNEL_TAG=netroam-623ba1c5 ./scripts/build-all.sh --arch x86_64`
in the worktree (build tag `netroam-623ba1c5` in the binary):
`/Users/forain/code/leandros/.claude/worktrees/agent-a39d18300cf19c8dd/target/final-x86_64/kernel`
and `/Users/forain/code/leandros/.claude/worktrees/agent-a39d18300cf19c8dd/initrd-x86_64.cpio`
(what `laptop-deploy-kernel` takes by default from that tree). Not deployed.

## Laptop test list (iwlwifi.mgr=1)

1. Boot: `[iwlwifi] autoconnect: 'Anvara' (priority 0) via ... (the legacy
   network)` (or the most recently used saved network). SSH from the Mac.
2. Scan lines followed by `not joinable:` lines: **copy the one for
   'LEANDRO2026' / 'LEANDRO20265G'** (group/pairwise/akm/MFPC/MFPR) — that is
   the answer to bug 3. They are no longer in the applet's list.
3. Applet: Anvara -> Anvara Starlink -> Anvara; after each switch `ssh` to
   the current address answers (no "Connection refused"), also from the Mac
   while the switch happens (start an ssh mid-roam, ctrl-C it, ssh again).
4. `cat /proc/net/tcp`: the `:0016` LISTEN row's rx_queue stays 0 when idle.
5. Connect a network that fails (wrong password on Anvara 5G): autoconnect
   returns to the network you were on (`last connected N s ago`), not
   Starlink.
6. If Starlink deauths during the handshake again: `<bssid>: failure 1 in a
   row, autoconnect skips it for 15 s`, then 60 s, 120 s, 300 s; no
   ping-pong; never leaves a working connection by itself.
7. `netctl connect LEANDRO2026 <pass>` fails with the specific cause.
8. Regression: wired + Wi-Fi together, `netcheck` both ways, Firefox.

## Open / follow-ups (also in TODO.md)

- NM shim: write `timestamp_s` on successful activation (NM semantics),
  without making the resulting reload wipe wrong-key blocks of unchanged
  profiles.
- Two SYNs for one listener inside one daemon poll: the second still gets an
  RST (one arm per pass; re-armed right after the poll). A second spare arm
  would cost 2.25 MiB of buffers per listener per stack.
- A delayed duplicate SYN for an *accepted* connection can still reach the
  arm when the arm's slot is below the connection's (pre-existing; ordering
  is only enforced against the queue).
