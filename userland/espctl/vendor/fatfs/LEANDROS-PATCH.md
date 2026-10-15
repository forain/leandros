fatfs 0.3.6 from crates.io, vendored for two fixes in `Dir::create_dir`
(src/dir.rs), both reported by fsck.fat on every directory it created:

- `encode_lfn_utf16`: no LFN entry in front of the "." and ".." entries.
- the ".." entry of a directory in the root names cluster 0 (FAT32 too).

Tests, examples and helper scripts of the published crate are omitted.
