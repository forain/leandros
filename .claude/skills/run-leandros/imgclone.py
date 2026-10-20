"""Cheap disk-image copies for the test harnesses."""
import os
import shutil
import subprocess


def clone_file(src, dst):
    """Copy a disk image keeping it cheap: APFS clone / reflink when possible,
    else a sparse copy (a plain copy would turn holes into real blocks)."""
    try:
        os.unlink(dst)
    except OSError:
        pass
    for cmd in (["cp", "-c", src, dst], ["cp", "--reflink=always", src, dst],
                ["cp", "--sparse=always", src, dst]):
        if subprocess.run(cmd, stderr=subprocess.DEVNULL).returncode == 0:
            return
    shutil.copyfile(src, dst)
