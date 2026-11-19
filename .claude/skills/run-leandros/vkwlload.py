#!/usr/bin/env python3
"""vkwlload.py -- vkwltest with a long run (VKWL_FRAMES, default 900) and /proc/drm/fence_stats captured before/after."""
# SELF_BOOTS: vkwltest.py boots its own VM
import os
import runpy
import sys

os.environ.setdefault("VKWL_STATS", "1")
os.environ.setdefault("VKWL_FRAMES", "900")
sys.argv[0] = os.path.join(os.path.dirname(os.path.abspath(__file__)), "vkwltest.py")
runpy.run_path(sys.argv[0], run_name="__main__")
