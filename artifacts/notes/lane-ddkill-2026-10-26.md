# lane/ddkill 2026-10-26 (openbugs #89) - FIXED (QEMU aarch64); laptop not redeployed

Root cause: NOT memory pressure, NOT the raw-block path. init's `sweep_strays` (userland/init/src/main.rs)
runs when greetd exits and SIGKILLs every process whose ppid is init outside the console/VT logins.
`nohup sh s0.sh &` over ssh is reparented to init when the ssh session closes; s0.sh itself kills greetd
(text-login migration), so the sweep killed sh, then (next pass, once orphaned) dd. Only init's console
got "killed N stray process(es)", nothing in klog. The 98 MB dsync write worked because no greetd exit
happened during it. mem_guard is not involved (only active while greetd runs, and logs MEMORY PRESSURE).

Fix: `from_ssh(pid)` (environ has SSH_CONNECTION=, which sshd sets for every session and children inherit)
exempts a process from the sweep and from mem_guard's pick_victim; each swept pid is now logged
("init: killing stray pid N (comm)").

Guard: host:straysweeptest.py (`runtests.py aarch64 <tag> --gpu virgl --suite straysweep`): three detached
ssh jobs, root kills greetd. Unfixed: 5/8 (plain job + `sh -c 'sleep | cat'` pipeline killed). Fixed: 8/8,
the non-ssh control stray is still swept. aarch64 regress: PASS.

Open: laptop still runs the old init until a trial from main is deployed. Workaround for lxmigrate s0:
stop greetd in a separate ssh command first, or keep SSH_CONNECTION (default) once the fix is deployed.
