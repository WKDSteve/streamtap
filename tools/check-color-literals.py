#!/usr/bin/env python3
"""Fail when a hex colour or a colour function is written in the CSS of a page outside the tokens.

Every colour of a page comes from the tokens of appkit-ui (ui-kit/src/tokens.css): a colour written in a component
looks right in one theme and wrong in the other, and does not move with the tokens to the other apps. This reads
the tracked .css and .svelte files under the given folders and refuses a hex colour (#rgb, #rgba, #rrggbb,
#rrggbbaa) or a colour function (rgb(), rgba(), hsl(), hsla(), hwb(), lab(), lch(), oklab(), oklch(), color(), in
any letter case) where a page states a colour:

  * in the value of a CSS declaration (`prop: value;`): a .css file, a <style> block, a style="..." attribute;
  * in a Svelte style directive (`style:color="..."`, `style:color={...}`) and in a style={...} expression;
  * in an SVG paint attribute (fill, stroke, stop-color, flood-color, lighting-color, color).

Not looked at: the <script> part of a component and .ts files (a colour there is a string, not a style), .svg
files (an icon file carries its own colours), named colours (`red`), comments, the text of the markup, quoted
strings and url(...) inside CSS (`content: "#1"`, `url(#fade)`). `color-mix()` over tokens, `transparent` and
`currentColor` pass. The same file runs in appkit and in every app:

    python tools/check-color-literals.py --root ui-kit/src --tokens ui-kit/src/tokens.css   (appkit)
    python tools/check-color-literals.py --root ui/src                                      (an app)
    python tools/check-color-literals.py --root ui/src --tokens ui/src/palette.css          (an app with a palette)

`--tokens` names a file that may hold colours, and may be given more than once: the tokens of appkit-ui and the
palette of an app (generated from the colours of its icon, loaded after tokens.css) are the only such files.

Paths are those of the repository, whatever the current folder; a root may be written with backslashes.

Exit 0: clean. Exit 1: the places are listed (file, line, literal). Exit 2: git could not be asked, or no
.css/.svelte file was found under the roots (a wrong root checks nothing, and that is no pass).
"""

import argparse
import os
import posixpath
import re
import subprocess
import sys

LITERAL = re.compile(
    r"#(?:[0-9a-fA-F]{8}|[0-9a-fA-F]{6}|[0-9a-fA-F]{3,4})\b"
    r"|\b(?:rgba?|hsla?|hwb|lab|lch|oklab|oklch|color)\(",
    re.I,
)

SCRIPT = re.compile(r"<script\b[^>]*>.*?</script\s*>", re.S | re.I)
HTML_COMMENT = re.compile(r"<!--.*?-->", re.S)
STYLE_BLOCK = re.compile(r"<style\b[^>]*>(.*?)</style\s*>", re.S | re.I)
# An attribute value: double-quoted, single-quoted, or a Svelte expression (one level of nested braces).
VALUE = r"(\"[^\"]*\"|'[^']*'|\{(?:[^{}]|\{[^{}]*\})*\})"
STYLE_ATTRIBUTE = re.compile(r"(?<![\w:-])style\s*=\s*" + VALUE, re.I)
STYLE_DIRECTIVE = re.compile(r"(?<![\w-])style:[\w-]+(?:\|important)?\s*=\s*" + VALUE)
PAINT_ATTRIBUTE = re.compile(
    r"(?<![\w:-])(?:fill|stroke|stop-color|flood-color|lighting-color|color)\s*=\s*" + VALUE, re.I
)
EXPRESSION = re.compile(r"\{(?:[^{}]|\{[^{}]*\})*\}")
# What in CSS is no colour even when it looks like one: a quoted string, a comment, the inside of url(...).
CSS_NOISE = re.compile(
    r"\"(?:\\.|[^\"\\\n])*\"|'(?:\\.|[^'\\\n])*'|/\*.*?\*/|\burl\(\s*[^)]*\)", re.S | re.I
)
# A declaration: a property, a colon, and a value up to the end of the declaration. A selector ends with `{` and so
# is no declaration.
DECLARATION = re.compile(r"(?<![\w-])-{0,2}[a-zA-Z][\w-]*\s*:\s*([^;{}]*)(?=[;}])")


def blank(match):
    """The text of a match with everything but its line feeds removed, so offsets and line numbers stay."""
    return re.sub(r"[^\n]", " ", match.group())


def literals_in_css(css, offset):
    """The (offset, literal) pairs of the colours in the declaration values of a piece of CSS."""
    masked = CSS_NOISE.sub(blank, css) + ";"
    found = []
    for declaration in DECLARATION.finditer(masked):
        for literal in LITERAL.finditer(declaration.group(1)):
            found.append((offset + declaration.start(1) + literal.start(), literal.group()))
    return found


def literals_in(value, offset):
    """The (offset, literal) pairs of the colours anywhere in a value (a directive, a paint attribute)."""
    return [(offset + m.start(), m.group()) for m in LITERAL.finditer(value)]


def svelte_literals(text):
    found = []
    text = SCRIPT.sub(blank, text)
    text = HTML_COMMENT.sub(blank, text)
    for block in STYLE_BLOCK.finditer(text):
        found += literals_in_css(block.group(1), block.start(1))
    markup = STYLE_BLOCK.sub(blank, text)
    for attribute in STYLE_ATTRIBUTE.finditer(markup):
        value, start = attribute.group(1), attribute.start(1)
        if value.startswith("{"):
            found += literals_in(value, start)
            continue
        inner = value[1:-1]
        # A quoted style may hold expressions (`style="color: {c}"`): their strings are read as they are, the
        # rest as CSS.
        found += literals_in_css(EXPRESSION.sub(blank, inner), start + 1)
        for expression in EXPRESSION.finditer(inner):
            found += literals_in(expression.group(), start + 1 + expression.start())
    for pattern in (STYLE_DIRECTIVE, PAINT_ATTRIBUTE):
        for attribute in pattern.finditer(markup):
            found += literals_in(attribute.group(1), attribute.start(1))
    return found


def problems(path, text):
    """The (line, literal) pairs of the colour literals of one file, in the order of the file."""
    found = svelte_literals(text) if path.endswith(".svelte") else literals_in_css(text, 0)
    return [(text.count("\n", 0, at) + 1, literal) for at, literal in sorted(set(found))]


def normal(path):
    """A repository path as git writes it: forward slashes, no leading `./`, no trailing slash."""
    path = posixpath.normpath(path.replace("\\", "/"))
    return "" if path == "." else path


def checked(path, roots, tokens):
    """Is this tracked file one to read: a stylesheet or component under a root, and none of the token files?

    `tokens` is one path or a list of them (the tokens of appkit-ui, the palette of an app)."""
    if isinstance(tokens, str):
        tokens = [tokens]
    allowed = {normal(t) for t in tokens if t}
    inside = any(path.startswith(normal(root) + "/") for root in roots)
    return inside and path.endswith((".css", ".svelte")) and path not in allowed


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True).stdout


def tracked_files():
    """The top of the repository and the tracked files, as paths from that top."""
    try:
        top = git("rev-parse", "--show-toplevel").decode("utf-8").strip()
        out = git("-C", top, "ls-files", "-z")
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"cannot list the tracked files: {error}", file=sys.stderr)
        sys.exit(2)
    return top, [p.decode("utf-8", "surrogateescape") for p in out.split(b"\0") if p]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--root", action="append", default=[], help="a folder to read (repeatable)")
    parser.add_argument(
        "--tokens", action="append", default=[], help="a file that may hold colours (repeatable: tokens, palette)"
    )
    args = parser.parse_args(argv)
    roots = args.root or ["ui/src"]
    top, files = tracked_files()
    read = bad = 0
    for path in files:
        if not checked(path, roots, args.tokens):
            continue
        try:
            with open(os.path.join(top, path), encoding="utf-8") as handle:
                text = handle.read()
        except (OSError, UnicodeDecodeError):
            continue
        read += 1
        for line, literal in problems(path, text):
            bad += 1
            print(f"{path}:{line}: {literal}")
    if read == 0:
        print(f"no tracked .css or .svelte file under {', '.join(roots)}: nothing was checked", file=sys.stderr)
        return 2
    if bad:
        print(f"{bad} colour literal(s) outside the tokens: use a token of appkit-ui", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
