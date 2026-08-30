#!/usr/bin/env python3
"""Create Axocoatl's release tarball with stable metadata and gzip headers."""

from __future__ import annotations

import gzip
import os
from pathlib import Path
import stat
import sys
import tarfile


ENTRIES = (
    ("LICENSE", 0o644),
    ("THIRD_PARTY_LICENSES.txt", 0o644),
    ("axocoatl", 0o755),
)


def fail(message: str) -> "NoReturn":
    raise SystemExit(f"create-release-archive: {message}")


def main() -> None:
    if len(sys.argv) != 3:
        fail("usage: create-release-archive.py <archive.tar.gz> <staging-directory>")

    archive = Path(sys.argv[1])
    staging = Path(sys.argv[2])
    if archive.exists() or archive.is_symlink():
        fail(f"refusing to overwrite {archive}")
    if not staging.is_dir() or staging.is_symlink():
        fail(f"staging directory is missing or is a symlink: {staging}")

    inputs: list[tuple[str, int, Path]] = []
    for name, mode in ENTRIES:
        source = staging / name
        try:
            source_stat = source.lstat()
        except FileNotFoundError:
            fail(f"required entry is missing: {source}")
        if not stat.S_ISREG(source_stat.st_mode):
            fail(f"required entry is not a regular file: {source}")
        inputs.append((name, mode, source))

    archive.parent.mkdir(parents=True, exist_ok=True)
    try:
        with archive.open("xb") as raw_archive:
            # filename="" and mtime=0 remove host path and wall-clock data from
            # the gzip header. The USTAR members below also use fixed ownership,
            # mode, order, and timestamps.
            with gzip.GzipFile(
                filename="",
                mode="wb",
                compresslevel=9,
                fileobj=raw_archive,
                mtime=0,
            ) as compressed:
                with tarfile.open(
                    fileobj=compressed,
                    mode="w",
                    format=tarfile.USTAR_FORMAT,
                ) as tar:
                    for name, mode, source in inputs:
                        info = tarfile.TarInfo(name)
                        info.size = source.stat().st_size
                        info.mode = mode
                        info.uid = 0
                        info.gid = 0
                        info.uname = "root"
                        info.gname = "root"
                        info.mtime = 0
                        with source.open("rb") as contents:
                            tar.addfile(info, contents)
        os.chmod(archive, 0o644)
    except BaseException:
        try:
            archive.unlink()
        except FileNotFoundError:
            pass
        raise


if __name__ == "__main__":
    main()
