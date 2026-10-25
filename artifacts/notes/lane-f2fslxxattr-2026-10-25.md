# Lane f2fslxxattr: Linux-format xattrs + ACLs (f2fslxrw S8 / F12), 2026-10-25

Status: FIXED.
- servers/xattr: `lx_decode` / `lx_encode` — the Linux store is `inline xattr area ++ xnode[0..4072]` with a 24 B header
  (magic 0xF2F52011, refcount 1), 4-byte aligned entries, 4 zero bytes terminator. Decode yields the ordinary arena
  (so all gates/ACL evaluators are unchanged); ACLs (index 2/3) convert f2fs format v1 (short USER_OBJ/GROUP_OBJ/MASK/OTHER)
  <-> posix_acl_xattr v2. Unknown name indexes round-trip untouched.
- servers/f2fs: on lx volumes `read_xattr_arena`/`persist_xattr_block` go through the codec, using the per-inode inline
  area (`inline_xattr_bytes`) and the xnode (lazy alloc via create_node_block, SSA/NAT as for other nodes; the inode block
  is rewritten through the cache so write-back refreshes its checksum). Pseudo xnid `XNID_INLINE` = "inline area only".
  Legacy volumes keep the old arena byte-for-byte. Arena buffers grew to 10240 B (decoded ACLs are larger).
- /bin/xattr gained `-X NAME FILE` (hex print) and `-W NAME HEX FILE` (hex write) for binary values/ACLs.
- interop.py: Linux sets user.small/mid (inline), user.big 3000 B (spilled), access ACL, default ACL, user.inl; LeandrOS
  reads all 7 (hex-identical), sets user.* (inline + 600 B spilled), removes one, sets access+default ACLs; Linux
  reads them back, getfacl shows `user:1234:rw-` / `default:user:77:rwx`; fsck rc 0 before and after Linux writes.
Evidence: `interop.sh all` 56 PASS, 0 FAIL/UNEXPECTED-PASS (lxdef lxnoi lxext lx2g lx32g home).
Open: security.* (index 6) still EOPNOTSUPP in split_name; a >~4.2 KB total store returns ENOSPC like Linux.
