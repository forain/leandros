#!/usr/bin/env bash
#
# coreutils-test.sh — behavioral smoke/content test for uutils/coreutils on
# LeandrOS aarch64/x86_64, driven against an ALREADY-RUNNING, ALREADY-IN-BRUSH
# QEMU guest over the serial console via driver.py.
#
# This is now a thin wrapper around coreutils-test.py. The logic moved to
# Python because reliably driving brush here requires careful sequencing
# around three real platform quirks discovered while building this harness
# (see the big comment at the top of coreutils-test.py for the full
# writeup):
#
#   1. `$(...)` command substitution always fails immediately with
#      `error: i/o error: not a pipe`.
#   2. Guest `<` input redirection always hangs the shell forever; a `>`
#      output redirect followed *anywhere later on the same line* by any
#      file-opening statement also hangs forever (needs a QEMU restart to
#      recover). `$?` silently swallows the rest of the line too.
#   3. brush's line editor redraws the whole input line on every keystroke;
#      past ~65 characters (the input wraps the 80-column terminal) this
#      redraw is so expensive that submitting the line blows past any sane
#      timeout and can leave the shell in a corrupted, unrecoverable
#      input-buffer state.
#
# Net effect: every guest command line here must be short, free of `$()`,
# `<`, and `$?`, and `>` may only be used for isolated, single-purpose
# fixture writes with no later read on the same line. Doing this in bash
# with the old `brush -c '...many-command-one-liner...'` approach isn't
# possible under these constraints, hence the rewrite in Python.
#
# PREREQUISITE: QEMU must already be booted for the target arch AND already
# sitting at a brush prompt:
#   python3 .claude/skills/run-leandros/driver.py start [aarch64|x86_64]
#   python3 .claude/skills/run-leandros/driver.py cmd 'brush' 15
# This script does NOT start, stop, or build anything.
#
# Usage:
#   ./coreutils-test.sh [aarch64|x86_64]

set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$HERE/coreutils-test.py" "${1:-aarch64}"
