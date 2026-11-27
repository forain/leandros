# Content-keyed binary cache in front of the COSMIC/m6 cargo builds (sourced; needs stage-lib.sh).
#
#   cosmic_cache_build <label> <arch> <srcdir> "<extra key text>" "<extra key files>" \
#                      "<srcbin>|<m6-out name> ..." -- <build command...>
#
# Hit: the cached binaries are staged into $M6_OUT and the build command is NOT run.
# Miss: the srcbins are removed, the command runs (it must succeed and leave every srcbin
# behind), the binaries are staged and stored under $M6/bincache/<key>/<name>.
# One log line per call: `[cosmic-cache] hit|miss <label> <arch> <key8>`.
#
# Key = sha1 over: this lib + the calling script + extra files (patches, ssp_guard.c, ...),
# extra text (features, flags), the source tree contents (all files except target/.cargo/.git,
# plus git HEAD and status when it is a checkout, plus out-of-tree `path = "../x"` deps),
# the generated .cargo/config.toml, build-rust.sh + gen-cargo-config.sh + toolchain wrappers and
# linker, rustc/cargo versions, the build env (CARGO*, RUST*, CC*, CFLAGS*, PKG_CONFIG*, ...), and
# a listing (path, type, size, mtime, link target) of the m3-gl-stack/sysroot-<arch> it links.
# Uncertain input => it belongs in the key: a wrong reuse is worse than a rebuild.
#
# Concurrency: flock on the key serialises two trees that miss at once (the loser then hits);
# the store is temp dir + `mv -T` (store-if-absent). Prune keeps the newest COSMIC_CACHE_KEEP (6)
# keys per binary name.

_cc_tree_hash() { # <dir> -> content hash of the source tree
  local d="$1"
  ( cd "$d" || exit 1
    find . \( -path ./target -o -path ./.cargo -o -path ./.git \) -prune -o -type f -print0 \
      | LC_ALL=C sort -z | xargs -0 -r sha1sum
    if [ -e .git ]; then
      echo "HEAD $(git rev-parse HEAD 2>/dev/null)"
      git status --porcelain 2>/dev/null | grep -v ' \.cargo/\| target/\?$' || true
    fi ) | sha1sum | cut -d' ' -f1
}

_cc_manifest() { # <label> <arch> <srcdir> <extra text> <extra files> -> the exact text that is hashed
  local label="$1" arch="$2" src="$3" text="$4" files="$5" m6 s3 f d
  m6="${M6:-$HOME/code/leandros-artifacts/m6-session-bins}"
  s3="${DYN_S3:-$HOME/code/leandros-artifacts/m3-gl-stack}"
  {
    echo "label=$label arch=$arch text=$text"
    for f in "${BASH_SOURCE[0]}" "${BASH_SOURCE[2]:-$0}" $files \
             "$m6/build-rust.sh" "$m6/gen-cargo-config.sh" "$m6"/toolchain/*-linux-musl-*; do
      [ -f "$f" ] && echo "file $(basename "$f") $(sha1sum < "$f" | cut -c1-40)"
    done
    # the real generator and linker the wrappers point at
    f=$(grep -o '/[^" ]*gen-cargo-config.sh' "$m6/gen-cargo-config.sh" 2>/dev/null | head -1)
    [ -f "$f" ] && echo "gen $(sha1sum < "$f" | cut -c1-40)"
    [ -e "$m6/toolchain/ld.lld" ] && echo "ld $(stat -L -c '%s %Y' "$m6/toolchain/ld.lld")"
    echo "src $(_cc_tree_hash "$src")"
    for d in $(grep -ho 'path *= *"\.\./[^"]*"' "$src/Cargo.toml" 2>/dev/null | sed 's/.*"\(.*\)"/\1/'); do
      [ -d "$src/$d" ] && echo "dep $d $(_cc_tree_hash "$src/$d")"
    done
    [ -f "$src/.cargo/config.toml" ] && echo "cfg $(sha1sum < "$src/.cargo/config.toml" | cut -c1-40)"
    rustc +nightly -vV 2>&1; cargo +nightly -V 2>&1
    env | grep -E '^(CARGO|RUST|CC|CXX|AR|CFLAGS|CXXFLAGS|LDFLAGS|PKG_CONFIG|SDKROOT|DYN_|LEANDROS_WGPU)' \
      | grep -v '^CARGO_HOME=\|^RUSTUP_HOME=' | LC_ALL=C sort
    echo "sysroot"
    # Non-directories only, and the alpine-dev stamp by CONTENT: alpine-dev.py
    # rewrites the stamp (and so bumps usr/ and usr/bin/ mtimes) on every build
    # even when nothing changed, which gave every tree a new key.
    find "$s3/sysroot-$arch" ! -type d ! -name .alpine-dev.stamp -printf '%P %y %s %T@ %l\n' 2>/dev/null | LC_ALL=C sort
    for _st in "$s3/sysroot-$arch"/usr/.alpine-dev.stamp; do [ -f "$_st" ] && echo "stamp $(sha1sum < "$_st" | cut -c1-40)"; done
  }
}

_cc_stage() { # <dir with files> <spec...>
  local dir="$1" sp; shift
  for sp in "$@"; do stage_file 0755 "$dir/${sp#*|}" "$M6_OUT/${sp#*|}" || return 1; done
}

_cc_prune() { # <bincache dir> <names...>
  local root="$1" n k i; shift
  for n in "$@"; do
    i=0
    while IFS= read -r k; do
      i=$((i+1)); [ $i -gt "${COSMIC_CACHE_KEEP:-6}" ] && rm -f "$root/$k/$n"
    done < <(cd "$root" && ls -td -- */ 2>/dev/null | tr -d / | while read -r d; do [ -f "$d/$n" ] && echo "$d"; done)
  done
  find "$root" -mindepth 1 -maxdepth 1 -type d -empty -not -name '.*' -exec rmdir {} + 2>/dev/null || true
}

cosmic_cache_build() {
  local label="$1" arch="$2" src="$3" text="$4" files="$5" specs="$6"; shift 7  # drops the `--`
  local m6="${M6:-$HOME/code/leandros-artifacts/m6-session-bins}"
  local root="$m6/bincache" key k8 sp names=() sps=() dir tmp rc
  mkdir -p "$root" "$M6_OUT"
  # a working .cargo/config.toml is part of the key: callers generate it before calling
  local man; man=$(mktemp "$root/.man.XXXXXX") || return 1
  _cc_manifest "$label" "$arch" "$src" "$text" "$files" > "$man" || { rm -f "$man"; return 1; }
  key=$(sha1sum < "$man" | cut -d' ' -f1)
  k8=${key:0:8}; dir="$root/$key"
  for sp in $specs; do sps+=("$sp"); names+=("${sp#*|}"); done
  local hit=1
  for sp in "${names[@]}"; do [ -f "$dir/$sp" ] || hit=0; done
  if [ $hit = 1 ]; then
    rm -f "$man"; touch "$dir"; _cc_stage "$dir" "${sps[@]}" || return 1
    echo "[cosmic-cache] hit $label $arch $k8"; return 0
  fi
  # miss: build under the key lock so a concurrent tree waits and then hits
  (
    exec 9>"$root/.lock-$key"; flock 9
    ok=1; for sp in "${names[@]}"; do [ -f "$dir/$sp" ] || ok=0; done
    if [ $ok = 1 ]; then echo "[cosmic-cache] hit $label $arch $k8"; exit 20; fi
    echo "[cosmic-cache] miss $label $arch $k8"
    mkdir -p "$root/.last-inputs"; cp "$man" "$root/.last-inputs/$label-$arch.txt"
    for sp in "${sps[@]}"; do rm -f "${sp%%|*}"; done   # build-rust.sh's exit status does not reflect cargo's
    "$@" || exit 1
    for sp in "${sps[@]}"; do [ -x "${sp%%|*}" ] || { echo "cosmic-cache: $label build for $arch left no ${sp%%|*}" >&2; exit 1; }; done
    tmp=$(mktemp -d "$root/.tmp.XXXXXX") || exit 1
    for sp in "${sps[@]}"; do cp "${sp%%|*}" "$tmp/${sp#*|}" && chmod 0755 "$tmp/${sp#*|}" || { rm -rf "$tmp"; exit 1; }; done
    cp "$man" "$tmp/inputs.txt"
    mv -T "$tmp" "$dir" 2>/dev/null || rm -rf "$tmp"      # store-if-absent
    for sp in "${sps[@]}"; do stage_file 0755 "${sp%%|*}" "$M6_OUT/${sp#*|}" || exit 1; done
    exit 0
  )
  rc=$?; rm -f "$man"
  if [ $rc = 20 ]; then touch "$dir"; _cc_stage "$dir" "${sps[@]}" || return 1
  elif [ $rc != 0 ]; then return 1; fi
  _cc_prune "$root" "${names[@]}"
}
