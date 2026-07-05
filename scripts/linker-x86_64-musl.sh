#!/usr/bin/env bash
exec zig cc -target x86_64-linux-musl "$@"
