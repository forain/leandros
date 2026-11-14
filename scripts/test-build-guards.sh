#!/usr/bin/env bash
# Guards for build-all.sh: a stale container-built port must not be silently
# reused without a container, and a port failure must be fatal.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd); cd "$ROOT"
source scripts/container-lib.sh
eval "$(awk '/^(port_failed|stage_container_port)\(\) \{/{p=1} p{print} p&&/^}/{p=0}' scripts/build-all.sh)"
export LEANDROS_CONTAINER=none   # simulate "no docker/podman" without touching the real one
T=$(mktemp -d); P=$T/p; mkdir -p "$P/out/aarch64"; touch "$P/out/aarch64/.stamp"; sleep 1; touch "$P/build.sh"
rc=0
(stage_container_port "$P" aarch64 zz Zz ZZ >/dev/null 2>&1) && { echo "FAIL: stale tree reused without container"; rc=1; }
sleep 1; touch "$P/out/aarch64/.stamp"
(stage_container_port "$P" aarch64 zz Zz ZZ >/dev/null 2>&1) || { echo "FAIL: fresh tree rejected"; rc=1; }
(LEANDROS_ALLOW_STALE_PORTS=1 stage_container_port "$P" aarch64 zz Zz ZZ >/dev/null 2>&1) || { echo "FAIL: override"; rc=1; }
(port_failed busd aarch64 x >/dev/null 2>&1) && { echo "FAIL: port failure not fatal"; rc=1; }
(LEANDROS_OPTIONAL_PORTS=a,busd port_failed busd aarch64 x >/dev/null 2>&1) || { echo "FAIL: optional port fatal"; rc=1; }
rm -rf "$T"; [ $rc = 0 ] && echo "build guards OK"; exit $rc
