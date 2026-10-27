#!/usr/bin/env python3
"""Mutation regressions using a reduced, actual attempt-1 REST snapshot."""
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("verify-v150-recovery-source.py")
spec = importlib.util.spec_from_file_location("recovery_source", SCRIPT)
validator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validator)
# Captured from GitHub REST run 40439869836, attempt 1 on 2026-10-27.
FIXTURE = json.loads(r'''{
  "run": {
    "id": 40439869836,
    "run_attempt": 1,
    "head_branch": "v1.5.0",
    "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7",
    "path": ".github/workflows/release.yml",
    "event": "push",
    "status": "completed",
    "conclusion": "failure",
    "head_commit": {
      "id": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7",
      "tree_id": "6cfea6ce18eaaa4c577efc75b6cf516e0baaa0d5",
      "message": "Include retained screenshots referenced by signed release evidence",
      "timestamp": "2026-10-27T02:40:46Z",
      "author": {
        "name": "Erick Echeverria",
        "email": "erick5eche@gmail.com"
      },
      "committer": {
        "name": "Erick Echeverria",
        "email": "erick5eche@gmail.com"
      }
    },
    "repository": {
      "id": 1258994885,
      "full_name": "axocoatl/axocoatl"
    },
    "head_repository": {
      "id": 1258994885,
      "full_name": "axocoatl/axocoatl"
    }
  },
  "jobs": {
    "total_count": 20,
    "jobs": [
      {
        "id": 121977464697,
        "name": "Release gate — docs site",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 121977464942,
        "name": "Release gate — new-user evidence",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 121977464965,
        "name": "Release gate — embedded product browser (x86_64, WebKit)",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 121977464971,
        "name": "Release gate — embedded product browser (aarch64, Chromium)",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 121977465055,
        "name": "Product test gate",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 121977465067,
        "name": "Release gate — embedded product browser (x86_64, Chromium)",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 121977465072,
        "name": "Release gate — marketing site",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 121977465121,
        "name": "Release gate — embedded product browser (macOS arm64, WebKit)",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 122005633819,
        "name": "Build and prove aarch64-unknown-linux-gnu",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 122005633827,
        "name": "Build and prove x86_64-unknown-linux-gnu",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 122005633834,
        "name": "Build and prove aarch64-apple-darwin",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 122005633857,
        "name": "Build and prove x86_64-apple-darwin",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 122018517730,
        "name": "Prove complete uploaded artifact set",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 122018517735,
        "name": "Release gate — new user (Linux aarch64-unknown-linux-gnu)",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 122018517738,
        "name": "Release gate — new user (Linux x86_64-unknown-linux-gnu)",
        "status": "completed",
        "conclusion": "success",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 122020508184,
        "name": "Create and verify GitHub draft",
        "status": "completed",
        "conclusion": "failure",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7",
        "steps": [
          {
            "name": "Set up job",
            "number": 1,
            "status": "completed",
            "conclusion": "success"
          },
          {
            "name": "Run actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09",
            "number": 2,
            "status": "completed",
            "conclusion": "success"
          },
          {
            "name": "Run actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333",
            "number": 3,
            "status": "completed",
            "conclusion": "success"
          },
          {
            "name": "Reverify downloaded release set",
            "number": 4,
            "status": "completed",
            "conclusion": "success"
          },
          {
            "name": "Extract matching changelog section",
            "number": 5,
            "status": "completed",
            "conclusion": "success"
          },
          {
            "name": "Require the remote tag to still identify this release commit",
            "number": 6,
            "status": "completed",
            "conclusion": "success"
          },
          {
            "name": "Reprove release order before draft mutation",
            "number": 7,
            "status": "completed",
            "conclusion": "success"
          },
          {
            "name": "Create or reconcile the exact release without overwriting",
            "number": 8,
            "status": "completed",
            "conclusion": "failure"
          },
          {
            "name": "Post Run actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09",
            "number": 16,
            "status": "completed",
            "conclusion": "success"
          },
          {
            "name": "Complete job",
            "number": 17,
            "status": "completed",
            "conclusion": "success"
          }
        ]
      },
      {
        "id": 122020640085,
        "name": "Deploy marketing after the product release is public",
        "status": "completed",
        "conclusion": "skipped",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 122020640114,
        "name": "Publish and prove crates.io packages",
        "status": "completed",
        "conclusion": "skipped",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 122020640368,
        "name": "Reverify and publish GitHub Release",
        "status": "completed",
        "conclusion": "skipped",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      },
      {
        "id": 122020641328,
        "name": "Deploy docs after the product release is public",
        "status": "completed",
        "conclusion": "skipped",
        "run_id": 40439869836,
        "run_attempt": 1,
        "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
      }
    ]
  },
  "artifacts": {
    "total_count": 7,
    "artifacts": [
      {
        "id": 12711231805,
        "name": "newuser-release-40439869836-A-x86",
        "digest": "sha256:671445236ba4690b6e4784baa6a4242a5d6215950ad958a82879706925bebeaa",
        "expired": false,
        "size_in_bytes": 22051422,
        "archive_download_url": "https://api.github.com/repos/axocoatl/axocoatl/actions/artifacts/12711231805/zip",
        "workflow_run": {
          "id": 40439869836,
          "repository_id": 1258994885,
          "head_repository_id": 1258994885,
          "head_branch": "v1.5.0",
          "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
        }
      },
      {
        "id": 12711171982,
        "name": "newuser-release-40439869836-A-arm",
        "digest": "sha256:6e1c46a8aabc91418b2d408e9c873b4af13890ab4a94b32d110f1c9e98196572",
        "expired": false,
        "size_in_bytes": 22051382,
        "archive_download_url": "https://api.github.com/repos/axocoatl/axocoatl/actions/artifacts/12711171982/zip",
        "workflow_run": {
          "id": 40439869836,
          "repository_id": 1258994885,
          "head_repository_id": 1258994885,
          "head_branch": "v1.5.0",
          "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
        }
      },
      {
        "id": 12711066172,
        "name": "verified-release-40439869836-v1.5.0",
        "digest": "sha256:2ca1f79d29d21425a395c60212ba8febdd90260f56666dfbce527e32f1444dfc",
        "expired": false,
        "size_in_bytes": 134430046,
        "archive_download_url": "https://api.github.com/repos/axocoatl/axocoatl/actions/artifacts/12711066172/zip",
        "workflow_run": {
          "id": 40439869836,
          "repository_id": 1258994885,
          "head_repository_id": 1258994885,
          "head_branch": "v1.5.0",
          "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
        }
      },
      {
        "id": 12710798714,
        "name": "release-40439869836-x86_64-apple-darwin",
        "digest": "sha256:ca25448ea2e58b214219ad9e1a53e30f277fb785a2f235598c7a10fc9cc9afca",
        "expired": false,
        "size_in_bytes": 34245000,
        "archive_download_url": "https://api.github.com/repos/axocoatl/axocoatl/actions/artifacts/12710798714/zip",
        "workflow_run": {
          "id": 40439869836,
          "repository_id": 1258994885,
          "head_repository_id": 1258994885,
          "head_branch": "v1.5.0",
          "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
        }
      },
      {
        "id": 12710606309,
        "name": "release-40439869836-aarch64-apple-darwin",
        "digest": "sha256:e65ea56c9b62dda1f1049f357ec24ce6a71539b0072fc46182a477a8bc65db75",
        "expired": false,
        "size_in_bytes": 32070044,
        "archive_download_url": "https://api.github.com/repos/axocoatl/axocoatl/actions/artifacts/12710606309/zip",
        "workflow_run": {
          "id": 40439869836,
          "repository_id": 1258994885,
          "head_repository_id": 1258994885,
          "head_branch": "v1.5.0",
          "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
        }
      },
      {
        "id": 12709956602,
        "name": "release-40439869836-x86_64-unknown-linux-gnu",
        "digest": "sha256:9eac4c11d866f04f2c9b68887a90eb5c25af9923fce9e624eee3a3dd0cf11280",
        "expired": false,
        "size_in_bytes": 34926658,
        "archive_download_url": "https://api.github.com/repos/axocoatl/axocoatl/actions/artifacts/12709956602/zip",
        "workflow_run": {
          "id": 40439869836,
          "repository_id": 1258994885,
          "head_repository_id": 1258994885,
          "head_branch": "v1.5.0",
          "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
        }
      },
      {
        "id": 12709524494,
        "name": "release-40439869836-aarch64-unknown-linux-gnu",
        "digest": "sha256:9f75cdf7ec8aa91a8386007a690f3482248310ffaf63a145de66982278ef35f1",
        "expired": false,
        "size_in_bytes": 33188410,
        "archive_download_url": "https://api.github.com/repos/axocoatl/axocoatl/actions/artifacts/12709524494/zip",
        "workflow_run": {
          "id": 40439869836,
          "repository_id": 1258994885,
          "head_repository_id": 1258994885,
          "head_branch": "v1.5.0",
          "head_sha": "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
        }
      }
    ]
  }
}''')


class SourceValidation(unittest.TestCase):
    def setUp(self):
        self.data = copy.deepcopy(FIXTURE)

    def validate(self):
        return validator.validate(self.data["run"], self.data["jobs"], self.data["artifacts"])

    def assert_refused(self):
        with self.assertRaises(validator.InvalidSource):
            self.validate()

    def test_actual_complete_snapshot(self):
        self.assertEqual(self.validate()["artifact_id"], 12711066172)
        self.assertEqual(self.validate()["successful_jobs"], 15)

    def test_each_run_binding_is_required(self):
        for key in ("id", "run_attempt", "head_branch", "head_sha", "path", "event", "status", "conclusion"):
            with self.subTest(key=key):
                self.setUp()
                self.data["run"][key] = "wrong"
                self.assert_refused()

    def test_repository_and_commit_bindings(self):
        for parent, key in (("repository", "full_name"), ("repository", "id"),
                            ("head_repository", "full_name"), ("head_repository", "id"),
                            ("head_commit", "id")):
            with self.subTest(parent=parent, key=key):
                self.setUp()
                self.data["run"][parent][key] = "wrong"
                self.assert_refused()

    def test_boolean_is_not_an_attempt_number(self):
        self.data["run"]["run_attempt"] = True
        self.assert_refused()

    def test_every_successful_job_is_required(self):
        for index in range(15):
            with self.subTest(index=index):
                self.setUp()
                self.data["jobs"]["jobs"][index]["conclusion"] = "failure"
                self.assert_refused()

    def test_job_status_and_bindings(self):
        for key in ("id", "name", "run_id", "run_attempt", "head_sha", "status"):
            with self.subTest(key=key):
                self.setUp()
                self.data["jobs"]["jobs"][0][key] = "wrong"
                self.assert_refused()

    def test_cancelled_pending_or_skipped_success_rejected(self):
        for status, conclusion in (("completed", "cancelled"), ("in_progress", None),
                                   ("queued", None), ("completed", "skipped")):
            with self.subTest(status=status, conclusion=conclusion):
                self.setUp()
                self.data["jobs"]["jobs"][0].update(status=status, conclusion=conclusion)
                self.assert_refused()

    def test_downstream_jobs_must_still_be_skipped(self):
        for index in range(16, 20):
            with self.subTest(index=index):
                self.setUp()
                self.data["jobs"]["jobs"][index]["conclusion"] = "success"
                self.assert_refused()

    def test_missing_extra_duplicate_or_paginated_jobs_rejected(self):
        for mutation in ("missing", "extra", "duplicate", "pagination"):
            with self.subTest(mutation=mutation):
                self.setUp()
                document = self.data["jobs"]
                if mutation == "missing":
                    document["jobs"].pop()
                    document["total_count"] -= 1
                elif mutation == "extra":
                    document["jobs"].append(copy.deepcopy(document["jobs"][0]))
                    document["total_count"] += 1
                elif mutation == "duplicate":
                    document["jobs"][0] = copy.deepcopy(document["jobs"][1])
                else:
                    document["total_count"] += 1
                self.assert_refused()

    def test_draft_failure_cannot_move_to_another_step(self):
        steps = self.data["jobs"]["jobs"][15]["steps"]
        steps[7]["conclusion"] = "success"
        steps[3]["conclusion"] = "failure"
        self.assert_refused()

    def test_draft_step_identity_status_and_inventory(self):
        for mutation in ("name", "number", "status", "missing", "duplicate"):
            with self.subTest(mutation=mutation):
                self.setUp()
                steps = self.data["jobs"]["jobs"][15]["steps"]
                if mutation == "missing":
                    steps.pop()
                elif mutation == "duplicate":
                    steps[0] = copy.deepcopy(steps[1])
                else:
                    steps[0][mutation] = "wrong"
                self.assert_refused()

    def test_artifact_identity_digest_expiry_and_download_url(self):
        for key, value in (("id", 1), ("name", "other"), ("digest", "sha256:" + "0" * 64),
                           ("expired", True), ("expired", 0), ("size_in_bytes", 1),
                           ("archive_download_url", "https://example.com/wrong.zip")):
            with self.subTest(key=key, value=value):
                self.setUp()
                self.data["artifacts"]["artifacts"][2][key] = value
                self.assert_refused()

    def test_every_artifact_run_binding_is_required(self):
        for key in ("id", "repository_id", "head_repository_id", "head_branch", "head_sha"):
            with self.subTest(key=key):
                self.setUp()
                self.data["artifacts"]["artifacts"][2]["workflow_run"][key] = "wrong"
                self.assert_refused()

    def test_missing_ambiguous_and_paginated_artifacts_rejected(self):
        for mutation in ("missing", "duplicate", "duplicate_name", "pagination"):
            with self.subTest(mutation=mutation):
                self.setUp()
                document = self.data["artifacts"]
                if mutation == "missing":
                    document["artifacts"].pop(2)
                    document["total_count"] -= 1
                elif mutation in ("duplicate", "duplicate_name"):
                    duplicate = copy.deepcopy(document["artifacts"][2])
                    if mutation == "duplicate_name":
                        duplicate["id"] += 1
                    document["artifacts"].append(duplicate)
                    document["total_count"] += 1
                else:
                    document["total_count"] += 1
                self.assert_refused()

    def test_malformed_collection_types_rejected(self):
        for key in ("jobs", "artifacts"):
            with self.subTest(key=key):
                self.setUp()
                self.data[key][key] = {}
                self.assert_refused()

    def test_actual_cli_and_bad_json_fail_closed(self):
        with tempfile.TemporaryDirectory() as temp:
            paths = [Path(temp) / (key + ".json") for key in ("run", "jobs", "artifacts")]
            for key, path in zip(("run", "jobs", "artifacts"), paths):
                path.write_text(json.dumps(self.data[key]), encoding="utf-8")
            command = [sys.executable, str(SCRIPT), *map(str, paths)]
            good = subprocess.run(command, text=True, capture_output=True, check=False)
            self.assertEqual(good.returncode, 0, good.stderr)
            self.assertTrue(json.loads(good.stdout)["verified"])
            for bad_json in ('{"id": 1, "id": 2}', "{", "null"):
                paths[0].write_text(bad_json, encoding="utf-8")
                bad = subprocess.run(command, text=True, capture_output=True, check=False)
                self.assertNotEqual(bad.returncode, 0)
                self.assertEqual(bad.stdout, "")
                self.assertIn("Recovery source refused", bad.stderr)

    def test_cli_requires_all_three_files(self):
        result = subprocess.run([sys.executable, str(SCRIPT)], text=True, capture_output=True, check=False)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
