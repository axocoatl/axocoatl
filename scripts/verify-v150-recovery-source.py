#!/usr/bin/env python3
"""Read-only, incident-specific validation of the frozen v1.5.0 release run.

Inputs must be fresh, complete GitHub REST run, attempt-1 jobs and artifact
responses. This validates provenance metadata; archive checksums, attestations,
remote refs and signed release evidence are checked separately by the workflow.
"""

import argparse
import json
from pathlib import Path
import sys

REPOSITORY = "axocoatl/axocoatl"
REPOSITORY_ID = 1258994885
SOURCE_SHA = "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
SOURCE_RUN = 40439869836
SOURCE_ATTEMPT = 1
SOURCE_TAG = "v1.5.0"
ARTIFACT_ID = 12711066172
ARTIFACT_NAME = "verified-release-40439869836-v1.5.0"
ARTIFACT_DIGEST = "sha256:2ca1f79d29d21425a395c60212ba8febdd90260f56666dfbce527e32f1444dfc"
DRAFT_JOB = 122020508184
EXPECTED_JOBS = {
    121977464697: ("Release gate — docs site", "success"),
    121977464942: ("Release gate — new-user evidence", "success"),
    121977464965: ("Release gate — embedded product browser (x86_64, WebKit)", "success"),
    121977464971: ("Release gate — embedded product browser (aarch64, Chromium)", "success"),
    121977465055: ("Product test gate", "success"),
    121977465067: ("Release gate — embedded product browser (x86_64, Chromium)", "success"),
    121977465072: ("Release gate — marketing site", "success"),
    121977465121: ("Release gate — embedded product browser (macOS arm64, WebKit)", "success"),
    122005633819: ("Build and prove aarch64-unknown-linux-gnu", "success"),
    122005633827: ("Build and prove x86_64-unknown-linux-gnu", "success"),
    122005633834: ("Build and prove aarch64-apple-darwin", "success"),
    122005633857: ("Build and prove x86_64-apple-darwin", "success"),
    122018517730: ("Prove complete uploaded artifact set", "success"),
    122018517735: ("Release gate — new user (Linux aarch64-unknown-linux-gnu)", "success"),
    122018517738: ("Release gate — new user (Linux x86_64-unknown-linux-gnu)", "success"),
    DRAFT_JOB: ("Create and verify GitHub draft", "failure"),
    122020640085: ("Deploy marketing after the product release is public", "skipped"),
    122020640114: ("Publish and prove crates.io packages", "skipped"),
    122020640368: ("Reverify and publish GitHub Release", "skipped"),
    122020641328: ("Deploy docs after the product release is public", "skipped"),
}
EXPECTED_DRAFT_STEPS = {
    1: ("Set up job", "success"),
    2: ("Run actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09", "success"),
    3: ("Run actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333", "success"),
    4: ("Reverify downloaded release set", "success"),
    5: ("Extract matching changelog section", "success"),
    6: ("Require the remote tag to still identify this release commit", "success"),
    7: ("Reprove release order before draft mutation", "success"),
    8: ("Create or reconcile the exact release without overwriting", "failure"),
    16: ("Post Run actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09", "success"),
    17: ("Complete job", "success"),
}


class InvalidSource(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise InvalidSource(message)


def fields(value, expected, context):
    require(isinstance(value, dict), f"{context}: expected an object")
    for key, wanted in expected.items():
        actual = value.get(key)
        require(type(actual) is type(wanted) and actual == wanted,
                f"{context}: {key} does not match the frozen incident")


def complete_collection(value, key):
    require(isinstance(value, dict), f"{key}: expected a collection object")
    items = value.get(key)
    require(isinstance(items, list), f"{key}: expected an array")
    require(type(value.get("total_count")) is int and value["total_count"] == len(items),
            f"{key}: incomplete or inconsistent collection")
    require(all(isinstance(item, dict) for item in items), f"{key}: invalid member")
    return items


def validate(run, jobs_response, artifacts_response):
    fields(run, {
        "id": SOURCE_RUN, "run_attempt": SOURCE_ATTEMPT,
        "head_branch": SOURCE_TAG, "head_sha": SOURCE_SHA,
        "path": ".github/workflows/release.yml", "event": "push",
        "status": "completed", "conclusion": "failure",
    }, "run")
    for key in ("repository", "head_repository"):
        fields(run.get(key), {"id": REPOSITORY_ID, "full_name": REPOSITORY}, key)
    fields(run.get("head_commit"), {"id": SOURCE_SHA}, "head_commit")

    jobs = complete_collection(jobs_response, "jobs")
    require(len(jobs) == len(EXPECTED_JOBS), "jobs: unexpected job count")
    seen = set()
    for job in jobs:
        job_id = job.get("id")
        require(type(job_id) is int and job_id in EXPECTED_JOBS and job_id not in seen,
                "jobs: unknown or duplicate job ID")
        seen.add(job_id)
        name, conclusion = EXPECTED_JOBS[job_id]
        fields(job, {
            "name": name, "status": "completed", "conclusion": conclusion,
            "run_id": SOURCE_RUN, "run_attempt": SOURCE_ATTEMPT, "head_sha": SOURCE_SHA,
        }, f"job {job_id}")
        if job_id == DRAFT_JOB:
            steps = job.get("steps")
            require(isinstance(steps, list) and len(steps) == len(EXPECTED_DRAFT_STEPS),
                    "draft: unexpected step count")
            step_ids = set()
            for step in steps:
                require(isinstance(step, dict), "draft: invalid step")
                number = step.get("number")
                require(type(number) is int and number in EXPECTED_DRAFT_STEPS and number not in step_ids,
                        "draft: unknown or duplicate step")
                step_ids.add(number)
                name, conclusion = EXPECTED_DRAFT_STEPS[number]
                fields(step, {"name": name, "status": "completed", "conclusion": conclusion},
                       f"draft step {number}")

    artifacts = complete_collection(artifacts_response, "artifacts")
    artifact_ids = [artifact.get("id") for artifact in artifacts]
    require(all(type(artifact_id) is int for artifact_id in artifact_ids), "artifacts: invalid ID")
    require(len(set(artifact_ids)) == len(artifact_ids), "artifacts: duplicate ID")
    matches = [artifact for artifact in artifacts
               if artifact.get("id") == ARTIFACT_ID or artifact.get("name") == ARTIFACT_NAME]
    require(len(matches) == 1, "artifacts: missing or ambiguous verified release artifact")
    artifact = matches[0]
    fields(artifact, {
        "id": ARTIFACT_ID, "name": ARTIFACT_NAME, "digest": ARTIFACT_DIGEST,
        "expired": False, "size_in_bytes": 134430046,
        "archive_download_url": f"https://api.github.com/repos/{REPOSITORY}/actions/artifacts/{ARTIFACT_ID}/zip",
    }, "verified artifact")
    fields(artifact.get("workflow_run"), {
        "id": SOURCE_RUN, "repository_id": REPOSITORY_ID, "head_repository_id": REPOSITORY_ID,
        "head_branch": SOURCE_TAG, "head_sha": SOURCE_SHA,
    }, "artifact workflow_run")
    return {
        "verified": True, "repository": REPOSITORY, "source_commit": SOURCE_SHA,
        "source_run": SOURCE_RUN, "source_attempt": SOURCE_ATTEMPT,
        "successful_jobs": 15, "failed_draft_job": DRAFT_JOB, "skipped_jobs": 4,
        "artifact_id": ARTIFACT_ID, "artifact_name": ARTIFACT_NAME,
        "artifact_digest": ARTIFACT_DIGEST,
    }


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"JSON: duplicate object key {key}")
        result[key] = value
    return result


def read_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8"), object_pairs_hook=unique_object)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run_json")
    parser.add_argument("jobs_json")
    parser.add_argument("artifacts_json")
    args = parser.parse_args()
    try:
        result = validate(read_json(args.run_json), read_json(args.jobs_json), read_json(args.artifacts_json))
    except (OSError, ValueError, TypeError) as error:
        print(f"Recovery source refused: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
