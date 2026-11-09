# lane/platfix 2026-11-09
FIXED. A --platform pull/run of alpine:3.21 replaces that tag (docker and podman), so mixed-arch runs got exec format error.
New leandros_alpine_image (scripts/container-lib.sh) pulls --platform and tags alpine:3.21-arm64 / -amd64 (re-pulled only if the tag is missing or wrong arch); leandros_alpine_run, make/check-dualboot-fixture.sh and dualboot-shrink-test.sh use it.
Mac/Docker: aarch64, x86_64, aarch64 runs print right machine, tags arm64/amd64. Box/podman: amd64 path OK, tags correct; arm64 containers cannot run on the box (no qemu-aarch64 binfmt, exec format error) - environmental, not this bug.
