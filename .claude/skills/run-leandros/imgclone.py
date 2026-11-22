"""Cheap disk-image copies for the test harnesses."""
import os
import shutil
import subprocess
import sys


# argv prefix for a copy-on-write clone: APFS `cp -c` on macOS, reflink elsewhere (GNU cp -c is not a clone flag)
CPC = ["cp", "-c"] if sys.platform == "darwin" else ["cp", "--reflink=auto", "--sparse=always"]


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


def ensure_remote_key(img):
    """Make root key login work in a cloned image. build-all only embeds
    ~/.ssh/leandros_remote.pub when it exists at build time; a build host
    without the key (the regress box) yields an image sshd refuses
    ("Permission denied (publickey,password)"). Create the key if missing and
    copy its public half into the clone's ESP (::/leandros/authorized_keys)."""
    key = os.path.expanduser(os.environ.get("LEANDROS_REMOTE_KEY", "~/.ssh/leandros_remote"))
    if not os.path.exists(key + ".pub"):
        os.makedirs(os.path.dirname(key), mode=0o700, exist_ok=True)
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "leandros-remote", "-f", key],
                       check=True, stdout=subprocess.DEVNULL)
    esp = f"{img}@@{2048 * 512}"
    subprocess.run(["mmd", "-i", esp, "::/leandros"], stderr=subprocess.DEVNULL)
    subprocess.run(["mcopy", "-o", "-i", esp, key + ".pub", "::/leandros/authorized_keys"], check=True)
