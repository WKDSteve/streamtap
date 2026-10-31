"""Tests of tools/check-workflows.py: the rules hold for the real workflows, and each rule fails
without what it asks for (a sabotage of the real text must be found)."""

import importlib.util
import pathlib
import unittest

HERE = pathlib.Path(__file__).resolve().parent
WORKFLOWS = HERE.parent / ".github" / "workflows"

spec = importlib.util.spec_from_file_location("check_workflows", HERE / "check-workflows.py")
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


def real():
    return {p.name: p.read_text(encoding="utf-8") for p in sorted(WORKFLOWS.glob("*.yml"))}


def sabotaged(name, old, new, count=1):
    """The real workflows with `old` replaced by `new` in one file; fails the test if `old` is not there."""
    texts = real()
    assert old in texts[name], f"{old!r} is not in {name}"
    texts[name] = texts[name].replace(old, new, count)
    return texts


class RealWorkflows(unittest.TestCase):
    def test_the_workflows_are_there(self):
        self.assertEqual({"ci.yml", "release.yml", "rolling.yml"}, set(real()))

    def test_the_rules_hold(self):
        self.assertEqual([], check.check_all(real()))


class Rules(unittest.TestCase):
    def found(self, texts, needle):
        problems = check.check_all(texts)
        self.assertTrue(any(needle in p for p in problems), f"{needle!r} not found in {problems}")

    def test_an_action_pinned_by_a_tag_is_found(self):
        self.found(sabotaged("ci.yml", "actions/checkout@d23441a48e516b6c34aea4fa41551a30e30af803", "actions/checkout@v6", 1), "not pinned by a full commit")

    def test_an_action_without_a_tag_comment_is_found(self):
        self.found(sabotaged("ci.yml", " # v6.1.0 (node24)", "", 1), "no tag in the comment")

    def test_appkit_actions_on_different_commits_are_found(self):
        other = "0123456789abcdef0123456789abcdef01234567"
        self.found(sabotaged("rolling.yml", "release-kit/prepare@518092716d00fb3fc18ce1b418032f71c3414817", f"release-kit/prepare@{other}"), "different commits")

    def test_a_job_on_a_hosted_runner_is_found(self):
        self.found(sabotaged("release.yml", "    runs-on: [self-hosted, windows]", "    runs-on: ubuntu-latest"), "not [self-hosted, windows]")

    def test_a_job_without_a_timeout_is_found(self):
        self.found(sabotaged("ci.yml", "    timeout-minutes: 10\n", "", 1), "no timeout-minutes")

    def test_a_pull_request_trigger_is_found(self):
        self.found(sabotaged("ci.yml", "  workflow_dispatch:\n", "  workflow_dispatch:\n  pull_request:\n"), "pull request")

    def test_a_checkout_that_keeps_credentials_is_found(self):
        self.found(sabotaged("ci.yml", "          persist-credentials: false\n", "", 1), "persist-credentials")

    def test_a_release_that_starts_on_a_branch_is_found(self):
        self.found(sabotaged("release.yml", 'tags: ["v*"]', 'tags: ["v*"]\n    branches: [main]'), "more than a tag")

    def test_a_release_that_starts_by_hand_is_found(self):
        self.found(sabotaged("release.yml", "on:\n  push:", "on:\n  workflow_dispatch:\n  push:"), "more than a tag")

    def test_a_release_that_publishes_is_found(self):
        self.found(sabotaged("release.yml", "          expect: 1\n", "          expect: 1\n          draft: false\n"), "draft")

    def test_a_release_step_that_makes_a_release_is_found(self):
        self.found(sabotaged("release.yml", "      - name: Empty dist\n        run: rm -rf dist", '      - name: Empty dist\n        run: gh release edit "$GITHUB_REF_NAME" --draft=false'), "NREL-07")

    def test_a_release_step_that_pushes_a_tag_is_found(self):
        self.found(sabotaged("release.yml", "run: rm -rf dist", "run: git tag v9.9.9"), "NREL-07")

    def test_a_release_without_the_main_check_is_found(self):
        self.found(sabotaged("release.yml", "merge-base --is-ancestor", "merge-base", -1), "on main")

    def test_a_release_without_the_green_ci_check_is_found(self):
        self.found(sabotaged("release.yml", "gh run list", "gh run view", -1), "green")

    def test_a_release_that_would_cut_a_published_one_again_is_found(self):
        self.found(sabotaged("release.yml", "isDraft", "name", -1), "NREL-10")

    def test_a_release_that_skips_tauri_conf_is_found(self):
        self.found(sabotaged("release.yml", "tauri.conf.json", "Cargo.toml", -1), "tauri.conf.json")

    def test_a_write_right_outside_publish_is_found(self):
        self.found(sabotaged("release.yml", "    permissions:\n      contents: read\n      actions: read", "    permissions:\n      contents: write\n      actions: read"), "only publish may write")

    def test_a_write_right_for_the_whole_release_is_found(self):
        self.found(sabotaged("release.yml", "permissions:\n  contents: read\n\nenv:", "permissions:\n  contents: write\n\nenv:"), "whole workflow")

    def test_a_rolling_build_that_starts_on_a_push_is_found(self):
        self.found(sabotaged("rolling.yml", "on:\n  workflow_run:", "on:\n  push:\n    branches: [main]\n  workflow_run:"), "more than a finished CI")

    def test_a_rolling_build_after_another_workflow_is_found(self):
        self.found(sabotaged("rolling.yml", "workflows: [ci]", "workflows: [other]"), "workflow_run")

    def test_a_rolling_build_for_any_branch_is_found(self):
        self.found(sabotaged("rolling.yml", "branches: [main]", "branches: [main, dev]"), "workflow_run")

    def test_a_rolling_build_that_does_not_wait_for_prepare_is_found(self):
        self.found(sabotaged("rolling.yml", "needs.prepare.outputs.current == 'true'", "true"), "the tip")

    def test_a_rolling_build_that_ignores_a_failed_ci_is_found(self):
        self.found(sabotaged("rolling.yml", "    if: ${{ github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' }}", "    if: ${{ true }}"), "green CI builds")

    def test_a_rolling_build_to_another_tag_is_found(self):
        self.found(sabotaged("rolling.yml", "tag: latest", "tag: nightly"), "latest")

    def test_a_rolling_build_packed_as_a_release_is_found(self):
        self.found(sabotaged("rolling.yml", " -Rolling ", " "), "rolling build")


class Pins(unittest.TestCase):
    def test_action_pins_reads_the_tag_comment(self):
        text = "      - uses: a/b/c@" + "1" * 40 + " # v1.2.3 (node24)\n"
        [(repo, path, ref, rest)] = list(check.action_pins(text))
        self.assertEqual(("a/b", "c", "1" * 40, "v1.2.3"), (repo, path, ref, check.tag_of(rest)))

    def test_a_comment_that_is_no_tag_is_none(self):
        self.assertIsNone(check.tag_of(" # the latest one"))

    def test_the_real_pins_name_their_tags(self):
        for name, text in real().items():
            for repo, _path, _ref, rest in check.action_pins(text):
                self.assertIsNotNone(check.tag_of(rest), f"{name}: {repo} has no tag comment")


if __name__ == "__main__":
    unittest.main()
