#!/usr/bin/env python3
"""Moved to .claude/skills/run-leandros/runtests.py (lane harness, 2026-10-02).
This shim keeps the old path working."""
import os, sys
NEW = os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../../.claude/skills/run-leandros/runtests.py")
os.execv(sys.executable, [sys.executable, os.path.normpath(NEW)] + sys.argv[1:])
