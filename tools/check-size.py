#!/usr/bin/env python3
"""Fail when what a UI ships has grown past its budget.

    python tools/check-size.py [--root DIR] [--print]

streamtap's two UIs are plain files, no bundler and no build step, so their size is the size of the tracked files that
ship: the extension without its test scripts and README, the app's page (desktop/crates/streamtap-app/ui) whole. A
budget is the measure of 2026-11-23 plus about 10 %, rounded up to a whole kilobyte (1000 bytes). A change that takes
more than the margin raises the budget on purpose, in this file, with the reason in its commit; it does not creep.

Exit 0: both within budget. Exit 1: the surfaces over budget are listed. Exit 2: git could not be asked, or a surface
holds no file (a moved folder must not read as "small").
"""

import argparse
import gzip
import os
import subprocess
import sys

# name: (folder, tracked paths below it that do not ship, budget in bytes)
SURFACES = {
    "extension": ("extension", ("scripts/", "README.md", ".mwgrc"), 251_000),
    "app page": ("desktop/crates/streamtap-app/ui", (), 86_000),
}


def tracked(root):
    out = subprocess.run(["git", "ls-files", "-z"], cwd=root, check=True, capture_output=True).stdout
    return [p for p in out.decode("utf-8").split("\0") if p]


def shipped(paths, folder, excluded):
    """The tracked paths of a surface that ship: below its folder, none of the excluded prefixes or names."""
    below = folder.rstrip("/") + "/"
    return [p for p in paths if p.startswith(below) and not any(p[len(below):].startswith(x) for x in excluded)]


def measure(root, files):
    """(raw bytes, gzip bytes of each file summed): what the browser reads and, roughly, what a transfer would."""
    raw = zipped = 0
    for p in files:
        with open(os.path.join(root, p), "rb") as f:
            data = f.read()
        raw += len(data)
        zipped += len(gzip.compress(data, 9, mtime=0))
    return raw, zipped


def problems(sizes, surfaces=SURFACES):
    """The surfaces over budget as (name, raw, budget); `sizes` is {name: raw bytes}."""
    return [(name, sizes[name], surfaces[name][2]) for name in surfaces if sizes[name] > surfaces[name][2]]


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--root", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
    ap.add_argument("--print", action="store_true", help="only print the measures")
    args = ap.parse_args(argv)
    try:
        paths = tracked(args.root)
    except (OSError, subprocess.CalledProcessError) as e:
        print(f"check-size: git could not be asked: {e}", file=sys.stderr)
        return 2
    sizes = {}
    for name, (folder, excluded, budget) in SURFACES.items():
        files = shipped(paths, folder, excluded)
        if not files:
            print(f"check-size: {name}: no tracked file below {folder}", file=sys.stderr)
            return 2
        raw, zipped = measure(args.root, files)
        sizes[name] = raw
        print(f"{name}: {len(files)} files, {raw} bytes ({zipped} gzipped), budget {budget}")
    if args.print:
        return 0
    over = problems(sizes)
    for name, raw, budget in over:
        print(f"check-size: {name} is {raw - budget} bytes over its budget ({raw} > {budget})", file=sys.stderr)
    return 1 if over else 0


if __name__ == "__main__":
    sys.exit(main())
