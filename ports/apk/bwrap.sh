#!/bin/sh
# bwrap stand-in: LeandrOS has no user/mount namespaces, so there is nothing to
# sandbox with. Parse bubblewrap's options just far enough to apply the
# environment (--clearenv, --setenv) and exec the command
# unsandboxed. glycin (gdk-pixbuf's image loaders) runs its loaders through it.
clear=0
E=""
while [ $# -gt 0 ]; do
  case "$1" in
    --clearenv) clear=1; shift ;;
    --setenv) E="$E
$2=$3"; shift 3 ;;
    --unsetenv) shift 2 ;;
    --version) echo "bubblewrap 0.12.0 (LeandrOS stand-in)"; exit 0 ;;
    --args|--userns|--userns2|--pidns|--uid|--gid|--hostname|--chdir|--lock-file|--sync-fd|--remount-ro|--exec-label|--file-label|--proc|--dev|--tmpfs|--mqueue|--dir|--seccomp|--add-seccomp-fd|--block-fd|--userns-block-fd|--info-fd|--json-status-fd|--cap-add|--cap-drop|--perms|--size|--ro-bind-data|--bind-data|--file|--chmod-data)
      shift 2 ;;
    --bind|--bind-try|--ro-bind|--ro-bind-try|--dev-bind|--dev-bind-try|--symlink|--chmod|--bind-fd|--ro-bind-fd|--overlay-src|--overlay|--tmp-overlay|--ro-overlay)
      shift 3 ;;
    --) shift; break ;;
    -*) shift ;;
    *) break ;;
  esac
done
[ $# -gt 0 ] || exit 1
IFS='
'
set -f
for kv in $E; do
  [ -n "$kv" ] && set -- "$kv" "$@"
done
unset IFS
if [ "$clear" = 1 ]; then exec /usr/bin/env -i "$@"; fi
exec /usr/bin/env "$@"
