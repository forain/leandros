"""Cheap disk-image copies for the test harnesses."""
import os
import shutil
import subprocess
import sys


def clone_file(src, dst):
    """Copy a disk image keeping it cheap: APFS clone / reflink when possible,
    else a sparse copy (a plain copy would turn holes into real blocks)."""
    try:
        os.unlink(dst)
    except OSError:
        pass
    # GNU cp accepts -c (= --preserve=context) and would make a full dense copy, so
    # only use it on macOS.
    clone = ["cp", "-c", src, dst] if sys.platform == "darwin" else ["cp", "--reflink=always", src, dst]
    for cmd in (clone, ["cp", "--reflink=always", src, dst],
                ["cp", "--sparse=always", src, dst]):
        if subprocess.run(cmd, stderr=subprocess.DEVNULL).returncode == 0:
            return
    shutil.copyfile(src, dst)
