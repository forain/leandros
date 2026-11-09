# integ-ffsb final acceptance (2026-11-09)

Hash: 197bbc9f (local main).

## Box regress (by hash, release)
- x86_64: 69 suites, 0 bad, overall PASS (build 407s, runtests 667s)
- aarch64: 69 suites, 0 bad, overall PASS (build 321s, runtests 899s)
- No flakes seen; host:wifisimtest.py 1/1.

## Firefox sandbox (Mac, ffsandbox.py integ)
- aarch64 --mode on: on=True, video progressing, 0 sandbox log lines.
- x86_64 --mode on (TCG): on=True, video progressing, 0 sandbox log lines.
- aarch64 --mode off (LEANDROS_FIREFOX_SANDBOX=0): Firefox works, video progressing, all procs Seccomp 0 / NNP 0 / root /, no namespace differences.

Per-process table (same on both arches; parent/forkserver unsandboxed by design):

| type | Seccomp | NNP | root | ns differing from parent |
|---|---|---|---|---|
| parent | 0 | 0 | / | - |
| forkserver | 0 | 0 | / | none |
| socket | 2 | 1 | /.leandros-void | user, ipc (net shared) |
| tab (x4) | 2 | 1 | /.leandros-void | user, net, ipc |
| rdd | 2 | 1 | /.leandros-void | user, net |
| utility | 2 | 1 | /.leandros-void | user, net, ipc |

## GMP
Untested: the image ships only gmp-clearkey (/usr/lib/firefox/gmp-clearkey);
no OpenH264 plugin and ffsession has no hook to drive a GMP launch
(no EME/RTCPeerConnection page, no gmp-manager fetch path). A gmplugin process was not observed.

## Flakes
None.
