#!/usr/bin/env python3
"""Offline contracts for the incident-bound publication continuation.

Installed in scripts/, paths resolve from this file, not the shell's cwd.
For a staged draft: --repo /absolute/repo --workflow /private/tmp/recover-v1.5.0.yml
Ruby/Psych supplies YAML parsing; no PyYAML, network, publishing, or Cargo required.
"""
import argparse
import copy
import json
from pathlib import Path
import re
import subprocess
import unittest

COMMIT = "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
TAG = "v1.5.0"
RUN = "40439869836"
SOURCE_ARTIFACT = "12711066172"
CONTROL = "${{ github.sha }}"
CURRENT_RUN = "${{ github.run_id }}"
HELPER = 'python3 ../recovery-control/scripts/release-notes.py CHANGELOG.md "$RELEASE_VERSION" "$RELEASE_COMMIT" release-notes.md'
PUBLISHERS = ("prepare-release", "publish-crates", "release")
NOTES_PATTERN = re.compile(
    r'''(?m)^([ \t]*)awk -v heading="## \[\$version\]" '\n.*?^\1' CHANGELOG\.md > release-notes\.md$''',
    re.S,
)


def yaml_read(path):
    data = json.loads(subprocess.check_output([
        "ruby", "-rjson", "-ryaml", "-e",
        "puts JSON.generate(YAML.load_file(ARGV.fetch(0)))", str(path),
    ], text=True))
    # YAML 1.1 represents an unquoted `on` key as true. GitHub uses YAML 1.2.
    if "on" not in data and "true" in data:
        data["on"] = data.pop("true")
    return data


def mapped_body(body):
    body = body.replace("GITHUB_REF_NAME", "RELEASE_TAG").replace("GITHUB_SHA", "RELEASE_COMMIT")
    body = NOTES_PATTERN.sub(lambda m: m[1] + HELPER, body)
    # The helper takes RELEASE_VERSION directly; remove only a now-unused
    # local assignment. Other bodies still need $version for registry proof.
    if HELPER in body and not re.search(r"\$version\b|\$\{version\}", body):
        body = re.sub(r'(?m)^version="\$\{RELEASE_TAG#v\}"\n', "", body)
    return body.strip()


def steps(workflow, job):
    return workflow["jobs"][job].get("steps", [])


def named(workflow, job, name):
    return next(s for s in steps(workflow, job) if s.get("name") == name)


def needs(job):
    value = job.get("needs", [])
    return value if isinstance(value, list) else [value]


def problems(workflow, original):
    errors = []
    def check(condition, message):
        if not condition:
            errors.append(message)

    trigger = workflow.get("on", {})
    check(set(trigger) == {"workflow_dispatch"}, "dispatch-only trigger")
    inputs = trigger.get("workflow_dispatch", {}).get("inputs", {})
    check(set(inputs) == {"confirmation"} and inputs["confirmation"].get("required") is True,
          "required confirmation only; no override input")
    check(workflow.get("concurrency") == original.get("concurrency"), "shared production release lock")
    check(workflow.get("permissions") == {"contents": "read"}, "default read-only permissions")
    env = workflow.get("env", {})
    check(all(env.get(k) == v for k, v in {
        "RELEASE_TAG": TAG, "RELEASE_VERSION": "1.5.0", "RELEASE_COMMIT": COMMIT, "SOURCE_RUN_ID": RUN,
    }.items()), "fixed original source/run/tag")
    check(not any(k.startswith("GITHUB_") for k in env), "do not redefine GitHub execution identity")
    check(workflow.get("defaults", {}).get("run") == {"shell": "bash", "working-directory": "release-source"},
          "frozen shell working directory")
    jobs = workflow.get("jobs", {})
    graph = {
        "preflight": [], "prepare-release": ["preflight"], "publish-crates": ["prepare-release"],
        "release": ["prepare-release", "publish-crates"], "deploy-marketing": ["release"], "deploy-docs": ["release"],
    }
    check(set(jobs) == set(graph), "only remaining publication jobs; no rebuild or parallel bypass")
    for job, dependencies in graph.items():
        if job not in jobs:
            continue
        check(needs(jobs[job]) == dependencies, "required dependency chain: " + job)
        check("if" not in jobs[job] and "continue-on-error" not in jobs[job], "no job success bypass: " + job)
        if "steps" in jobs[job]:
            check("defaults" not in jobs[job], "no job cwd override: " + job)
            check(all("working-directory" not in s and "continue-on-error" not in s for s in steps(workflow, job)),
                  "no step cwd/error bypass: " + job)
            checkout = [s for s in steps(workflow, job) if s.get("uses", "").startswith("actions/checkout@")]
            source = [s for s in checkout if s.get("with", {}).get("path") == "release-source"]
            check(len(source) == 1 and source[0]["with"].get("ref") == COMMIT
                  and source[0]["with"].get("fetch-depth") == 0, "exact source checkout: " + job)
            control = [s for s in checkout if s.get("with", {}).get("path") == "recovery-control"]
            wanted_control = job in ("preflight", "prepare-release", "release")
            check(len(control) == int(wanted_control)
                  and all(s["with"].get("ref") == CONTROL for s in control)
                  and len(checkout) == 1 + int(wanted_control), "isolated control checkout: " + job)

    # Preserve the original publisher command bodies, order, conditions and credentials.
    # Only source-context replacement and the three notes extractions may change.
    note_count = 0
    for job in PUBLISHERS:
        if job not in jobs:
            continue
        expected_permissions = copy.deepcopy(original["jobs"][job].get("permissions"))
        if job == "prepare-release":
            expected_permissions["attestations"] = "read"
        check(jobs[job].get("permissions") == expected_permissions,
              "original publisher permissions: " + job)
        old_runs = [s for s in steps(original, job) if "run" in s]
        old_names = [s["name"] for s in old_runs]
        current_runs = [s for s in steps(workflow, job) if "run" in s]
        check([s.get("name") for s in current_runs if s.get("name") in old_names] == old_names,
              "original publisher operation order: " + job)
        allowed_extra = {"Prove every archive's attestation verifies"} if job == "prepare-release" else set()
        check(all(s.get("name") in set(old_names) | allowed_extra for s in current_runs),
              "no extra publisher commands: " + job)
        for old in old_runs:
            matches = [s for s in current_runs if s.get("name") == old["name"]]
            label = job + ": " + old["name"]
            check(len(matches) == 1, "unique original publisher step: " + label)
            if len(matches) != 1:
                continue
            new = matches[0]
            check(new["run"].strip() == mapped_body(old["run"]), "original publisher body changed: " + label)
            check(new.get("if") == old.get("if") and new.get("id") == old.get("id"),
                  "original publisher condition/id: " + label)
            expected_env = copy.deepcopy(old.get("env", {}))
            if "ARTIFACT_NAME" in expected_env:
                expected_env["ARTIFACT_NAME"] = "published-crates-" + CURRENT_RUN + "-" + TAG
            if "NEW_USER_GATE_OVERRIDE" in expected_env:
                expected_env["NEW_USER_GATE_OVERRIDE"] = ""
            check(new.get("env", {}) == expected_env, "original publisher environment: " + label)
            note_count += new["run"].count(HELPER)
    check(note_count == 3, "all three notes generation sites use one deterministic helper")

    # Actions resolve paths from workspace root, unlike shell steps in release-source.
    expected_downloads = {
        "prepare-release": [(None, SOURCE_ARTIFACT, "release-source/dist", RUN)],
        "release": [(None, SOURCE_ARTIFACT, "release-source/reviewed", RUN),
                    ("published-crates-" + CURRENT_RUN + "-" + TAG, None, "release-source/crate-state", CURRENT_RUN)],
    }
    for job, expected in expected_downloads.items():
        actual = [s for s in steps(workflow, job) if s.get("uses", "").startswith("actions/download-artifact@")]
        observed = [(s.get("with", {}).get("name"), s.get("with", {}).get("artifact-ids"),
                     s.get("with", {}).get("path"), str(s.get("with", {}).get("run-id"))) for s in actual]
        check(observed == expected, "artifact run and workspace paths: " + job)
        check(all(s["with"].get("github-token") == "${{ github.token }}" for s in actual), "explicit cross-run artifact token: " + job)
    uploads = [s for s in steps(workflow, "publish-crates") if s.get("uses", "").startswith("actions/upload-artifact@")]
    check(len(uploads) == 1 and uploads[0].get("with", {}).get("name") == "published-crates-" + CURRENT_RUN + "-" + TAG
          and uploads[0]["with"].get("path", "").split() == ["release-source/release-crates.txt", "release-source/crate-checksums.sha256"]
          and uploads[0]["with"].get("if-no-files-found") == "error", "crate proof upload from frozen working directory")

    attest = [s for s in steps(workflow, "prepare-release") if s.get("name") == "Prove every archive's attestation verifies"]
    expected_attest = mapped_body(named(original, "prove-artifacts", "Prove every archive's attestation verifies")["run"])
    expected_attest = expected_attest.replace('"$GITHUB_REF"', '"refs/tags/$RELEASE_TAG"')
    check(len(attest) == 1 and attest[0].get("run", "").strip() == expected_attest,
          "original release-workflow attestation identity and four archives")
    for job, called in (("deploy-marketing", "marketing-deploy.yml"), ("deploy-docs", "docs-deploy.yml")):
        j = jobs.get(job, {})
        check(j.get("uses") == "./.github/workflows/" + called
              and j.get("with") == {"release_tag": TAG, "release_sha": COMMIT}
              and j.get("permissions") == original["jobs"][job].get("permissions")
              and j.get("secrets") == original["jobs"][job].get("secrets"), "frozen deployment inputs: " + job)

    preflight = "\n".join(s.get("run", "") for s in steps(workflow, "preflight"))
    for guard in (
        'test "$GITHUB_REPOSITORY" = axocoatl/axocoatl', 'test "$GITHUB_EVENT_NAME" = workflow_dispatch',
        'test "$GITHUB_REF" = refs/heads/main', 'test "$DEFAULT_BRANCH" = main',
        'CONTROL $GITHUB_SHA"', 'test "$(git rev-parse \'HEAD^{commit}\')" = "$RELEASE_COMMIT"',
        'test -z "$(git status --porcelain)"', 'git merge-base --is-ancestor "$RELEASE_COMMIT" "$GITHUB_SHA"',
        'test "$(git rev-parse refs/remotes/origin/main)" = "$GITHUB_SHA"',
        'changed and changed<=allowed', '/attempts/1/jobs?per_page=100', '/artifacts?per_page=100',
        'python3 ../recovery-control/scripts/verify-v150-recovery-source.py',
        'python3 ../recovery-control/scripts/test-release-notes.py',
        'python3 ../recovery-control/scripts/test-v150-recovery-source.py',
        'python3 ../recovery-control/scripts/test-recover-v1.5.0-workflow.py',
        'node scripts/release-gate/check-evidence.mjs --version "$RELEASE_VERSION" --tag-commit "$RELEASE_COMMIT"',
        './scripts/verify-release-order.sh "$RELEASE_TAG"',
    ):
        check(guard in preflight, "preflight guard: " + guard)
    original_pins = {s["uses"] for j in original["jobs"].values() for s in j.get("steps", []) if "uses" in s}
    for job in jobs:
        for step in steps(workflow, job):
            if "uses" in step:
                check(step["uses"] in original_pins, "existing pinned action: " + step["uses"])
    return errors


class WorkflowContracts(unittest.TestCase):
    def test_actual_draft_preserves_publication_contract(self):
        self.assertEqual(problems(RECOVERY, ORIGINAL), [])

    def rejected(self, change, diagnostic):
        altered = copy.deepcopy(RECOVERY)
        change(altered)
        self.assertTrue(any(diagnostic in e for e in problems(altered, ORIGINAL)), diagnostic)

    def test_main_checkout_cannot_replace_frozen_source(self):
        self.rejected(lambda w: w["jobs"]["publish-crates"]["steps"][0]["with"].update(ref=CONTROL), "exact source checkout")

    def test_removed_crate_prerequisite_blocks_publication(self):
        self.rejected(lambda w: w["jobs"]["release"].update(needs=["prepare-release"]), "dependency chain")

    def test_success_bypass_is_rejected(self):
        self.rejected(lambda w: w["jobs"]["release"].update({"if": "always()"}), "success bypass")

    def test_native_artifacts_cannot_come_from_recovery_run(self):
        def change(w):
            s = next(s for s in steps(w, "prepare-release") if s.get("uses", "").startswith("actions/download-artifact@"))
            s["with"]["run-id"] = CURRENT_RUN
        self.rejected(change, "artifact run")

    def test_original_artifact_identity_cannot_revert_to_mutable_name(self):
        def change(w):
            s = next(s for s in steps(w, "prepare-release") if s.get("uses", "").startswith("actions/download-artifact@"))
            s["with"].pop("artifact-ids")
            s["with"]["name"] = "verified-release-" + RUN + "-" + TAG
        self.rejected(change, "artifact run")

    def test_original_artifact_id_cannot_change_in_final_publication(self):
        def change(w):
            s = next(s for s in steps(w, "release") if s.get("uses", "").startswith("actions/download-artifact@"))
            s["with"]["artifact-ids"] = "12711066173"
        self.rejected(change, "artifact run")

    def test_attestation_read_permission_is_required(self):
        self.rejected(lambda w: w["jobs"]["prepare-release"]["permissions"].pop("attestations"), "publisher permissions")

    def test_upload_cannot_silently_miss_source_directory(self):
        def change(w):
            s = next(s for s in steps(w, "publish-crates") if s.get("uses", "").startswith("actions/upload-artifact@"))
            s["with"]["path"] = "release-crates.txt\ncrate-checksums.sha256\n"
        self.rejected(change, "crate proof upload")

    def test_overwrite_existing_asset_cannot_replace_byte_proof(self):
        def change(w):
            s = named(w, "prepare-release", "Create or reconcile the exact release without overwriting")
            s["run"] = s["run"].replace('cmp -s "dist/$name" "existing-assets/$name"', "true")
        self.rejected(change, "original publisher body changed")

    def test_sparse_index_proof_cannot_be_removed(self):
        def change(w):
            s = named(w, "publish-crates", "Publish in dependency order")
            s["run"] = s["run"].replace("./scripts/prove-release-crates.sh", "true")
        self.rejected(change, "original publisher body changed")

    def test_final_notes_cannot_revert_to_oversized_original(self):
        def change(w):
            s = named(w, "release", "Prove the release is public and not a prerelease")
            s["run"] = s["run"].replace(HELPER, "cp CHANGELOG.md release-notes.md")
        self.rejected(change, "notes generation")

    def test_new_user_evidence_override_cannot_be_enabled(self):
        self.rejected(lambda w: named(w, "release", "Require signed new-user evidence again before publishing")["env"].update(NEW_USER_GATE_OVERRIDE="publish v1.5.0 without the new-user gate"), "publisher environment")

    def test_attestation_cannot_identify_control_source(self):
        self.rejected(lambda w: named(w, "prepare-release", "Prove every archive's attestation verifies").update(run="true"), "attestation identity")

    def test_deployment_cannot_use_control_sha(self):
        self.rejected(lambda w: w["jobs"]["deploy-docs"]["with"].update(release_sha=CONTROL), "frozen deployment")

    def test_source_validator_cannot_be_skipped(self):
        def change(w):
            s = named(w, "preflight", "Verify frozen source, original successful gates, and publishing repair")
            s["run"] = s["run"].replace("python3 ../recovery-control/scripts/verify-v150-recovery-source.py", "echo skipped")
        self.rejected(change, "preflight guard")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--workflow", type=Path)
    args, rest = parser.parse_known_args()
    ORIGINAL = yaml_read(args.repo / ".github/workflows/release.yml")
    RECOVERY = yaml_read(args.workflow or args.repo / ".github/workflows/recover-v1.5.0.yml")
    unittest.main(argv=[str(Path(__file__)), *rest])
