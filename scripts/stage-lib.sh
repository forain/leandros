# Race-free staging into output dirs (per-tree target/m6-out, see scripts/lib/m6out.sh)
# written by concurrent worktree builds. Source it; call: stage_file <mode> <src> <dst>
# Writes a unique temp file in dst's directory, then rename(2)s it over dst, so
# concurrent stagers never see "File exists" and readers never see a partial file.
stage_file() {
  local mode="$1" src="$2" dst="$3" tmp
  tmp="$(mktemp "$(dirname "$dst")/.stage.$(basename "$dst").XXXXXX")" || return 1
  if ! cp "$src" "$tmp" || ! chmod "$mode" "$tmp" || ! mv -f "$tmp" "$dst"; then
    rm -f "$tmp"; return 1
  fi
}
