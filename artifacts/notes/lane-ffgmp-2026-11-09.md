# lane/ffgmp 2026-11-09 -- GMP (ClearKey) sandbox: DELIVERED

ffsandbox.py now runs an EME/ClearKey flow (requestMediaKeySystemAccess -> createMediaKeys -> createSession ->
generateRequest('keyids')), holding the MediaKeys/session on window so the CDM stays alive until the collect step
(without that the gmplugin exits before collection). Pref media.eme.enabled=true only; no sandbox pref changed.
The gmplugin must exist, Seccomp 2, NoNewPrivs 1, root /.leandros-void.

Results (mode on, main ff4887a3, no kernel change):
- aarch64: gmplugin pid 948 Seccomp 2, filters 1, NNP 1, root /.leandros-void, user/net/ipc ns all differ from parent. 0 Sandbox lines. on=True.
- x86_64:  gmplugin pid 682 same values and ns. 0 Sandbox lines. on=True.
- CDM ran: SBE access, mediakeys, generateRequest-resolved, message license-request on both.

Harness note: gmplugin logs two benign "Sandbox: attempt to open unexpected file /usr/lib/lib{softokn3,freeblpriv3}.so"
lines (NSS libs opened pre-sandbox, not denials). Added to the noise filter narrowly by exact path.
