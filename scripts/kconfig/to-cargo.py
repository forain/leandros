#!/usr/bin/env python3
"""LeandrOS kconfig driver (uses the vendored kconfiglib).

  to-cargo.py defconfig    --arch A --defconfig F --out .config
  to-cargo.py olddefconfig --arch A --config .config --out OUT
  to-cargo.py features     --arch A --config .config [--kmod-list FILE]

`features` prints a comma-separated cargo feature list for the kernel crate:
  BOARD_RPI5 -> rpi5, BOARD_RASPI4B -> raspi4b, NVIDIA -> nvidia,
  CONFIG_FOO_BAR=y -> drv-foo-bar   (only if kernel/Cargo.toml declares it)
  CONFIG_FOO_BAR=m -> not a feature; "foo-bar" is added to the module list.
"""
import argparse, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
import kconfiglib  # noqa: E402

# Kconfig symbol (lowercase, dashed) -> modules/<dir> when the two differ.
KMOD_NAME_MAP = {"kmod-hello": "hello", "usb-video-class": "uvc"}

BOARD_FEATURES = {"BOARD_RPI5": "rpi5", "BOARD_RASPI4B": "raspi4b", "NVIDIA": "nvidia"}


def load(arch):
    os.environ["ARCH"] = arch
    os.environ["srctree"] = ROOT
    cwd = os.getcwd()
    os.chdir(ROOT)
    try:
        return kconfiglib.Kconfig("Kconfig", warn_to_stderr=False)
    finally:
        os.chdir(cwd)


def declared_features():
    feats, inside = set(), False
    for line in open(os.path.join(ROOT, "kernel", "Cargo.toml")):
        if line.startswith("["):
            inside = line.strip() == "[features]"
        elif inside:
            m = re.match(r"([A-Za-z0-9_-]+)\s*=", line)
            if m:
                feats.add(m.group(1))
    return feats


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["defconfig", "olddefconfig", "features"])
    ap.add_argument("--arch", required=True, choices=["x86_64", "aarch64"])
    ap.add_argument("--defconfig")
    ap.add_argument("--config")
    ap.add_argument("--out")
    ap.add_argument("--kmod-list")
    a = ap.parse_args()
    kconf = load(a.arch)
    if a.cmd == "defconfig":
        kconf.load_config(a.defconfig)
        kconf.write_config(a.out)
        return
    kconf.load_config(a.config)
    if a.cmd == "olddefconfig":
        kconf.write_config(a.out or a.config)
        return
    declared = declared_features()
    feats, mods = [], []
    for sym in kconf.unique_defined_syms:
        if sym.type not in (kconfiglib.BOOL, kconfiglib.TRISTATE) or sym.name.startswith("ARCH_"):
            continue
        val = sym.tri_value
        if sym.name in BOARD_FEATURES:
            if val == 2:
                feats.append(BOARD_FEATURES[sym.name])
            continue
        name = sym.name.lower().replace("_", "-")
        if val == 2 and "drv-" + name in declared:
            feats.append("drv-" + name)
        elif val == 1:
            if "mod-" + name in declared:
                feats.append("mod-" + name)  # =m: leave the built-in driver out of the kernel
            mod = KMOD_NAME_MAP.get(name, name)
            if not os.path.isfile(os.path.join(ROOT, "modules", mod, "Cargo.toml")):
                sys.exit("to-cargo.py: CONFIG_%s=m but there is no module crate modules/%s/Cargo.toml"
                         % (sym.name, mod))
            mods.append(mod)
    if a.kmod_list:
        with open(a.kmod_list, "w") as f:
            f.write("".join(m + "\n" for m in sorted(mods)))
    print(",".join(feats))


main()
