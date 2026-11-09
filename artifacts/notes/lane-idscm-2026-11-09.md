# lane/idscm (design-idmap lane 10, H7) - 2026-11-09

FIXED. `parse_scm_creds` used `sched::ns::kid_from_view`, still identity-only after idcore, so a
sender in a `0 100000 10` userns claiming uid 0 was checked as kuid 0 (wrong). It now uses
`sched::ns::to_k(kind, current_userns(), id)` (EINVAL when unmapped) before the scm_check_creds tests.
Receive side (`deliver_scm`) and SO_PEERCRED already went through `view_id` (reader's view, 65534 unmapped)
and the receiver's pidns; verified by tests, no change needed.

Tests (userland/idmaptest, knob `--cmdline idmap=test`): scm_creds_in_translate, scm_creds_unmapped_einval,
peercred_view. On main (tests only) scm_creds_in_translate FAILED (box x86_64); the other two already held.
After the fix: box x86_64 full regress 76 suites, 0 bad. aarch64 knob run of idmaptest/scmtest/nstest: see merge.
nstest map_firefox_seq is expected red under the knob (lane idpermf2fs).
