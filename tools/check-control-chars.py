#!/usr/bin/env python3
"""Fail when a tracked text file holds a control character.

A tab and a backspace in the path of a README (an install folder) went by
unseen; a bidirectional override in source hides what the code does (the
"Trojan Source" trick). This reads every file `git ls-files` lists, skips
binary files (a NUL byte, or not UTF-8), and rejects:

  * C0 controls except line feed and carriage return, and DEL; a TAB too, except in the files
    whose format needs it (Makefile, *.mk, *.tsv, .gitmodules);
  * the C1 range (U+0080 to U+009F);
  * the line and paragraph separators (U+2028, U+2029);
  * the invisible format characters: zero width (U+200B to U+200F, U+2060 to
    U+2064), the bidirectional controls (U+202A to U+202E, U+2066 to U+2069)
    and the byte order mark (U+FEFF; one at the very start of a file is a
    BOM and passes).

    python tools/check-control-chars.py [--allow PATH ...]

A file that holds such a character on purpose (a test corpus of hostile
text) is named in ALLOWED below, with the reason; write the character as an
escape in the source instead where the language allows it.

Exit 0: clean. Exit 1: the places are listed (file, line, code point).
Exit 2: git could not be asked.
"""

import argparse
import os
import re
import subprocess
import sys

# Code point ranges (inclusive) that are refused; kept as numbers so that this
# file holds none of the characters it forbids.
RANGES = [
    (0x00, 0x09),
    (0x0B, 0x0C),
    (0x0E, 0x1F),
    (0x7F, 0x9F),
    # The soft hyphen: invisible unless a line breaks there, and the one such character the other
    # ranges miss; a source file must not carry it (a regex that lists it would change unseen).
    (0xAD, 0xAD),
    (0x200B, 0x200F),
    (0x2028, 0x2029),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x2069),
    (0xFEFF, 0xFEFF),
]
BAD = re.compile(
    "[" + "".join(re.escape(chr(a)) + "-" + re.escape(chr(b)) for a, b in RANGES) + "]"
)

# Files that carry such characters on purpose.
ALLOWED: dict[str, str] = {}

# A TAB belongs to the format of these files (rule NREL-14); everywhere else it is refused, since
# a tab in a text is nearly always a lost backslash-t (rule APP-02).
TAB_FILE_NAMES = ("Makefile", "GNUmakefile", ".gitmodules")
TAB_FILE_SUFFIXES = (".mk", ".tsv")


def tracked_files():
    try:
        out = subprocess.run(
            ["git", "ls-files", "-z"], check=True, capture_output=True
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"cannot list the tracked files: {error}", file=sys.stderr)
        sys.exit(2)
    return [p.decode("utf-8", "surrogateescape") for p in out.split(b"\0") if p]


def name_problems(path):
    """The code points of the bad characters in a file name (the path as git lists it)."""
    return [ord(match.group()) for match in BAD.finditer(path)]


def problems(path, data):
    """The (line, code point) pairs of the bad characters of one file's bytes."""
    if b"\0" in data:
        return []
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        return []
    # A byte order mark at the start of a file is the way some editors say UTF-8.
    if text.startswith(chr(0xFEFF)):
        text = text[1:]
    tab_ok = os.path.basename(path) in TAB_FILE_NAMES or path.endswith(TAB_FILE_SUFFIXES)
    found = []
    # Split on \n only: a lone \r or a CRLF is a line ending, not a finding.
    for number, line in enumerate(text.split("\n"), start=1):
        for match in BAD.finditer(line):
            code = ord(match.group())
            if code == 0x09 and tab_ok:
                continue
            found.append((number, code))
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--allow", nargs="*", default=[], help="extra paths to skip")
    args = parser.parse_args()
    skip = set(ALLOWED) | set(args.allow)
    bad = 0
    for path in tracked_files():
        if path in skip:
            continue
        for code in name_problems(path):
            bad += 1
            # repr: the name itself must not act on the terminal.
            print(f"{path!r}: the name holds U+{code:04X}")
        try:
            with open(path, "rb") as handle:
                data = handle.read()
        except OSError:
            # A submodule, a symlink to nowhere: nothing to read.
            continue
        for line, code in problems(path, data):
            bad += 1
            print(f"{path}:{line}: U+{code:04X}")
    if bad:
        print(f"{bad} control character(s) in tracked text files", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
