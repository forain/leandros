# lane xattrsec 2026-10-25 (follow-up to f2fslxxattr)

FIXED: `security.*` xattrs (f2fs name index 6) now supported. `split_name`/`join_name` map `security.` <-> IDX_SECURITY
(servers/xattr/src/lib.rs). Gates: readable by everyone on any inode type; writable/removable by root only
(CAP_SYS_ADMIN / CAP_SETFCAP collapse to euid 0, Cred carries no capability set), allowed on symlinks (labels).
Still unsupported (EOPNOTSUPP): lustre (5), encryption (9, internal), anything else.
Legacy arena/tmpfs share split_name, so they now accept security.* too (arena codec is index-generic; no format change).
Interop: security.capability set by Linux `setcap cap_net_raw+ep` read hex-identical on LeandrOS; set on LeandrOS read by Linux
getfattr hex + `getcap`. `interop.sh all`: 56 PASS, 0 FAIL/UNEXPECTED-PASS. aarch64 regress PASS.
