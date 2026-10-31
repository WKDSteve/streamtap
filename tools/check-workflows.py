#!/usr/bin/env python3
"""Check the rules the workflows of .github/workflows must keep.

    python tools/check-workflows.py [--online] [--dir .github/workflows]

The workflows run on the author's self-hosted Windows runner of a private repository and publish
releases, so a few rules are kept by a check instead of by memory. The files are read by a small
parser of the YAML these workflows use (block mappings and lists, flow lists, block scalars; no
anchors, no flow mappings, no multi-line plain scalars): anything it does not know is a problem, never
a pass. The rules are then about the structure, so a comment, a different spelling of a trigger or a
renamed file does not get around them:

  * only ci.yml, release.yml and rolling.yml exist (a new workflow gets its rules here first);
  * every `uses:` is pinned by a full commit and names its tag in a comment (NREL-03);
  * all actions of appkit's release-kit use the same commit (streamtap has no appkit crate, so
    this takes the place of the Cargo.lock comparison that tasks-desktop's check-appkit-pin.py does);
  * every job runs on `[self-hosted, windows]` and has a timeout;
  * no workflow reacts to a pull request or a comment (it would run on the author's machine);
  * the token: `permissions: contents: read` for the whole workflow, `contents: write` only in the
    jobs that publish (release: publish; rolling: prepare, publish), nothing else, no write-all;
  * every checkout keeps no credentials (`persist-credentials: false`);
  * no `continue-on-error`, no `always()`/`cancelled()`/`failure()` in a condition;
  * release.yml starts on a tag `v*` only; checks tag, versions (app, sink, host), main, a green CI and
    a not yet published release before building (and a failing `gh` is not "no release"); build and
    publish wait for that check; it never publishes (no `draft` input, no `gh release create/edit/delete`,
    no `git tag/push`, no API write: the draft is made by the kit's publish action, NREL-07);
  * rolling.yml starts after the workflow `ci` on `main`, only builds for the tip of main, publishes
    to the tag `latest`, a newer build cancels an older one but nothing cancels a running publish, and a
    publish that waited behind a newer one stops when main has moved on.

With --online each action's tag is asked of its repository (`git ls-remote`) and must name the
pinned commit now: a moved tag is noticed.

Exit 0: all rules hold. Exit 1: what is wrong is listed. Exit 2: --online could not ask (not a pass).
"""

import argparse
import os
import pathlib
import re
import subprocess
import sys

APPKIT = "WKDSteve/appkit"
USES = re.compile(r"^(?P<indent>\s*)-?\s*uses:\s*(?P<ref>\S+)(?P<rest>.*)$", re.M)
WRITE_COMMANDS = re.compile(
    r"\bgh\s+release\s+(?:create|edit|delete|upload)\b"
    r"|\bgit\s+(?:-\S+\s+(?:\S+\s+)?)*(?:tag|push)\b(?!\s+(?:-l\b|--list\b))"
    r"|\bgh\s+api\b[^\n]*(?:\s-X\b|\s-X[A-Za-z]|\s--method\b|\s-f\b|\s-F\b|\s--field\b|\s--raw-field\b|\s--input\b)"
    r"|\b(?:curl|wget|Invoke-RestMethod|Invoke-WebRequest|irm|iwr)\b[^\n]*api\.github\.com"
)
DRAFT_OFF = re.compile(r"draft\s*[=:]\s*['\"]?(?:false|no|off|0)\b", re.I)
EXPECTED = ("ci.yml", "release.yml", "rolling.yml")
# Events that hand the workflow to someone who is not the maintainer.
OUTSIDE_EVENTS = {"pull_request", "pull_request_target", "pull_request_review", "pull_request_review_comment", "issue_comment", "issues", "discussion", "discussion_comment", "fork", "watch"}
ALLOWED_EVENTS = {"ci.yml": {"push", "workflow_dispatch"}, "release.yml": {"push"}, "rolling.yml": {"workflow_run"}}
WRITERS = {"ci.yml": set(), "release.yml": {"publish"}, "rolling.yml": {"prepare", "publish"}}
TIP_IF = "${{ steps.tip.outputs.current == 'true' }}"


class YamlError(ValueError):
    pass


# ---------------------------------------------------------------- the parser

def _strip_comment(text):
    """The text without a trailing ` # comment` (a # inside quotes stays)."""
    quote = None
    for i, c in enumerate(text):
        if quote:
            if c == quote:
                quote = None
        elif c in "\"'" and (i == 0 or text[i - 1] in " \t[,:"):
            quote = c
        elif c == "#" and (i == 0 or text[i - 1] in " \t"):
            return text[:i].rstrip()
    return text.rstrip()


def _tokens(text):
    """(indent, text, block) per content line; `block` is the raw text of a `|` or `>` scalar."""
    lines = text.replace("\r\n", "\n").split("\n")
    tokens = []
    i = 0
    while i < len(lines):
        raw = lines[i]
        i += 1
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        if "\t" in raw[: len(raw) - len(raw.lstrip())]:
            raise YamlError(f"line {i}: a tab in the indent")
        indent = len(raw) - len(raw.lstrip(" "))
        body = _strip_comment(raw.strip())
        block = None
        if re.search(r"(?:^|:\s+|^-\s+)[|>][+-]?[0-9]?$", body):
            body = re.sub(r"\s*[|>][+-]?[0-9]?$", "", body)
            collected = []
            while i < len(lines) and (not lines[i].strip() or len(lines[i]) - len(lines[i].lstrip(" ")) > indent):
                collected.append(lines[i])
                i += 1
            block = "\n".join(collected)
            # `key: |` leaves `key:`; the KEY pattern sees an empty value and the block wins.
        tokens.append([indent, body, block])
    return tokens


def _scalar(value):
    value = value.strip()
    if not value:
        return None
    if value[0] in "&*!%@`|>":
        raise YamlError(f"unsupported value: {value}")
    if value[0] == "{":
        raise YamlError(f"a flow mapping is not supported: {value}")
    if value[0] == "[":
        if not value.endswith("]"):
            raise YamlError(f"a flow list over several lines is not supported: {value}")
        inner = value[1:-1].strip()
        return [_scalar(part) for part in _split_flow(inner)] if inner else []
    if value[0] in "\"'":
        if len(value) < 2 or value[-1] != value[0]:
            raise YamlError(f"a quoted value that does not end with its quote: {value}")
        return value[1:-1]
    return value


def _split_flow(inner):
    parts, depth, quote, current = [], 0, None, ""
    for c in inner:
        if quote:
            current += c
            if c == quote:
                quote = None
        elif c in "\"'":
            quote = c
            current += c
        elif c == "[":
            raise YamlError("a nested flow list is not supported")
        elif c == "," and depth == 0:
            parts.append(current)
            current = ""
        else:
            current += c
    parts.append(current)
    return parts


KEY = re.compile(r"^(\"[^\"]*\"|'[^']*'|[^\s:'\"-][^:]*?|-[^\s:][^:]*?):(?:\s+(.*))?$")


def _parse_block(tokens, i, indent):
    if tokens[i][1] == "-" or tokens[i][1].startswith("- "):
        return _parse_seq(tokens, i, indent)
    return _parse_map(tokens, i, indent)


def _parse_seq(tokens, i, indent):
    items = []
    while i < len(tokens) and tokens[i][0] == indent and (tokens[i][1] == "-" or tokens[i][1].startswith("- ")):
        rest = tokens[i][1][1:].lstrip()
        if not rest:
            if i + 1 < len(tokens) and tokens[i + 1][0] > indent:
                item, i = _parse_block(tokens, i + 1, tokens[i + 1][0])
            else:
                item, i = None, i + 1
            items.append(item)
            continue
        offset = len(tokens[i][1]) - len(rest)
        tokens[i] = [indent + offset, rest, tokens[i][2]]
        if KEY.match(rest):
            item, i = _parse_map(tokens, i, indent + offset)
        else:
            item, i = _scalar(rest), i + 1
        items.append(item)
    return items, i


def _parse_map(tokens, i, indent):
    result = {}
    while i < len(tokens) and tokens[i][0] == indent:
        text, block = tokens[i][1], tokens[i][2]
        match = KEY.match(text)
        if not match:
            raise YamlError(f"cannot read: {text}")
        key = match.group(1).strip("\"'")
        value = match.group(2)
        if key in result:
            raise YamlError(f"the key {key} twice")
        i += 1
        if block is not None:
            result[key] = block
        elif value:
            result[key] = _scalar(value)
        elif i < len(tokens) and tokens[i][0] > indent:
            result[key], i = _parse_block(tokens, i, tokens[i][0])
        elif i < len(tokens) and tokens[i][0] == indent and (tokens[i][1] == "-" or tokens[i][1].startswith("- ")):
            result[key], i = _parse_seq(tokens, i, indent)
        else:
            result[key] = None
    return result, i


def parse_yaml(text):
    tokens = _tokens(text)
    if not tokens:
        raise YamlError("empty")
    value, i = _parse_block(tokens, 0, tokens[0][0])
    if i != len(tokens):
        raise YamlError(f"cannot read near: {tokens[i][1]}")
    return value


# ---------------------------------------------------------------- helpers

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


def code_of(shell):
    """A run script without its comments (they may talk about the commands they forbid)."""
    return re.sub(r"(?m)(?:^|[ \t])#.*$", "", shell)


def jobs_of(wf):
    jobs = wf.get("jobs") if isinstance(wf, dict) else None
    return {k: v for k, v in jobs.items() if isinstance(v, dict)} if isinstance(jobs, dict) else {}


def steps_of(job):
    steps = job.get("steps")
    return [s for s in steps if isinstance(s, dict)] if isinstance(steps, list) else []


def run_of(job):
    """All run scripts of a job, comments removed."""
    return "\n".join(code_of(str(s["run"])) for s in steps_of(job) if "run" in s)


def all_run(wf):
    return "\n".join(run_of(job) for job in jobs_of(wf).values())


def triggers_of(wf):
    on = wf.get("on")
    if isinstance(on, dict):
        return on
    if isinstance(on, list):
        return {str(k): None for k in on}
    if isinstance(on, str):
        return {on: None}
    return {}


def needs_of(job):
    needs = job.get("needs")
    if isinstance(needs, list):
        return set(needs)
    return {needs} if isinstance(needs, str) else set()


def uses_of(step):
    return str(step.get("uses", ""))


def with_of(step):
    w = step.get("with")
    return w if isinstance(w, dict) else {}


def publish_step(job, kind="publish"):
    for step in steps_of(job):
        if uses_of(step).startswith(f"{APPKIT}/release-kit/{kind}@"):
            return step
    return None


# ---------------------------------------------------------------- the rules

def check_common(name, text, wf):
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
    if wf is None:
        return problems

    events = triggers_of(wf)
    if not events:
        problems.append(f"{name}: no trigger (`on:`) found")
    for event in sorted(events):
        if event in OUTSIDE_EVENTS:
            problems.append(f"{name}: reacts to a pull request or an outside event ({event}); it would run on the self-hosted runner")
    if name == "ci.yml":
        for event in sorted(set(events) - ALLOWED_EVENTS["ci.yml"]):
            problems.append(f"{name}: starts on {event}, which only push and workflow_dispatch may")

    jobs = jobs_of(wf)
    if not jobs:
        problems.append(f"{name}: no jobs found")
    for job, body in jobs.items():
        runs_on = body.get("runs-on")
        if runs_on != ["self-hosted", "windows"]:
            problems.append(f"{name}: job {job} runs on {runs_on or 'nothing named'}, not [self-hosted, windows]")
        if not re.fullmatch(r"[0-9]+", str(body.get("timeout-minutes", ""))):
            problems.append(f"{name}: job {job} has no timeout-minutes")
        if "continue-on-error" in body or any("continue-on-error" in s for s in steps_of(body)):
            problems.append(f"{name}: job {job} uses continue-on-error: a failure must stop the run")
        conditions = [str(body.get("if", ""))] + [str(s.get("if", "")) for s in steps_of(body)]
        if any(re.search(r"\b(always|cancelled|failure)\s*\(", c) for c in conditions):
            problems.append(f"{name}: job {job} runs after a failure (always/cancelled/failure in an if): a failed check must stop the run")
        for number, step in enumerate(steps_of(body), 1):
            if uses_of(step).startswith("actions/checkout@") and str(with_of(step).get("persist-credentials")) != "false":
                problems.append(f"{name}: job {job} step {number}: a checkout without persist-credentials: false")

    # The token: read for the whole workflow, write in the jobs that publish and nowhere else.
    top = wf.get("permissions")
    if top != {"contents": "read"}:
        problems.append(f"{name}: the permissions of the whole workflow must be exactly `contents: read` (found {top!r})")
    writers = WRITERS.get(name, set())
    for job, body in jobs.items():
        perms = body.get("permissions")
        if perms is None:
            continue
        if not isinstance(perms, dict):
            problems.append(f"{name}: job {job} has permissions {perms!r}; list them one by one")
            continue
        for key, value in perms.items():
            if key not in ("contents", "actions") or value not in ("read", "write"):
                problems.append(f"{name}: job {job} has the permission {key}: {value}, which no job here needs")
            elif value == "write" and not (key == "contents" and job in writers):
                problems.append(f"{name}: job {job} has {key}: write; only {' and '.join(sorted(writers)) or 'no job'} may write")
        if job in writers and perms.get("contents") != "write":
            problems.append(f"{name}: job {job} needs contents: write")
    for job in writers:
        if job in jobs and jobs[job].get("permissions") is None:
            problems.append(f"{name}: job {job} needs contents: write")
    return problems


def check_release(name, text, wf):
    problems = []
    events = triggers_of(wf)
    push = events.get("push")
    for other in sorted(set(events) - {"push"}):
        problems.append(f"{name}: starts on more than a tag ({other})")
    if isinstance(push, dict):
        for other in sorted(set(push) - {"tags"}):
            problems.append(f"{name}: starts on more than a tag ({other})")
    if not (isinstance(push, dict) and push.get("tags") == ["v*"]):
        problems.append(f"{name}: must start on `push: tags: [\"v*\"]`")

    jobs = jobs_of(wf)
    code = all_run(wf)
    publish = jobs.get("publish", {})
    if any("draft" in with_of(s) for job in jobs.values() for s in steps_of(job)) or DRAFT_OFF.search(code):
        problems.append(f"{name}: publishes a release (draft false or a draft input): a release stays a draft for a person (NREL-07)")
    if WRITE_COMMANDS.search(code):
        problems.append(f"{name}: a step makes or changes a release or a ref itself: only the kit's publish action does (NREL-07)")
    check = run_of(jobs.get("check", {}))
    for pattern, why in (
        (r'(?m)^\s*if\s+!\s+git merge-base --is-ancestor "\$GITHUB_SHA" origin/main\b', "the commit of the tag must be on main"),
        (r'(?m)^\s*green="\$\(gh run list\b[^\n]*--workflow ci\.yml\b[^\n]*--commit "\$GITHUB_SHA"[^\n]*--status success\b', "the CI of this commit must be green (NREL-08)"),
        (r'(?m)^\s*if answer="\$\(gh release view "\$tag"[^\n]*--json isDraft\b', "a published release is never cut again (NREL-10)"),
        (r"release not found", "only `release not found` counts as no release; any other failure of gh stops (NREL-10)"),
        (r"tauri\.conf\.json", "the version of tauri.conf.json must be the tag's"),
        (r"for member in streamtap-sink streamtap-host\b[\s\S]*crates/\$member/Cargo\.toml", "the versions of the sink and the host must be the tag's"),
    ):
        if not re.search(pattern, check):
            problems.append(f"{name}: missing check: {why}")
    if "check" not in jobs:
        problems.append(f"{name}: no job check")
    if not needs_of(jobs.get("build", {})) >= {"check"}:
        problems.append(f"{name}: job build must need check")
    if not needs_of(publish) >= {"check", "build"}:
        problems.append(f"{name}: job publish must need check and build")
    step = publish_step(publish)
    if step is None:
        problems.append(f"{name}: missing check: the draft is made by the kit's publish action")
    else:
        if with_of(step).get("tag") != "${{ github.ref_name }}":
            problems.append(f"{name}: the publish action must publish the tag of this run (`tag: ${{{{ github.ref_name }}}}`)")
        if str(with_of(step).get("expect")) != "1":
            problems.append(f"{name}: the publish action must expect exactly one file")
    return problems


def check_rolling(name, text, wf):
    problems = []
    events = triggers_of(wf)
    for other in sorted(set(events) - {"workflow_run"}):
        problems.append(f"{name}: starts on more than a finished CI of main ({other})")
    if events.get("workflow_run") != {"workflows": ["ci"], "types": ["completed"], "branches": ["main"]}:
        problems.append(f"{name}: must start on `workflow_run` of `ci` (completed, branch main)")

    jobs = jobs_of(wf)
    prepare, build, publish = jobs.get("prepare", {}), jobs.get("build", {}), jobs.get("publish", {})
    code = all_run(wf)
    if "concurrency" in wf:
        problems.append(f"{name}: workflow-level concurrency would cancel a publish that is running; put it on the jobs")
    if build.get("concurrency") != {"group": "rolling-build-${{ needs.prepare.outputs.current }}", "cancel-in-progress": "true"}:
        problems.append(f"{name}: job build must be cancelled by a newer build (concurrency rolling-build, cancel-in-progress: true)")
    if publish.get("concurrency") != {"group": "rolling-publish", "cancel-in-progress": "false"}:
        problems.append(f"{name}: job publish must never be cancelled (concurrency rolling-publish, cancel-in-progress: false)")
    if prepare.get("if") != "${{ github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' }}":
        problems.append(f"{name}: missing: only a green CI builds (the if of prepare)")
    if build.get("if") != "${{ needs.prepare.outputs.current == 'true' }}":
        problems.append(f"{name}: missing: the build must wait for prepare's word that it is the tip")
    if not needs_of(build) >= {"prepare"}:
        problems.append(f"{name}: job build must need prepare")
    if not needs_of(publish) >= {"prepare", "build"}:
        problems.append(f"{name}: job publish must need prepare and build")
    step = publish_step(prepare, "prepare")
    if step is None:
        problems.append(f"{name}: missing: only the tip of main is built (prepare)")
    elif with_of(step).get("sha") != "${{ github.event.workflow_run.head_sha }}":
        problems.append(f"{name}: prepare must be asked about the commit of the CI run (head_sha)")
    step = publish_step(publish)
    if step is None:
        problems.append(f"{name}: missing: the file goes up by the kit's publish action")
    else:
        if with_of(step).get("tag") != "latest":
            problems.append(f"{name}: missing: the build goes to the pre-release `latest`")
        if with_of(step).get("sha") != "${{ needs.prepare.outputs.sha }}":
            problems.append(f"{name}: the publish action must name the commit prepare checked (sha)")
    for job_name in ("build", "publish"):
        for step in steps_of(jobs.get(job_name, {})):
            if uses_of(step).startswith("actions/checkout@") and with_of(step).get("ref") != "${{ needs.prepare.outputs.sha }}":
                problems.append(f"{name}: job {job_name}: the checkout must take the commit prepare checked (ref)")
    if not re.search(r"package-release\.ps1\b[^\n]*\s-Rolling\b[^\n]*-Sha\b", run_of(build)):
        problems.append(f"{name}: missing: the zip is packed as the rolling build (-Rolling, with -Sha)")
    # A publish that waited behind a newer one stops when main has moved on.
    steps = steps_of(publish)
    tip = [s for s in steps if s.get("id") == "tip"]
    if not tip or "git/ref/heads/main" not in str(tip[0].get("run", "")):
        problems.append(f"{name}: publish must ask first whether main is still the commit it publishes (step tip)")
    else:
        for step in steps[steps.index(tip[0]) + 1:]:
            if step.get("if") != TIP_IF:
                problems.append(f"{name}: publish: every step after `tip` must run only if it is still the tip")
                break
    if WRITE_COMMANDS.search(code):
        problems.append(f"{name}: a step makes or changes a release or a ref itself: only the kit's actions do")
    if any("draft" in with_of(s) for job in jobs.values() for s in steps_of(job)) or DRAFT_OFF.search(code):
        problems.append(f"{name}: publishes a draft")
    return problems


def check_all(texts):
    """texts maps a file name to its text; returns the list of problems."""
    problems = []
    for name, text in sorted(texts.items()):
        if name not in EXPECTED:
            problems.append(f"{name}: an unexpected workflow file (only {', '.join(EXPECTED)}; give a new one its rules here first)")
        try:
            wf = parse_yaml(text)
            if not isinstance(wf, dict):
                raise YamlError("not a mapping")
        except YamlError as error:
            problems.append(f"{name}: cannot read it ({error}); the checker knows block mappings and lists, flow lists and block scalars only")
            wf = None
        problems += check_common(name, text, wf)
        if wf is not None and name == "release.yml":
            problems += check_release(name, text, wf)
        if wf is not None and name == "rolling.yml":
            problems += check_rolling(name, text, wf)
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
    env = dict(os.environ, GIT_TERMINAL_PROMPT="0")
    for name, text in sorted(texts.items()):
        for repo, _path, ref, rest in action_pins(text):
            tag = tag_of(rest)
            if not tag or not re.fullmatch(r"[0-9a-f]{40}", ref):
                continue
            key = (repo, tag)
            if key not in seen:
                try:
                    result = subprocess.run(
                        ["git", "ls-remote", "--tags", f"https://github.com/{repo}", f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}"],
                        capture_output=True,
                        text=True,
                        check=False,
                        timeout=30,
                        env=env,
                    )
                except subprocess.TimeoutExpired as error:
                    raise RuntimeError(f"git ls-remote {repo} did not answer in {error.timeout} s") from error
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
    texts = {p.name: p.read_text(encoding="utf-8") for p in sorted(list(folder.glob("*.yml")) + list(folder.glob("*.yaml")))}
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
