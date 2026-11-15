#!/usr/bin/env python3
"""Fail when an app's copy of a file of appkit's release kit is not the file of the appkit it pins.

pult, takt and streamtap install with scripts of their own and ship a copy of
`release-kit/scripts/uninstall-entry.ps1` beside their install.ps1 (the entry in Settings > Apps is
written and checked by that one text). The copy is the kit's file at the commit the app's workflows
pin the actions of the release kit to (`uses: WKDSteve/appkit/release-kit/<action>@<sha>`); every such
pin of the app must name the same commit.

    python tools/check-release-kit-copy.py [--root .] [--appkit-dir DIR]
        [--copy scripts/uninstall-entry.ps1=release-kit/scripts/uninstall-entry.ps1 ...]

The kit's file is read from a checkout of appkit (`--appkit-dir`, or `$APPKIT_DIR`, with `git show`)
or else from GitHub (appkit is public; no token is sent). Line ends do not count (a checkout on
Windows may have CRLF).

Exit 0: every copy is the kit's file. Exit 1: a copy differs or is missing, or the pins disagree; the
message says which and how to take the kit's file. Exit 2: the check could not run (no pin, no
network, an unknown commit); that is not a pass.

A copy of this tool lives in each of those apps (as with check-appkit-pin.py): a fix here goes there too.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

USER_AGENT = "appkit-release-kit-copy-check (https://github.com/WKDSteve/appkit)"
RAW = "https://raw.githubusercontent.com/WKDSteve/appkit/{sha}/{path}"
PIN = re.compile(r"uses:\s*WKDSteve/appkit/release-kit/[A-Za-z0-9_-]+@([0-9a-f]{40})\b")
DEFAULT_COPIES = ["scripts/uninstall-entry.ps1=release-kit/scripts/uninstall-entry.ps1"]


class Unchecked(Exception):
    """The check could not run."""


def pinned_commit(root: Path) -> str:
    """The one commit of appkit all release-kit actions of the workflows of `root` are pinned to."""
    found: dict[str, list[str]] = {}
    workflows = root / ".github" / "workflows"
    for path in sorted(workflows.glob("*.y*ml")) if workflows.is_dir() else []:
        for sha in PIN.findall(path.read_text(encoding="utf-8")):
            found.setdefault(sha, []).append(path.name)
    if not found:
        raise Unchecked(f"no action of appkit's release kit is pinned in {workflows}")
    if len(found) > 1:
        names = "; ".join(f"{sha[:12]} in {', '.join(sorted(set(files)))}" for sha, files in sorted(found.items()))
        raise ValueError(f"the release-kit actions are pinned to more than one commit of appkit: {names}")
    return next(iter(found))


def kit_file(sha: str, path: str, appkit_dir: Path | None) -> bytes:
    """The bytes of `path` in appkit at `sha`."""
    if appkit_dir is not None:
        res = subprocess.run(["git", "-C", str(appkit_dir), "show", f"{sha}:{path}"], capture_output=True)
        if res.returncode != 0:
            raise Unchecked(f"appkit at {appkit_dir} has no {path} at {sha[:12]} "
                            f"(git fetch there?): {res.stderr.decode(errors='replace').strip()}")
        return res.stdout
    url = RAW.format(sha=sha, path=path)
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:  # noqa: S310 (a fixed https host)
            return response.read()
    except urllib.error.HTTPError as e:
        if e.code == 404:
            raise ValueError(f"appkit has no {path} at {sha[:12]}: the pinned release kit is older than the file") from e
        raise Unchecked(f"GitHub answered {e.code} for {url}") from e
    except (urllib.error.URLError, TimeoutError) as e:
        raise Unchecked(f"GitHub could not be asked for {url}: {e}") from e


def plain(data: bytes) -> bytes:
    return data.replace(b"\r\n", b"\n")


def check(root: Path, copies: list[str], appkit_dir: Path | None) -> list[str]:
    """What is wrong, as sentences (none: every copy is the kit's file)."""
    sha = pinned_commit(root)
    problems = []
    for pair in copies:
        ours, _, theirs = pair.partition("=")
        if not ours or not theirs:
            raise Unchecked(f"--copy wants ours=theirs, not {pair!r}")
        kit = kit_file(sha, theirs, appkit_dir)
        copy = root / ours
        if not copy.is_file():
            problems.append(f"{ours} is missing: take appkit's {theirs} at {sha[:12]}")
        elif plain(copy.read_bytes()) != plain(kit):
            problems.append(f"{ours} is not appkit's {theirs} at {sha[:12]} (the commit the release-kit actions "
                            f"are pinned to): take that file, `git -C <appkit> show {sha}:{theirs} > {ours}`")
    return problems


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--root", type=Path, default=Path("."))
    parser.add_argument("--appkit-dir", type=Path, default=None)
    parser.add_argument("--copy", action="append", default=None)
    args = parser.parse_args(argv)
    appkit_dir = args.appkit_dir or (Path(os.environ["APPKIT_DIR"]) if os.environ.get("APPKIT_DIR") else None)
    try:
        problems = check(args.root, args.copy or DEFAULT_COPIES, appkit_dir)
    except Unchecked as e:
        print(f"check-release-kit-copy: not checked: {e}", file=sys.stderr)
        return 2
    except ValueError as e:
        print(f"check-release-kit-copy: {e}", file=sys.stderr)
        return 1
    for problem in problems:
        print(f"check-release-kit-copy: {problem}", file=sys.stderr)
    if not problems:
        print("check-release-kit-copy: every copy is the file of the pinned release kit")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
