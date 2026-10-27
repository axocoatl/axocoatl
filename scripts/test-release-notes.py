import hashlib
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

import importlib.util

_spec = importlib.util.spec_from_file_location("release_notes", Path(__file__).with_name("release-notes.py"))
notes = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(notes)


SHA = "19e3c89ca7b4d02cd742bccd671f7274e68b95d7"
HEADING = b"## [1.5.0] - 2026-10-22\n"
AWK = r'''/^## \[/ {
  if (found) exit
  if ($0 == heading || index($0, heading " ") == 1) found = 1
}
found { print }
END { if (!found) exit 1 }'''


class ExtractionTests(unittest.TestCase):
    def test_matches_actual_workflow_awk_byte_for_byte(self):
        self.assertIsNotNone(shutil.which("awk"), "awk parity is a required test")
        cases = [
            b"# Changelog\n\n" + HEADING + b"\n### Fixed\n- one\n\n## [1.4.0]\nold\n",
            b"## [1.5.01]\nwrong\n## [1.5.0]suffix\nwrong\n" + HEADING + b"last line",
            HEADING + "- naïve 🐍\r\n\r\n".encode(),
            b"## [1.5.0]\n\n",
            b"## [1.5.0] - date\r\nbody\r\n## [1.4.0]\r\n",
            HEADING + b"first\n## [1.5.0] - repeated\nsecond\n",
            HEADING + b"```markdown\n## [quoted heading]\n```\n",
        ]
        for data in cases:
            with self.subTest(data=data[:50]):
                expected = subprocess.run(
                    ["awk", "-v", "heading=## [1.5.0]", AWK],
                    input=data, capture_output=True, check=True,
                ).stdout
                self.assertEqual(notes.release_notes(data, "1.5.0", SHA), expected)

    def test_missing_version_and_similar_prefixes_are_refused(self):
        for data in [b"", b"## [1.5.01]\n", b"## [1.5.0]suffix\n", b"## [1.5.0]\tdate\n"]:
            with self.subTest(data=data), self.assertRaises(notes.NotesError):
                notes.release_notes(data, "1.5.0", SHA)

    def test_repeated_subheadings_are_not_release_boundaries(self):
        data = HEADING + b"### Fixed\n- a\n### Fixed\n- b\n"
        self.assertEqual(notes.release_notes(data, "1.5.0", SHA), data)

    def test_unterminated_final_line_gets_original_awk_newline(self):
        self.assertEqual(notes.release_notes(HEADING + b"tail", "1.5.0", SHA), HEADING + b"tail\n")


class BoundTests(unittest.TestCase):
    def section_of_bytes(self, size):
        return HEADING + b"x" * (size - len(HEADING) - 1) + b"\n"

    def test_at_and_below_hard_byte_limit_are_exact(self):
        for size in [124_999, 125_000]:
            with self.subTest(size=size):
                data = self.section_of_bytes(size)
                self.assertEqual(notes.release_notes(data, "1.5.0", SHA), data)

    def test_one_byte_over_uses_frozen_permalink_and_full_digest(self):
        data = self.section_of_bytes(125_001)
        output = notes.release_notes(data, "1.5.0", SHA)
        self.assertLessEqual(len(output), 125_000)
        self.assertTrue(output.startswith(HEADING + b"\n"))
        self.assertIn(f"/blob/{SHA}/CHANGELOG.md".encode(), output)
        self.assertIn(hashlib.sha256(data).hexdigest().encode(), output)
        self.assertIn(b"remaining entries are linked", output)
        self.assertNotIn(b"xxxxxxxxxxxxxxxx", output)
        self.assertEqual(output, notes.release_notes(data, "1.5.0", SHA))

    def test_unicode_limit_is_utf8_bytes_not_codepoints(self):
        data = HEADING + ("🐍" * 31_250).encode() + b"\n"
        self.assertLess(len(data.decode()), 125_000)
        self.assertGreater(len(data), 125_000)
        output = notes.release_notes(data, "1.5.0", SHA)
        self.assertLess(len(output), 125_000)
        self.assertNotIn("🐍".encode(), output)
        short = HEADING + "- café 🐍\n".encode()
        self.assertEqual(notes.release_notes(short, "1.5.0", SHA), short)

    def test_oversized_fences_lists_and_subsections_are_never_partial(self):
        bodies = [
            b"### Fixed\n- first item\n  continued\n" + b"  body\n" * 20_000,
            b"### Fixed\n```python\n" + b"print(1)\n" * 20_000 + b"```\n### More\n- last\n",
            b"### Fixed\n~~~\n" + b"### not a subsection\n" * 10_000 + b"~~~\n",
            b"### Fixed\n```unclosed\n" + b"code\n" * 30_000,
        ]
        for body in bodies:
            with self.subTest(prefix=body[:40]):
                output = notes.release_notes(HEADING + body, "1.5.0", SHA)
                self.assertTrue(output.startswith(HEADING))
                self.assertNotIn(b"### Fixed", output)
                self.assertNotIn(b"```", output)
                self.assertNotIn(b"~~~", output)
                self.assertNotIn(b"- first item", output)
                self.assertIn(hashlib.sha256(HEADING + body).hexdigest().encode(), output)

    def test_digest_covers_only_first_matching_release_section(self):
        section = self.section_of_bytes(125_001)
        data = b"ignored\n" + section + b"## [1.5.0] - repeated\nignored too\n"
        output = notes.release_notes(data, "1.5.0", SHA)
        self.assertIn(hashlib.sha256(section).hexdigest().encode(), output)
        self.assertNotIn(hashlib.sha256(data).hexdigest().encode(), output)

    def test_oversized_heading_is_refused_instead_of_cut(self):
        with self.assertRaisesRegex(notes.NotesError, "heading is too large"):
            notes.release_notes(b"## [1.5.0] " + b"x" * 125_000 + b"\n", "1.5.0", SHA)


class ValidationAndCliTests(unittest.TestCase):
    def test_invalid_versions_and_shas(self):
        for version in ["v1.5.0", "1.5", "01.5.0", "1.5.0-01", "1.5.0]\n## [x", "1.5.0/"]:
            with self.subTest(version=version), self.assertRaises(notes.NotesError):
                notes.release_notes(HEADING, version, SHA)
        for sha in ["19e3", "g" * 40, SHA + "\n", "../" + "a" * 37]:
            with self.subTest(sha=sha), self.assertRaises(notes.NotesError):
                notes.release_notes(HEADING, "1.5.0", sha)

    def test_valid_semver_and_uppercase_sha(self):
        version = "1.5.0-rc.1+build.123"
        data = f"## [{version}]\ntext\n".encode()
        self.assertEqual(notes.release_notes(data, version, SHA.upper()), data)

    def test_invalid_selected_utf8_is_refused(self):
        with self.assertRaisesRegex(notes.NotesError, "UTF-8"):
            notes.release_notes(HEADING + b"\xff\n", "1.5.0", SHA)

    def test_cli_writes_exact_bytes_and_preserves_output_on_error(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "CHANGELOG.md"
            output = Path(directory) / "notes.md"
            data = HEADING + "- café\r\n".encode()
            source.write_bytes(data)
            command = [sys.executable, str(Path(notes.__file__)), str(source),
                       "1.5.0", SHA, str(output)]
            subprocess.run(command, check=True, capture_output=True)
            self.assertEqual(output.read_bytes(), data)
            source.write_bytes(b"## [1.4.0]\n")
            result = subprocess.run(command, capture_output=True)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(output.read_bytes(), data)
            self.assertEqual(sorted(p.name for p in Path(directory).iterdir()), ["CHANGELOG.md", "notes.md"])

    def test_cli_cannot_overwrite_changelog(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "CHANGELOG.md"
            source.write_bytes(HEADING)
            result = subprocess.run(
                [sys.executable, str(Path(notes.__file__)), str(source),
                 "1.5.0", SHA, str(source)],
                capture_output=True,
            )
            self.assertEqual(result.returncode, 2)
            self.assertEqual(source.read_bytes(), HEADING)


if __name__ == "__main__":
    unittest.main()
