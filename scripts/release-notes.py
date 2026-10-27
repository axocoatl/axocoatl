#!/usr/bin/env python3
"""Extract release.yml's changelog section, with a deterministic GitHub-size fallback.

The unabridged extraction is byte-for-byte equivalent to the release workflow's
awk program (including its final LF and first matching heading behavior). Bodies
of at most 125,000 UTF-8 bytes are unchanged. Larger bodies retain only the exact
release heading plus a notice, frozen changelog permalink and full-section digest;
no subsection, list or code fence is cut into a partial excerpt.
"""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import re
import tempfile


MAX_BODY_BYTES = 125_000
REPOSITORY = "https://github.com/axocoatl/axocoatl"
_NUMBER = r"(?:0|[1-9][0-9]*)"
_PRERELEASE = r"(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
_VERSION = re.compile(
    rf"{_NUMBER}\.{_NUMBER}\.{_NUMBER}"
    rf"(?:-{_PRERELEASE}(?:\.{_PRERELEASE})*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)
_COMMIT = re.compile(r"[0-9a-fA-F]{40}")


class NotesError(ValueError):
    """Invalid input or no safe bounded representation."""


def validate_identity(version: str, commit: str) -> str:
    if not _VERSION.fullmatch(version):
        raise NotesError("version must be a complete SemVer without a leading v")
    if not _COMMIT.fullmatch(commit):
        raise NotesError("commit must be the full 40-hex-digit release SHA")
    return commit.lower()


def extract_section(changelog: bytes, version: str) -> bytes:
    """Match release.yml's awk extraction exactly, rather than parse Markdown.

    awk consumes LF-separated records, preserves CR and every other input byte,
    and `print` adds one LF even to a final unterminated record. Any following
    column-zero `## [` heading terminates the first matched release section,
    including a repeated version or a heading-shaped line inside a code fence.
    """
    heading = f"## [{version}]".encode("ascii")
    records = changelog.split(b"\n")
    if records[-1] == b"":
        records.pop()  # A terminating LF is not an additional empty awk record.
    found = False
    selected: list[bytes] = []
    for record in records:
        if record.startswith(b"## ["):
            if found:
                break
            if record == heading or record.startswith(heading + b" "):
                found = True
        if found:
            selected.append(record + b"\n")
    if not found:
        raise NotesError(f"no matching changelog heading for {version}")
    section = b"".join(selected)
    try:
        section.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        raise NotesError("the selected changelog section is not valid UTF-8") from error
    return section


def release_notes(changelog: bytes, version: str, commit: str) -> bytes:
    commit = validate_identity(version, commit)
    section = extract_section(changelog, version)
    if len(section) <= MAX_BODY_BYTES:
        return section
    heading = section.partition(b"\n")[0] + b"\n"
    digest = hashlib.sha256(section).hexdigest()
    notice = (
        f"\nThe complete {version} change notes exceed GitHub's release-note size "
        "limit. All entries are preserved in the "
        f"[full changelog at the release commit]({REPOSITORY}/blob/{commit}/CHANGELOG.md). "
        "The remaining entries are linked there rather than partially reproduced here.\n\n"
        f"SHA256 of the complete extracted changelog section: `{digest}`.\n"
    ).encode("utf-8")
    bounded = heading + notice
    if len(bounded) > MAX_BODY_BYTES:
        raise NotesError("release heading is too large to preserve within GitHub's size limit")
    return bounded


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("changelog", type=Path)
    parser.add_argument("version")
    parser.add_argument("commit")
    parser.add_argument("output", type=Path)
    args = parser.parse_args(argv)
    temporary: str | None = None
    try:
        if args.changelog.resolve() == args.output.resolve():
            raise NotesError("output must not replace the input changelog")
        notes = release_notes(args.changelog.read_bytes(), args.version, args.commit)
        # Validate first, then atomically replace the explicitly requested output.
        with tempfile.NamedTemporaryFile(dir=args.output.parent, delete=False) as stream:
            temporary = stream.name
            stream.write(notes)
        os.replace(temporary, args.output)
        temporary = None
    except (NotesError, OSError) as error:
        parser.error(str(error))
    finally:
        if temporary is not None:
            Path(temporary).unlink(missing_ok=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
