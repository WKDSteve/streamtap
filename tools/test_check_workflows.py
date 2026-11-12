"""Tests of tools/check-workflows.py: the rules hold for the real workflows, and each rule fails
without what it asks for (a sabotage of the real text must be found)."""

import importlib.util
import pathlib
import subprocess
import unittest
from unittest import mock

HERE = pathlib.Path(__file__).resolve().parent
WORKFLOWS = HERE.parent / ".github" / "workflows"

spec = importlib.util.spec_from_file_location("check_workflows", HERE / "check-workflows.py")
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)

CHECKOUT = "actions/checkout@d23441a48e516b6c34aea4fa41551a30e30af803"
KIT = "WKDSteve/appkit/release-kit"
PIN = "518092716d00fb3fc18ce1b418032f71c3414817"


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


class Parser(unittest.TestCase):
    def test_maps_lists_flow_lists_and_block_scalars(self):
        text = "a: 1  # one\nb:\n  - x\n  - y: 2\n    z: [p, 'q r']\nc: |\n  # not a comment\n  line\nd:\n- k\n"
        self.assertEqual(
            {"a": "1", "b": ["x", {"y": "2", "z": ["p", "q r"]}], "c": "  # not a comment\n  line", "d": ["k"]},
            check.parse_yaml(text),
        )

    def test_what_it_does_not_know_is_an_error_not_a_pass(self):
        for text in ("a: 1\na: 2\n", "a: {b: 1}\n", "a: &x 1\n", "a: b\n  c\n", "a: [b,\n  c]\n"):
            with self.assertRaises(check.YamlError, msg=text):
                check.parse_yaml(text)

    def test_a_file_the_parser_cannot_read_is_a_problem(self):
        texts = sabotaged("ci.yml", "permissions:\n  contents: read\n", "permissions: {contents: read}\n")
        self.assertTrue(any("cannot read it" in p for p in check.check_all(texts)))


class Rules(unittest.TestCase):
    def found(self, texts, needle):
        problems = check.check_all(texts)
        self.assertTrue(any(needle in p for p in problems), f"{needle!r} not found in {problems}")

    def clean(self, texts):
        self.assertEqual([], check.check_all(texts))

    # pins, runner, timeout, checkout
    def test_an_action_pinned_by_a_tag_is_found(self):
        self.found(sabotaged("ci.yml", CHECKOUT, "actions/checkout@v6", 1), "not pinned by a full commit")

    def test_an_action_without_a_tag_comment_is_found(self):
        self.found(sabotaged("ci.yml", " # v6.1.0 (node24)", "", 1), "no tag in the comment")

    def test_appkit_actions_on_different_commits_are_found(self):
        other = "0123456789abcdef0123456789abcdef01234567"
        self.found(sabotaged("rolling.yml", f"release-kit/prepare@{PIN}", f"release-kit/prepare@{other}"), "rolling.yml: appkit actions are pinned to different")

    def test_appkit_pinned_differently_in_two_workflows_is_found(self):
        other = "0123456789abcdef0123456789abcdef01234567"
        self.found(sabotaged("release.yml", f"release-kit/publish@{PIN}", f"release-kit/publish@{other}"), "different workflows")

    def test_a_job_on_a_hosted_runner_is_found(self):
        self.found(sabotaged("release.yml", "    runs-on: [self-hosted, windows]", "    runs-on: ubuntu-latest"), "not [self-hosted, windows]")

    def test_a_job_without_a_timeout_is_found(self):
        self.found(sabotaged("ci.yml", "    timeout-minutes: 10\n", "", 1), "no timeout-minutes")

    def test_a_checkout_that_keeps_credentials_is_found(self):
        self.found(sabotaged("ci.yml", "          persist-credentials: false\n", "", 1), "persist-credentials")

    def test_persist_credentials_in_a_comment_does_not_count(self):
        self.found(sabotaged("ci.yml", "          persist-credentials: false\n", "          # persist-credentials: false\n", 1), "persist-credentials")

    def test_persist_credentials_true_with_a_comment_does_not_count(self):
        self.found(sabotaged("ci.yml", "persist-credentials: false\n", "persist-credentials: true # was persist-credentials: false\n", 1), "persist-credentials")

    def test_a_job_with_no_jobs_is_found(self):
        self.found({"release.yml": 'name: x\non:\n  push:\n    tags: ["v*"]\n'}, "no jobs found")

    def test_another_workflow_file_is_found(self):
        texts = real()
        texts["extra.yml"] = texts["ci.yml"]
        self.found(texts, "unexpected workflow file")

    def test_a_workflow_renamed_to_yaml_is_found(self):
        texts = real()
        texts["release.yaml"] = texts.pop("release.yml")
        self.found(texts, "unexpected workflow file")

    # triggers
    def test_a_pull_request_trigger_outside_ci_is_found(self):
        self.found(sabotaged("rolling.yml", "on:\n", "on:\n  pull_request:\n"), "pull request")

    def test_a_pull_request_target_trigger_is_found(self):
        self.found(sabotaged("ci.yml", "  workflow_dispatch:\n", "  workflow_dispatch:\n  pull_request_target:\n"), "pull request")

    def test_a_ci_job_on_the_self_hosted_runner_is_found(self):
        """Literally the self-hosted runner: a pull request would run its code on the author's machine."""
        self.found(sabotaged("ci.yml", check.CI_RUNS_ON, "[self-hosted, windows]"), "keeps a pull request off")

    def test_a_ci_job_with_another_expression_is_found(self):
        self.found(sabotaged("ci.yml", check.CI_RUNS_ON, "${{ github.event.inputs.runner }}"), "keeps a pull request off")

    def test_another_ci_job_on_a_hosted_vm_for_a_push_is_found(self):
        """Only the jobs of HOSTED_CI_JOBS leave the expression, and only by their name."""
        page = "  page:\n    runs-on: "
        self.found(sabotaged("ci.yml", page + check.CI_RUNS_ON, page + "ubuntu-latest"), "keeps a pull request off")

    def test_the_hosted_job_on_another_runner_is_found(self):
        job = "  extension-sheet:\n    runs-on: "
        self.found(sabotaged("ci.yml", job + "ubuntu-latest", job + "[self-hosted, windows]"), "not ubuntu-latest")

    def test_a_secret_in_ci_is_found(self):
        self.found(sabotaged("ci.yml", "  CARGO_TERM_COLOR: always\n", "  CARGO_TERM_COLOR: always\n  X: ${{ secrets.X }}\n"), "reads a secret")

    def test_an_inline_pull_request_target_trigger_is_found(self):
        self.found(sabotaged("ci.yml", "on:\n  push:\n    branches: [main]\n  workflow_dispatch:\n  pull_request:\n    branches: [main]\n", "on: [push, pull_request_target]\n"), "pull request")

    def test_a_scalar_pull_request_target_trigger_is_found(self):
        self.found(sabotaged("ci.yml", "on:\n  push:\n    branches: [main]\n  workflow_dispatch:\n  pull_request:\n    branches: [main]\n", "on: pull_request_target\n"), "pull request")

    def test_a_comment_trigger_is_found(self):
        self.found(sabotaged("ci.yml", "  workflow_dispatch:\n", "  workflow_dispatch:\n  issue_comment:\n"), "outside event")

    def test_another_trigger_of_ci_is_found(self):
        self.found(sabotaged("ci.yml", "  workflow_dispatch:\n", "  workflow_dispatch:\n  schedule:\n"), "starts on schedule")

    def test_a_release_that_starts_on_a_branch_is_found(self):
        self.found(sabotaged("release.yml", 'tags: ["v*"]', 'tags: ["v*"]\n    branches: [main]'), "more than a tag")

    def test_a_release_that_starts_on_branches_only_is_found(self):
        self.found(sabotaged("release.yml", '    tags: ["v*"]', "    branches: [main]"), "must start on")

    def test_a_release_that_starts_on_another_tag_pattern_is_found(self):
        self.found(sabotaged("release.yml", 'tags: ["v*"]', 'tags: ["*"]'), "must start on")

    def test_a_release_that_starts_by_hand_is_found(self):
        self.found(sabotaged("release.yml", "on:\n  push:", "on:\n  workflow_dispatch:\n  push:"), "more than a tag")

    def test_a_manual_start_behind_a_comment_at_column_zero_is_found(self):
        self.found(sabotaged("release.yml", 'tags: ["v*"]', 'tags: ["v*"]\n# note\n  workflow_dispatch:'), "more than a tag")

    def test_a_comment_inside_on_is_no_false_alarm(self):
        self.clean(sabotaged("release.yml", "on:\n  push:", "on:\n  # no schedule here\n  push:"))

    # permissions
    def test_a_write_right_outside_publish_is_found(self):
        self.found(sabotaged("release.yml", "    permissions:\n      contents: read\n      actions: read", "    permissions:\n      contents: write\n      actions: read"), "only publish may write")

    def test_a_write_right_for_the_whole_release_is_found(self):
        self.found(sabotaged("release.yml", "permissions:\n  contents: read\n\nenv:", "permissions:\n  contents: write\n\nenv:"), "whole workflow")

    def test_a_release_without_top_level_permissions_is_found(self):
        self.found(sabotaged("release.yml", "permissions:\n  contents: read\n\nenv:", "env:"), "whole workflow")

    def test_write_all_is_found(self):
        self.found(sabotaged("release.yml", "permissions:\n  contents: read\n\nenv:", "permissions: write-all\n\nenv:"), "whole workflow")

    def test_a_second_top_level_right_is_found(self):
        self.found(sabotaged("release.yml", "permissions:\n  contents: read\n\nenv:", "permissions:\n  actions: read\n  contents: read\n\nenv:"), "whole workflow")

    def test_a_job_with_write_all_is_found(self):
        self.found(sabotaged("release.yml", "    permissions:\n      contents: write\n", "    permissions: write-all\n"), "list them one by one")

    def test_another_write_right_in_a_job_is_found(self):
        self.found(sabotaged("release.yml", "    timeout-minutes: 40\n", "    timeout-minutes: 40\n    permissions:\n      packages: write\n"), "no job here needs")

    def test_an_id_token_in_a_job_is_found(self):
        self.found(sabotaged("release.yml", "    timeout-minutes: 40\n", "    timeout-minutes: 40\n    permissions:\n      id-token: write\n"), "no job here needs")

    def test_a_write_right_in_the_rolling_build_is_found(self):
        self.found(sabotaged("rolling.yml", "    timeout-minutes: 40\n", "    timeout-minutes: 40\n    permissions:\n      contents:  write\n"), "only prepare and publish may write")

    def test_a_rolling_workflow_with_write_for_all_is_found(self):
        self.found(sabotaged("rolling.yml", "permissions:\n  contents: read\n\nenv:", "permissions:\n  contents: write\n\nenv:"), "whole workflow")

    def test_a_rolling_workflow_without_top_level_permissions_is_found(self):
        self.found(sabotaged("rolling.yml", "permissions:\n  contents: read\n\nenv:", "env:"), "whole workflow")

    def test_ci_with_write_is_found(self):
        self.found(sabotaged("ci.yml", "permissions:\n  contents: read\n\nconcurrency:", "permissions:\n  contents: write\n\nconcurrency:"), "whole workflow")

    def test_ci_without_permissions_is_found(self):
        self.found(sabotaged("ci.yml", "permissions:\n  contents: read\n\nconcurrency:", "concurrency:"), "whole workflow")

    def test_a_publish_job_without_write_is_found(self):
        self.found(sabotaged("release.yml", "    permissions:\n      contents: write\n", ""), "needs contents: write")

    # the release
    def test_a_release_that_publishes_is_found(self):
        self.found(sabotaged("release.yml", "          expect: 1\n", "          expect: 1\n          draft: false\n"), "draft")

    def test_a_release_with_a_quoted_draft_input_is_found(self):
        self.found(sabotaged("release.yml", "          expect: 1\n", "          expect: 1\n          draft: 'false'\n"), "draft")

    def test_a_release_step_that_makes_a_release_is_found(self):
        self.found(sabotaged("release.yml", "      - name: Empty dist\n        run: rm -rf dist", '      - name: Empty dist\n        run: gh release edit "$GITHUB_REF_NAME" --draft=false'), "NREL-07")

    def test_a_release_step_that_pushes_a_tag_is_found(self):
        self.found(sabotaged("release.yml", "run: rm -rf dist", "run: git tag v9.9.9"), "NREL-07")

    def test_other_ways_to_write_are_found(self):
        for command in (
            "curl -X PATCH https://api.github.com/repos/x/y/releases/1",
            "gh api repos/x/y/releases/1 -XPATCH",
            "gh api repos/x/y/releases/1 -f draft=false",
            "git -C . push origin HEAD",
            "git -c user.name=x tag v9",
            "Invoke-RestMethod https://api.github.com/repos/x/y/releases",
        ):
            with self.subTest(command=command):
                self.found(sabotaged("release.yml", "run: rm -rf dist", f"run: {command}"), "NREL-07")

    def test_reading_commands_are_no_false_alarm(self):
        for command in ("git tag --list", "git tag -l 'v*'", 'gh api "repos/x/y/git/ref/heads/main" --jq .object.sha'):
            with self.subTest(command=command):
                self.clean(sabotaged("release.yml", "run: rm -rf dist", f"run: {command}"))

    def test_a_release_without_the_main_check_is_found(self):
        self.found(sabotaged("release.yml", "merge-base --is-ancestor", "merge-base", -1), "on main")

    def test_a_main_check_that_is_only_echoed_is_found(self):
        self.found(sabotaged("release.yml", "if ! git merge-base --is-ancestor", "echo git merge-base --is-ancestor", -1), "on main")

    def test_a_main_check_that_is_only_a_comment_is_found(self):
        self.found(sabotaged("release.yml", 'if ! git merge-base --is-ancestor "$GITHUB_SHA" origin/main; then', "if false; then # git merge-base --is-ancestor", -1), "on main")

    def test_a_release_without_the_green_ci_check_is_found(self):
        self.found(sabotaged("release.yml", "gh run list", "gh run view", -1), "green")

    def test_a_green_check_of_another_workflow_is_found(self):
        self.found(sabotaged("release.yml", "--workflow ci.yml", "--workflow other.yml"), "green")

    def test_a_green_check_that_takes_any_status_is_found(self):
        self.found(sabotaged("release.yml", "--status success", "--status completed"), "green")

    def test_a_release_that_would_cut_a_published_one_again_is_found(self):
        self.found(sabotaged("release.yml", "isDraft", "name", -1), "NREL-10")

    def test_a_release_that_takes_every_gh_error_for_no_release_is_found(self):
        self.found(sabotaged("release.yml", "release not found", "anything", -1), "release not found")

    def test_a_release_that_skips_tauri_conf_is_found(self):
        self.found(sabotaged("release.yml", "tauri.conf.json", "Cargo.toml", -1), "tauri.conf.json")

    def test_a_release_that_skips_the_sink_and_host_versions_is_found(self):
        self.found(sabotaged("release.yml", "for member in streamtap-sink streamtap-host", "for member in streamtap-sink", -1), "sink and the host")

    def test_a_build_that_does_not_wait_for_the_check_is_found(self):
        self.found(sabotaged("release.yml", "  build:\n    needs: check\n", "  build:\n"), "build must need check")

    def test_a_publish_that_does_not_wait_for_the_check_is_found(self):
        self.found(sabotaged("release.yml", "needs: [check, build]", "needs: [build]"), "publish must need check")

    def test_always_in_a_release_job_is_found(self):
        self.found(sabotaged("release.yml", "    needs: [check, build]\n", "    needs: [check, build]\n    if: always()\n"), "always")

    def test_continue_on_error_is_found(self):
        self.found(sabotaged("release.yml", "      - name: The tag, the version and the CI of this commit agree\n", "      - name: The tag, the version and the CI of this commit agree\n        continue-on-error: true\n"), "continue-on-error")

    def test_a_release_that_publishes_another_tag_is_found(self):
        self.found(sabotaged("release.yml", "tag: ${{ github.ref_name }}", "tag: latest"), "tag of this run")

    # rolling
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

    def test_a_publish_that_does_not_wait_for_the_build_is_found(self):
        self.found(sabotaged("rolling.yml", "needs: [prepare, build]", "needs: [prepare]"), "publish must need")

    def test_a_publish_without_the_commit_is_found(self):
        self.found(sabotaged("rolling.yml", "          sha: ${{ needs.prepare.outputs.sha }}\n          files: dist", "          files: dist"), "name the commit")

    def test_a_checkout_of_another_ref_is_found(self):
        self.found(sabotaged("rolling.yml", "ref: ${{ needs.prepare.outputs.sha }}", "ref: ${{ github.event.workflow_run.head_branch }}"), "checkout must take")

    def test_a_prepare_for_another_commit_is_found(self):
        self.found(sabotaged("rolling.yml", "sha: ${{ github.event.workflow_run.head_sha }}", "sha: ${{ github.sha }}"), "head_sha")

    def test_a_workflow_level_concurrency_is_found(self):
        self.found(sabotaged("rolling.yml", "permissions:\n  contents: read\n\nenv:", "concurrency:\n  group: rolling\n  cancel-in-progress: true\n\npermissions:\n  contents: read\n\nenv:"), "workflow-level concurrency")

    def test_a_build_that_is_not_cancelled_by_a_newer_one_is_found(self):
        self.found(sabotaged("rolling.yml", "      group: rolling-build-${{ needs.prepare.outputs.current }}\n      cancel-in-progress: true", "      group: rolling-build-${{ needs.prepare.outputs.current }}\n      cancel-in-progress: false"), "cancelled by a newer build")

    def test_a_publish_that_can_be_cancelled_is_found(self):
        self.found(sabotaged("rolling.yml", "      group: rolling-publish\n      cancel-in-progress: false", "      group: rolling-publish\n      cancel-in-progress: true"), "never be cancelled")

    def test_a_publish_without_the_tip_check_is_found(self):
        self.found(sabotaged("rolling.yml", "git/ref/heads/main", "git/ref/heads/other"), "still the commit")

    def test_a_publish_step_that_ignores_the_tip_check_is_found(self):
        self.found(sabotaged("rolling.yml", "      - name: Empty dist\n        if: ${{ steps.tip.outputs.current == 'true' }}\n", "      - name: Empty dist\n"), "only if it is still the tip")

    def test_a_rolling_step_that_makes_a_release_is_found(self):
        self.found(sabotaged("rolling.yml", "run: rm -rf dist", "run: gh release create x"), "only the kit's actions do")

    def test_a_rolling_build_that_publishes_a_draft_is_found(self):
        self.found(sabotaged("rolling.yml", "run: rm -rf dist", "run: x --draft=false"), "publishes a draft")

    def test_a_rolling_draft_input_is_found(self):
        self.found(sabotaged("rolling.yml", "          expect: 1\n", "          expect: 1\n          draft: false\n"), "publishes a draft")

    def test_a_workflow_without_a_trigger_is_found(self):
        self.found(sabotaged("ci.yml", "on:\n  push:\n    branches: [main]\n  workflow_dispatch:\n  pull_request:\n    branches: [main]\n", ""), "no trigger")

    def test_a_publish_job_with_read_only_is_found(self):
        self.found(sabotaged("release.yml", "    permissions:\n      contents: write\n", "    permissions:\n      contents: read\n"), "needs contents: write")

    def test_a_release_without_the_check_job_is_found(self):
        self.found(sabotaged("release.yml", "  check:\n", "  checkx:\n"), "no job check")

    def test_a_release_without_the_kit_publish_is_found(self):
        self.found(sabotaged("release.yml", f"release-kit/publish@{PIN}", f"release-kit/other@{PIN}"), "the draft is made by the kit")

    def test_a_release_that_expects_two_files_is_found(self):
        self.found(sabotaged("release.yml", "expect: 1", "expect: 2"), "exactly one file")

    def test_a_rolling_build_that_does_not_need_prepare_is_found(self):
        self.found(sabotaged("rolling.yml", "  build:\n    needs: prepare\n", "  build:\n"), "build must need prepare")

    def test_a_rolling_build_without_the_kit_prepare_is_found(self):
        self.found(sabotaged("rolling.yml", f"release-kit/prepare@{PIN}", f"release-kit/other@{PIN}"), "(prepare)")

    def test_a_rolling_build_without_the_kit_publish_is_found(self):
        self.found(sabotaged("rolling.yml", f"release-kit/publish@{PIN}", f"release-kit/other@{PIN}"), "the kit's publish action")


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


class Online(unittest.TestCase):
    TEXTS = {"ci.yml": f"      - uses: actions/checkout@{'a' * 40} # v6.1.0\n"}

    def answer(self, stdout="", code=0, stderr=""):
        return mock.Mock(return_value=subprocess.CompletedProcess([], code, stdout, stderr))

    def test_a_tag_that_still_names_the_pin_is_fine(self):
        run = self.answer(f"{'a' * 40}\trefs/tags/v6.1.0^{{}}\n")
        with mock.patch.object(check.subprocess, "run", run):
            self.assertEqual([], check.online(self.TEXTS))
        self.assertEqual(30, run.call_args.kwargs["timeout"])
        self.assertEqual("0", run.call_args.kwargs["env"]["GIT_TERMINAL_PROMPT"])

    def test_a_moved_tag_is_found(self):
        run = self.answer(f"{'b' * 40}\trefs/tags/v6.1.0\n")
        with mock.patch.object(check.subprocess, "run", run):
            [problem] = check.online(self.TEXTS)
        self.assertIn("names " + "b" * 40, problem)

    def test_a_missing_tag_is_found(self):
        with mock.patch.object(check.subprocess, "run", self.answer("")):
            [problem] = check.online(self.TEXTS)
        self.assertIn("names None", problem)

    def test_a_failing_git_is_not_a_pass(self):
        with mock.patch.object(check.subprocess, "run", self.answer("", 128, "no route")):
            with self.assertRaisesRegex(RuntimeError, "no route"):
                check.online(self.TEXTS)

    def test_a_stalled_git_is_not_a_pass(self):
        with mock.patch.object(check.subprocess, "run", mock.Mock(side_effect=subprocess.TimeoutExpired("git", 30))):
            with self.assertRaisesRegex(RuntimeError, "did not answer"):
                check.online(self.TEXTS)


if __name__ == "__main__":
    unittest.main()
