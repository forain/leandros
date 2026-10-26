# lane/legacyrm2 (openbugs #46, slice 2) -- FIXED

Slice 1 (lane/legacyrm) made the mount refuse the legacy LeandrOS f2fs layout, so every mounted volume is lx or
Linux-made. This slice removes the now-dead legacy branches from the f2fs server.

## Change (servers/f2fs/src/lib.rs, durable.rs)
- `struct Fmt { lx }` and its `impl` are gone: the Linux node layout is plain constants (`NODE_FOOTER`, `ADDRS_PER_BLOCK`,
  `I_ADDR`, `I_NID`, `I_XATTR_NID`, `I_BLOCKS`, `I_*TIME[_NS]`) and free fns (`write_footer`, `footer_nid`, `footer_ino`,
  `set_footer_ino`; `dentry_dt` -> `ft_to_dt`). The 24 helpers that took `f: Fmt` lost the parameter (161 call sites).
- `SbInfo.fmt` -> `SbInfo.lxnode: bool`, used only by the mount refusal.
- Removed legacy-only code: linear `dir_add_entry` body (76 lines) and `dir_find` linear scan, the non-hash dir walk,
  the legacy xattr arena paths and `xarena()`, NAT version bump, `(false, _) => nid` NAT owner, LSB-first `sit_vbit`,
  the non-lx log_alloc / log-head fallbacks, `F2FS_ADVISE_IBLOCKS` opt-in (`inode_blocks` now returns u64) and `INO_ADVISE`.
- lx vs Linux-made (`native`) distinctions untouched. `durable::cp_is_legacy` kept: it tests the checkpoint checksum
  offset, which a Linux volume can legitimately vary (large NAT bitmap), so it is not provably unreachable.
- No new compiler warnings (the remaining f2fs warnings are the same set as on main).
- Lines: 2 files, +395 / -602 (net -207).

## Evidence (aarch64 HVF)
- regress: overall PASS (incl. pollnarrowtest, nettag2test, sshsmoke, wifisimtest).
- f2fscptest PASS incl. `legacy` case 4/4 (refused, logged, alive, image untouched); blkmanytest PASS (12/12 mounted);
  stdevtest PASS.
- `scripts/f2fslx/interop.sh all`: 124 PASS, 0 EXPECTED-FAIL, 0 FAIL.
- x86_64 build OK; scripts/check-rpi5.sh OK. Tests run sequentially.

## Open
- None for #46. `cp_is_legacy` (checkpoint CRC variant) intentionally kept.
