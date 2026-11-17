# russh 0.64.1 -- LeandrOS patches

Vendored unmodified from crates.io (russh 0.64.1), wired in through
`[patch.crates-io]` in `userland/sshd/Cargo.toml`. Every change is marked
`LEANDROS-PATCH` in the source. Purpose: sshd privilege separation
(`orch/designs/design-sshdprivsep.md`, phase P1).

1. **(a) Host-key signer hook.** `server::Config::host_key_signer:
   Option<HostKeySigner>` (`server/mod.rs`). When set, the kex exchange-hash
   signature (`server/kex.rs`) comes from the callback `(key algorithm, RSA
   hash alg, exchange hash) -> SSH-encoded signature blob` instead of
   `sign_with_hash_alg` on `Config::keys`. `keys` still selects the algorithm
   and public half, so it may later hold public-only stand-ins.
2. **(b) Raw userauth signature hook.** New defaulted
   `Handler::auth_publickey_signed(user, &PublicKeyOrCertificate, signed_data,
   signature, session_id)` (`server/mod.rs`), called in `server/encrypted.rs`
   after russh's own verification, in place of the direct `auth_publickey` /
   `auth_openssh_certificate` calls. The default delegates to those, so
   behaviour is unchanged. `signed_data = string(session_id) ||
   USERAUTH_REQUEST up to the signature`, the exact blob that was verified.
3. **Exports** (`lib_inner.rs`): `PublicKeyOrCertificate` and
   `sign_with_hash_alg` (doc-hidden), needed by the two hooks' users. `AlgorithmExt` (doc-hidden) is re-exported too, for the cert algorithm name mapping in `userauth.rs`.

Upgrading: re-vendor the new crate and re-apply the `LEANDROS-PATCH` markers
(`grep -rn LEANDROS-PATCH src`). Tests: `cd userland/sshd/unit && cargo
+<pinned> test --release --target aarch64-apple-darwin` (host-side unit tests of
`src/userauth.rs`), plus the sshsmoke ssh suite.
