# lane/iddevpts
devpts uid=/gid= mount options go through to_k; unmapped id -> EINVAL (kernel/src/syscall.rs callers, vfs devpts::create).
Tests: idmaptest devpts_gid_translated, devpts_gid_unmapped_einval, devpts_uid_unmapped_einval (cmdline idmap=test).
Fail-first: lane/iddevpts-ff (unfixed) devpts_gid_translated FAIL (ptmx open errno 13); fixed: PASS.
Regress aarch64 box: 81 suites, only mnttest (tfs_sb_registered, tfs_umount_frees) failed in full run; isolated rerun PASS.
