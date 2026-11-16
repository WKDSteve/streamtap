#!/usr/bin/env python3
"""Fail when Rust code starts a process without the helper of appkit.

An app starts a process for its own work through `appkit_core::child` (no console
window, the child ends with the app, the start is in the log) and a program of
somebody else through `appkit_hub::launch`. A start written by hand forgets one of
these, or hides a window and nothing else. This reads every tracked `*.rs` file and
reports the lines that start a process themselves:

  * `Command::new` (std and tokio);
  * `.creation_flags(` (the Windows flags of a start);
  * `CreateProcess...` and `ShellExecute...` (the system calls behind them).

Not looked at: comment lines, files in a `tests`, `benches` or `examples` folder,
`build.rs`, and the rest of a file from a `#[cfg(test)]` module at the left margin on
(test code starts what it likes). A `Command` that was imported under another name is
not seen.

    python tools/check-process-starts.py [--allow FILE] [--root DIR]

A file that may start processes itself (the helper, or a place that has not moved
yet) is listed in the allow file (default: `tools/process-starts.allow`, missing: no
exceptions), one per line:

    <path> <count> <reason>

`<count>` is the number of lines found in that file. A file with more or fewer than
its entry says fails, and so does an entry for a file without any: the list can
neither hide a new start nor keep an exception that is gone. `#` starts a comment.

The script knows no repository: an app copies it, with an allow file of its own.

Exit 0: clean. Exit 1: the places are listed. Exit 2: git or the allow file could
not be read.
"""

import argparse
import os
import re
import subprocess
import sys

FOUND = re.compile(
    r"\bCommand::new\b|\.creation_flags\s*\(|\bCreateProcess\w*|\bShellExecute\w*"
)
SKIPPED_FOLDERS = {"tests", "benches", "examples"}
TEST_MODULE = re.compile(r"#\[cfg\(test\)\]\s*$")


def tracked_rust_files(root):
    try:
        out = subprocess.run(
            ["git", "ls-files", "-z", "--", "*.rs"],
            check=True,
            capture_output=True,
            cwd=root,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"cannot list the tracked files: {error}", file=sys.stderr)
        sys.exit(2)
    return [p.decode("utf-8", "surrogateescape") for p in out.split(b"\0") if p]


def looked_at(path):
    """Whether the file is code of the app (not a test, a bench, an example or a build script)."""
    parts = path.split("/")
    return parts[-1] != "build.rs" and not SKIPPED_FOLDERS.intersection(parts[:-1])


def findings(text):
    """The (line number, line) pairs of one file's text that start a process."""
    found = []
    lines = text.split("\n")
    for number, line in enumerate(lines, start=1):
        # `lines[number]` is the line after this one.
        if TEST_MODULE.match(line) and "".join(lines[number : number + 1]).startswith("mod "):
            break
        stripped = line.strip()
        if stripped.startswith("//"):
            continue
        if FOUND.search(line):
            found.append((number, stripped))
    return found


def read_allow(path):
    """The allow file as {path: (count, reason)}; problems as a list of texts."""
    allowed, problems = {}, []
    try:
        with open(path, encoding="utf-8") as handle:
            lines = handle.read().split("\n")
    except FileNotFoundError:
        return allowed, problems
    for number, line in enumerate(lines, start=1):
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split(None, 2)
        if len(parts) < 3 or not parts[1].isdigit():
            problems.append(f"{path}:{number}: expected '<path> <count> <reason>'")
            continue
        if parts[0] in allowed:
            problems.append(f"{path}:{number}: {parts[0]} is listed twice")
            continue
        allowed[parts[0]] = (int(parts[1]), parts[2])
    return allowed, problems


def check(files, allowed):
    """Compare what the files hold ({path: findings}) with the allow list; the texts of the problems."""
    problems = []
    for path in sorted(files):
        found = files[path]
        if path not in allowed:
            for number, line in found:
                problems.append(f"{path}:{number}: starts a process itself: {line}")
            continue
        count = allowed[path][0]
        if len(found) != count:
            problems.append(
                f"{path}: {len(found)} line(s) start a process, the allow file says {count}:"
            )
            for number, line in found:
                problems.append(f"  {path}:{number}: {line}")
    for path in sorted(allowed):
        if path not in files:
            problems.append(f"{path}: in the allow file, but it starts no process (remove the entry)")
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--root", default=".", help="the repository (default: here)")
    parser.add_argument(
        "--allow",
        default=None,
        help="the allow file (default: tools/process-starts.allow below the root)",
    )
    args = parser.parse_args()
    allow_path = args.allow or os.path.join(args.root, "tools", "process-starts.allow")
    allowed, problems = read_allow(allow_path)
    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 2
    files = {}
    for path in tracked_rust_files(args.root):
        if not looked_at(path):
            continue
        try:
            with open(os.path.join(args.root, path), encoding="utf-8") as handle:
                text = handle.read()
        except (OSError, UnicodeDecodeError):
            continue
        found = findings(text)
        if found:
            files[path] = found
    problems = check(files, allowed)
    if problems:
        print("\n".join(problems))
        print(
            "start processes through appkit_core::child (or appkit_hub::launch for a program of "
            "somebody else); see docs/PROCESSES.md of appkit",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
