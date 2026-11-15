# lane tun6dad (2026-11-15)
tunbe.rs: RFC4862 DAD for tap/veth/bridge vifs (1 s, NS from ::, tentative held out of stack, IFA_F_TENTATIVE/DADFAILED/NODAD in RTM_GETADDR). IFF_TUN is NOARP (no DAD); L3 tun answers NS for the peer; v6 over IFF_TUN with PI 0x86dd. Tests: tuntest tun6_*, brtest NODAD.
Open: DAD for NIC SLAAC/static (ip6.rs), vif addresses in /proc/net/if_inet6, accept_dad/dad_transmits sysctl.
