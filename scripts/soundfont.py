#!/usr/bin/env python3
"""Fetch the unmodified Fluid R3 GM SoundFont and its original license.

Source SHA256 is published in Debian's fluid-soundfont_3.1-5.3.dsc:
https://deb.debian.org/debian/pool/main/f/fluid-soundfont/
Large assets stay in the host cache, outside the source repository.
"""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

ARCHIVE = "fluid-soundfont_3.1.orig.tar.gz"
URL = "https://deb.debian.org/debian/pool/main/f/fluid-soundfont/" + ARCHIVE
ARCHIVE_SHA256 = "2621acaa1c78e4abdb24bdd163230cc577e61276936d6aa6e3180582142f0343"
PAYLOADS = {
    "FluidR3_GM.sf2": "74594e8f4250680adf590507a306655a299935343583256f3b722c48a1bc1cb0",
    "COPYING": "8ef830b65c97a976b86e34bb5fde08d99dfb1db13c4149b5b20bc837ac6c4568",
    "README": "2f92d462684629f207e43815115b79d7f1ce130d30ca8792b7d82b398a8306f9",
}


def cache_dir():
    return Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "leandros" / "soundfonts"


def digest(path):
    result = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def stage():
    cache = cache_dir()
    cache.mkdir(parents=True, exist_ok=True)
    if all((cache / name).is_file() and digest(cache / name) == expected
           for name, expected in PAYLOADS.items()):
        return
    archive = cache / ARCHIVE
    if not archive.exists() or digest(archive) != ARCHIVE_SHA256:
        with tempfile.TemporaryDirectory(dir=cache) as temporary:
            download = Path(temporary) / ARCHIVE
            subprocess.run(["curl", "--fail", "--location", "--retry", "3",
                            "--output", str(download), URL], check=True)
            if digest(download) != ARCHIVE_SHA256:
                raise SystemExit("Fluid R3 source archive SHA256 mismatch")
            os.replace(download, archive)
    # Extract only the three known files, never archive-controlled paths.
    with tarfile.open(archive, "r:gz") as source:
        for name in PAYLOADS:
            member = source.getmember("fluid-soundfont-3.1/" + name)
            if not member.isfile():
                raise SystemExit(f"Fluid R3 archive member is not a file: {name}")
            with source.extractfile(member) as stream, tempfile.NamedTemporaryFile(dir=cache, delete=False) as output:
                temporary = Path(output.name)
                try:
                    shutil.copyfileobj(stream, output)
                except BaseException:
                    temporary.unlink(missing_ok=True)
                    raise
            temporary.chmod(0o644)
            os.replace(temporary, cache / name)
    packaged_files()
    print(f"Fluid R3 GM staged at {cache / 'FluidR3_GM.sf2'}")


def packaged_files():
    cache = cache_dir()
    for name, expected in PAYLOADS.items():
        path = cache / name
        if not path.is_file():
            raise SystemExit(f"Missing Fluid R3 payload: {path}; run python3 scripts/soundfont.py")
        if digest(path) != expected:
            raise SystemExit(f"Fluid R3 payload SHA256 mismatch: {path}; run python3 scripts/soundfont.py")
    return [("/usr/share/soundfonts", name, str(cache / name)) for name in PAYLOADS]


if __name__ == "__main__":
    stage()
