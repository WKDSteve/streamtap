#!/usr/bin/env python3
"""Check the rules the workflows of .github/workflows must keep.

    python tools/check-workflows.py [--online] [--dir .github/workflows]

The workflows run on the author's self-hosted Windows runner of a private repository and publish
releases, so a few rules are kept by a check instead of by memory (text level, no YAML library):

  * every `uses:` is pinned by a full commit and names its tag in a comment (NREL-03);
  * all actions of appkit's release-kit use the same commit (streamtap has no appkit crate, so
    this takes the place of the Cargo.lock comparison that tasks-desktop's check-appkit-pin.py does);
  * every job runs on `[self-hosted, windows]` and has a timeout;
  * no workflow reacts to a pull request (it would run its own workflow on the author's machine);
  * every checkout keeps no credentials (`persist-credentials: false`);
  * release.yml starts on a tag `v*` only, writes (`contents: write`) in the publish job only, never
    publishes (no `draft: false`, no `gh release create/edit/delete`, no `git tag/push`: the draft is
    made by the kit's publish action, NREL-07) and checks tag, main and a green CI before building;
  * rolling.yml starts after the workflow `ci` on `main`, only builds for the tip of main, and
    publishes to the tag `latest`.

With --online each action's tag is asked of its repository (`git ls-remote`) and must name the
pinned commit now: a moved tag is noticed.

Exit 0: all rules hold. Exit 1: what is wrong is listed. Exit 2: --online could not ask (not a pass).
"""

import argparse
import pathlib
import re
import subprocess
import sys

APPKIT = "WKDSteve/appkit"
USES = re.compile(r"^(?P<indent>\s*)-?\s*uses:\s*(?P<ref>\S+)(?P<rest>.*)$", re.M)
WRITE_COMMANDS = re.compile(r"\bgh\s+release\s+(create|edit|delete|upload)\b|\bgit\s+(tag|push)\b|\bgh\s+api\b[^\n]*-X\s+(POST|PATCH|PUT|DELETE)")


def action_pins(text):
    """Yield (repo, path, ref, comment) of every `uses:` of the text."""
    for match in USES.finditer(text):
        ref = match.group("ref").strip("\"'")
        if ref.startswith("./"):
            yield ref, "", "", match.group("rest")
            continue
        name, _, version = ref.partition("@")
        parts = name.split("/")
        repo = "/".join(parts[:2])
        yield repo, "/".join(parts[2:]), version, match.group("rest")


def job_blocks(text):
    """Split the text of a workflow into (job name, text) of its jobs."""
    head = re.search(r"^jobs:\s*$", text, re.M)
    if not head:
        return []
    body = text[head.end():]
    parts = re.split(r"^  ([A-Za-z0-9_-]+):\s*$", body, flags=re.M)
    return [(parts[i], parts[i + 1]) for i in range(1, len(parts) - 1, 2)]


def without_comments(text):
    """The text without its comment lines (they may talk about the commands they forbid)."""
    return re.sub(r"^\s*#.*$", "", text, flags=re.M)


def check_common(name, text):
    problems = []
    appkit = set()
    for repo, path, ref, rest in action_pins(text):
        where = f"{name}: {repo}{'/' + path if path else ''}@{ref}"
        if not path and not ref and repo.startswith("./"):
            continue
        if not re.fullmatch(r"[0-9a-f]{40}", ref):
            problems.append(f"{where}: not pinned by a full commit (NREL-03)")
        elif not re.match(r"\s*#\s*v[0-9]", rest):
            problems.append(f"{where}: no tag in the comment after the commit")
        if repo.lower() == APPKIT.lower():
            appkit.add(ref)
    if len(appkit) > 1:
        problems.append(f"{name}: appkit actions are pinned to different commits: {sorted(appkit)}")
    if re.search(r"^\s*pull_request(_target)?\s*:", text, re.M):
        problems.append(f"{name}: reacts to a pull request (it would run on the self-hosted runner)")
    jobs = job_blocks(text)
    if not jobs:
        problems.append(f"{name}: no jobs found")
    for job, body in jobs:
        runs_on = re.findall(r"^    runs-on:\s*(.+?)\s*$", body, re.M)
        if runs_on != ["[self-hosted, windows]"]:
            problems.append(f"{name}: job {job} runs on {runs_on or 'nothing named'}, not [self-hosted, windows]")
        if not re.search(r"^    timeout-minutes:\s*\d+\s*$", body, re.M):
            problems.append(f"{name}: job {job} has no timeout-minutes")
    checkouts = len(re.findall(r"uses:\s*actions/checkout@", text))
    keeps = len(re.findall(r"persist-credentials:\s*false", text))
    if checkouts != keeps:
        problems.append(f"{name}: {checkouts} checkout(s) but {keeps} with persist-credentials: false")
    return problems


def check_release(name, text):
    problems = []
    code = without_comments(text)
    on = re.search(r"^on:\s*\n((?:[ \t]+.*\n|\s*\n)+)", text, re.M)
    on_text = on.group(1) if on else ""
    if not re.search(r"^\s+push:\s*\n\s+tags:\s*\[\"v\*\"\]\s*$", on_text, re.M):
        problems.append(f"{name}: must start on `push: tags: [\"v*\"]`")
    for other in ("branches:", "workflow_dispatch", "schedule", "workflow_run", "release:"):
        if other in on_text:
            problems.append(f"{name}: starts on more than a tag ({other.strip(':')})")
    if re.search(r"draft\s*[=:]\s*false", text):
        problems.append(f"{name}: publishes a release (draft false): a release stays a draft for a person (NREL-07)")
    if WRITE_COMMANDS.search(without_comments(text)):
        problems.append(f"{name}: a step makes or changes a release or a ref itself: only the kit's publish action does (NREL-07)")
    for needle, why in (
        ("merge-base --is-ancestor", "the commit of the tag must be on main"),
        ("gh run list", "the CI of this commit must be green (NREL-08)"),
        ("isDraft", "a published release is never cut again (NREL-10)"),
        ("tauri.conf.json", "the version of tauri.conf.json must be the tag's"),
        ("release-kit/publish@", "the draft is made by the kit's publish action"),
    ):
        if needle not in code:
            problems.append(f"{name}: missing check: {why}")
    for job, body in job_blocks(text):
        writes = "contents: write" in body
        if job == "publish" and not writes:
            problems.append(f"{name}: job publish needs contents: write")
        if job != "publish" and writes:
            problems.append(f"{name}: job {job} has contents: write; only publish may write")
    if re.search(r"^permissions:\s*\n\s+contents:\s*write", text, re.M):
        problems.append(f"{name}: write permission for the whole workflow")
    return problems


def check_rolling(name, text):
    problems = []
    code = without_comments(text)
    if not re.search(r"workflow_run:\s*\n\s+workflows:\s*\[ci\]\s*\n\s+types:\s*\[completed\]\s*\n\s+branches:\s*\[main\]", text):
        problems.append(f"{name}: must start on `workflow_run` of `ci` (completed, branch main)")
    for other in ("push:", "workflow_dispatch", "schedule"):
        if re.search(rf"^\s+{other}", text, re.M):
            problems.append(f"{name}: starts on more than a finished CI of main ({other.strip(':')})")
    for needle, why in (
        ("release-kit/prepare@", "only the tip of main is built (prepare)"),
        ("release-kit/publish@", "the file goes up by the kit's publish action"),
        ("tag: latest", "the build goes to the pre-release `latest`"),
        ("needs.prepare.outputs.current == 'true'", "the build must wait for prepare's word that it is the tip"),
        ("if: ${{ github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' }}", "only a green CI builds"),
        ("-Rolling", "the zip is packed as the rolling build (version label with the commit)"),
    ):
        if needle not in code:
            problems.append(f"{name}: missing: {why}")
    if WRITE_COMMANDS.search(without_comments(text)):
        problems.append(f"{name}: a step makes or changes a release or a ref itself: only the kit's actions do")
    if re.search(r"draft\s*[=:]\s*false", text):
        problems.append(f"{name}: publishes a draft")
    return problems


def check_all(texts):
    """texts maps a file name to its text; returns the list of problems."""
    problems = []
    for name, text in sorted(texts.items()):
        problems += check_common(name, text)
        if name == "release.yml":
            problems += check_release(name, text)
        if name == "rolling.yml":
            problems += check_rolling(name, text)
    shas = {}
    for name, text in texts.items():
        for repo, _path, ref, _rest in action_pins(text):
            if repo.lower() == APPKIT.lower():
                shas.setdefault(ref, []).append(name)
    if len(shas) > 1:
        problems.append(f"appkit actions in different workflows are pinned to different commits: {sorted(shas)}")
    return problems


def tag_of(rest):
    match = re.match(r"\s*#\s*(v[0-9][0-9A-Za-z.+-]*)", rest)
    return match.group(1) if match else None


def online(texts):
    """Ask every repository where the tag named in the comment points now; a list of problems."""
    problems = []
    seen = {}
    for name, text in sorted(texts.items()):
        for repo, _path, ref, rest in action_pins(text):
            tag = tag_of(rest)
            if not tag or not re.fullmatch(r"[0-9a-f]{40}", ref):
                continue
            key = (repo, tag)
            if key not in seen:
                result = subprocess.run(
                    ["git", "ls-remote", "--tags", f"https://github.com/{repo}", f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}"],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if result.returncode != 0:
                    raise RuntimeError(f"git ls-remote {repo} failed: {result.stderr.strip()}")
                lines = dict(reversed(line.split("\t")) for line in result.stdout.strip().splitlines())
                seen[key] = lines.get(f"refs/tags/{tag}^{{}}") or lines.get(f"refs/tags/{tag}")
            if seen[key] != ref:
                problems.append(f"{name}: {repo}@{ref} but the tag {tag} names {seen[key]} now")
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dir", default=".github/workflows")
    parser.add_argument("--online", action="store_true")
    args = parser.parse_args()
    folder = pathlib.Path(args.dir)
    texts = {p.name: p.read_text(encoding="utf-8") for p in sorted(folder.glob("*.yml"))}
    if not texts:
        print(f"no workflows in {folder}", file=sys.stderr)
        return 2
    problems = check_all(texts)
    if args.online:
        try:
            problems += online(texts)
        except (RuntimeError, OSError) as error:
            print(f"cannot ask the repositories: {error}", file=sys.stderr)
            return 2
    for problem in problems:
        print(problem)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
