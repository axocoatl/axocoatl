#!/usr/bin/env python3
"""Bounded supervisor artifact verification and self-contained negative tests.

The build wrapper uses snapshot/package/verify. `self-test` executes only Python
fixtures, never Cargo, a generated ELF, or a container. ELF structure and hashes
are necessary packaging checks, not proof that a payload implements the protocol.
Actual helper tests in Alpine and Debian remain a separate required proof.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import struct
import tarfile
import tempfile
import tomllib
import unittest


MAX_BINARY_BYTES = 64 * 1024 * 1024
MAX_SOURCE_BYTES = 64 * 1024 * 1024
MAX_MANIFEST_BYTES = 512 * 1024
ARCHITECTURES = {"x86_64": 62, "aarch64": 183}
TOOLCHAIN = "1.95.0"
SCHEMA_VERSION = 2


class ArtifactError(ValueError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ArtifactError(message)


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def encoded(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True) + "\n").encode()


def regular_bytes(path: Path, limit: int) -> bytes:
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
    with os.fdopen(os.open(path, flags), "rb") as source:
        metadata = os.fstat(source.fileno())
        require(stat.S_ISREG(metadata.st_mode), f"not a regular file: {path}")
        require(metadata.st_size <= limit, f"file exceeds size bound: {path}")
        data = source.read(limit + 1)
        require(len(data) <= limit, f"file grew beyond size bound: {path}")
        require(len(data) == metadata.st_size, f"file changed during read: {path}")
        return data


def inspect_elf(data: bytes, architecture: str) -> dict:
    require(architecture in ARCHITECTURES, "unsupported architecture")
    require(64 <= len(data) <= MAX_BINARY_BYTES, "ELF size is outside bounds")
    fields = struct.unpack_from("<16sHHIQQQIHHHHHH", data)
    ident, kind, machine, version, entry, phoff, _, _, ehsize, phsize, phcount, _, _, _ = fields
    require(ident[:7] == b"\x7fELF\x02\x01\x01", "expected little-endian ELF64 version 1")
    require(ident[7] in (0, 3), "unsupported ELF ABI")
    require(kind in (2, 3) and version == 1 and ehsize == 64, "not a supported Linux executable ELF")
    require(machine == ARCHITECTURES[architecture], "ELF architecture differs from payload architecture")
    require(phsize == 56 and 1 <= phcount <= 256 and phoff >= 64, "invalid ELF program-header table")
    require(phoff + phsize * phcount <= len(data), "truncated ELF program-header table")
    executable_entry = False
    for index in range(phcount):
        ptype, flags, offset, address, _, file_size, memory_size, alignment = struct.unpack_from(
            "<IIQQQQQQ", data, phoff + index * phsize
        )
        require(offset + file_size <= len(data), "program segment exceeds retained ELF")
        require(ptype != 3, "PT_INTERP requires an external dynamic loader")
        if ptype == 1:
            require(file_size <= memory_size, "LOAD file size exceeds memory size")
            require(address + memory_size <= (1 << 64), "LOAD address overflow")
            require(alignment in (0, 1) or alignment & (alignment - 1) == 0, "invalid LOAD alignment")
            require(alignment <= 1 or address % alignment == offset % alignment, "LOAD offset alignment differs")
            executable_entry |= bool(flags & 1 and address <= entry < address + file_size)
        if ptype == 2:
            require(file_size >= 16 and file_size % 16 == 0, "invalid dynamic table")
            terminated = False
            for position in range(offset, offset + file_size, 16):
                tag, _ = struct.unpack_from("<qQ", data, position)
                require(tag not in (1, 0x7FFFFFFD, 0x7FFFFFFF), "ELF requires a dynamic dependency")
                if tag == 0:
                    terminated = True
                    break
            require(terminated, "unterminated dynamic table")
    require(executable_entry, "entry point is not in an executable file-backed LOAD")
    return {"architecture": architecture, "format": "elf64-little-endian", "type": "exec" if kind == 2 else "static-pie",
            "external_interpreter": False, "dynamic_dependencies": False}


def helper_source_fingerprint(entries: list[dict], version: str) -> str:
    value = bytearray(f"axocoatl-exec-source-v1\nversion={version}\n".encode())
    for entry in entries:
        value.extend(f"{entry['path']}\0{entry['bytes']}\0{entry['sha256']}\n".encode())
    return digest(value)


def helper_source(crate: Path, version: str) -> dict:
    """Fingerprint Rust source without Cargo's rewritten manifest or lockfile.

    Matches axocoatl-exec/build.rs. The separate complete workspace snapshot
    binds dependencies, build configuration and tooling at artifact creation.
    """
    require(not crate.is_symlink(), "helper crate cannot be symlinked")
    paths = [crate / "build.rs"]
    require((crate / "src").is_dir() and not (crate / "src").is_symlink(), "helper source directory missing or symlinked")
    for directory, directories, files in os.walk(crate / "src", followlinks=False):
        for name in directories + files:
            require(not (Path(directory) / name).is_symlink(), "helper source cannot be symlinked")
        paths.extend(Path(directory) / name for name in files if name.endswith(".rs"))
        require(len(paths) <= 4096, "helper source file count exceeds bound")
    entries = []
    total = 0
    for path in sorted(paths):
        relative = path.relative_to(crate).as_posix()
        require(not any(character in relative for character in "\0\r\n"), "invalid helper source path")
        data = regular_bytes(path, MAX_SOURCE_BYTES)
        total += len(data)
        require(total <= MAX_SOURCE_BYTES, "helper source inventory exceeds byte bound")
        entries.append({"path": relative, "bytes": len(data), "sha256": digest(data)})
    return {"files": entries, "sha256": helper_source_fingerprint(entries, version)}


def dependency_groups(manifest: dict) -> list[tuple[str, dict]]:
    result = [(section, manifest.get(section, {})) for section in ("dependencies", "build-dependencies", "dev-dependencies")]
    for target, values in sorted(manifest.get("target", {}).items()):
        result.extend((f"target.{target}.{section}", values.get(section, {}))
                      for section in ("dependencies", "build-dependencies", "dev-dependencies"))
    return result


def resolved_dependency_groups(manifest: dict, workspace: dict) -> dict:
    groups = {}
    for group, dependencies in dependency_groups(manifest):
        if not dependencies:
            continue
        resolved = {}
        for name, declaration in dependencies.items():
            declaration = {"version": declaration} if isinstance(declaration, str) else declaration.copy()
            if declaration.pop("workspace", False):
                inherited = workspace["workspace"]["dependencies"][name]
                inherited = {"version": inherited} if isinstance(inherited, str) else inherited.copy()
                if "features" in declaration:
                    declaration["features"] = sorted(set(inherited.get("features", []) + declaration["features"]))
                declaration = {**inherited, **declaration}
            declaration.pop("path", None)
            if "features" in declaration:
                declaration["features"] = sorted(declaration["features"])
            resolved[name] = declaration
        groups[group] = resolved
    return groups


def relevant_build_inputs(root: Path, workspace: dict) -> dict:
    manifest_data = regular_bytes(root / "crates/axocoatl-exec/Cargo.toml", MAX_SOURCE_BYTES)
    manifest = tomllib.loads(manifest_data.decode())
    inherited_package = {name: workspace["workspace"]["package"][name]
                         for name, value in manifest["package"].items()
                         if isinstance(value, dict) and value.get("workspace") is True}
    inherited_dependencies = {}
    for _, group in dependency_groups(manifest):
        for name, value in group.items():
            if isinstance(value, dict) and value.get("workspace") is True:
                inherited_dependencies[name] = workspace["workspace"]["dependencies"][name]
    lock = tomllib.loads(regular_bytes(root / "Cargo.lock", MAX_SOURCE_BYTES).decode())
    packages = lock.get("package", [])
    require(isinstance(packages, list) and len(packages) <= 16384, "invalid or excessive locked package inventory")
    roots = [package for package in packages if package.get("name") == "axocoatl-exec" and "source" not in package]
    require(len(roots) == 1, "lockfile must contain exactly one local axocoatl-exec package")
    pending = roots.copy()
    selected = {}
    while pending:
        package = pending.pop()
        identity = (package["name"], package["version"], package.get("source", ""))
        if identity in selected:
            continue
        require(len(selected) < 4096, "helper dependency closure exceeds bound")
        require("source" in package or package["name"] == "axocoatl-exec",
                "additional local helper dependencies require expanded build-input fingerprints")
        selected[identity] = package
        require("replace" not in package, "locked replacements require explicit build-input support")
        for dependency in package.get("dependencies", []):
            match = re.fullmatch(r"([^\s]+)(?: ([^\s]+))?(?: \(([^\r\n]+)\))?", dependency)
            require(match is not None, "unsupported locked dependency identity")
            name, version, source = match.groups()
            candidates = [item for item in packages if item.get("name") == name
                          and (version is None or item.get("version") == version)
                          and (source is None or item.get("source") == source)]
            require(len(candidates) == 1, f"locked dependency is missing or ambiguous: {dependency}")
            pending.append(candidates[0])
    file_inputs = []
    for relative in (".cargo/config", ".cargo/config.toml", "scripts/build-exec-supervisor.sh", "scripts/test-exec-supervisor-artifact.py"):
        path = root / relative
        if relative.startswith(".cargo/") and not path.exists() and not path.is_symlink():
            continue
        data = regular_bytes(path, MAX_SOURCE_BYTES)
        file_inputs.append({"path": relative, "bytes": len(data), "sha256": digest(data)})
    inputs = {"schema_version": 1, "helper_manifest": {"bytes": len(manifest_data), "sha256": digest(manifest_data)},
              "inherited_package": inherited_package, "inherited_dependencies": inherited_dependencies,
              "resolved_dependencies": resolved_dependency_groups(manifest, workspace),
              "profiles": workspace.get("profile", {}), "resolver": workspace["workspace"].get("resolver"),
              "patches": workspace.get("patch", {}), "build_files": sorted(file_inputs, key=lambda entry: entry["path"]),
              "locked_packages": [selected[key] for key in sorted(selected)]}
    return {"inputs": inputs, "sha256": digest(encoded(inputs))}


def source_snapshot(root: Path) -> dict:
    root = root.resolve(strict=True)
    workspace = tomllib.loads(regular_bytes(root / "Cargo.toml", MAX_SOURCE_BYTES).decode())
    paths = {root / "Cargo.toml", root / "Cargo.lock"}
    for name in ("config", "config.toml"):
        candidate = root / ".cargo" / name
        if candidate.exists() or candidate.is_symlink():
            paths.add(candidate)
    # Include tooling that defines artifact acceptance, in addition to all
    # source bytes of local dependencies. Lockfile checksums bind registry inputs.
    for name in ("build-exec-supervisor.sh", "test-exec-supervisor-artifact.py"):
        paths.add(root / "scripts" / name)
    pending = [root / "crates" / "axocoatl-exec"]
    visited: set[Path] = set()
    while pending:
        crate = pending.pop()
        require(not crate.is_symlink(), "local source crate cannot be a symlink")
        crate = crate.resolve(strict=True)
        require(crate.is_relative_to(root), "local dependency leaves the source root")
        if crate in visited:
            continue
        visited.add(crate)
        require(len(visited) <= 128, "local dependency count exceeds bound")
        manifest = tomllib.loads(regular_bytes(crate / "Cargo.toml", MAX_SOURCE_BYTES).decode())
        groups = [manifest.get(section, {}) for section in ("dependencies", "build-dependencies", "dev-dependencies")]
        for target in manifest.get("target", {}).values():
            groups.extend(target.get(section, {}) for section in ("dependencies", "build-dependencies", "dev-dependencies"))
        for group in groups:
            for name, dependency in group.items():
                if isinstance(dependency, dict) and dependency.get("workspace") is True:
                    dependency = workspace.get("workspace", {}).get("dependencies", {}).get(name, {})
                    base = root
                else:
                    base = crate
                if isinstance(dependency, dict) and "path" in dependency:
                    pending.append(base / dependency["path"])
        for directory, directories, files in os.walk(crate, followlinks=False):
            directories[:] = sorted(name for name in directories if name not in (".git", "target", "__pycache__"))
            for name in directories:
                require(not (Path(directory) / name).is_symlink(), "source directory cannot be a symlink")
            for name in files:
                paths.add(Path(directory) / name)
                require(len(paths) <= 4096, "source file count exceeds bound")
    # Local patches/replacements can change dependency code despite a lockfile.
    # Require them to be represented rather than publishing a partial inventory.
    for patch in workspace.get("patch", {}).values():
        require(not any(isinstance(value, dict) and "path" in value for value in patch.values()),
                "local workspace patches require an expanded source inventory")
    require(not workspace.get("replace"), "workspace replacements require an expanded source inventory")
    entries = []
    total = 0
    for path in sorted(paths):
        require(not path.is_symlink(), f"source file cannot be a symlink: {path}")
        data = regular_bytes(path, MAX_SOURCE_BYTES)
        total += len(data)
        require(total <= MAX_SOURCE_BYTES, "source inventory exceeds byte bound")
        entries.append({"path": path.relative_to(root).as_posix(), "bytes": len(data), "sha256": digest(data)})
    package = tomllib.loads(regular_bytes(root / "crates/axocoatl-exec/Cargo.toml", MAX_SOURCE_BYTES).decode())["package"]
    version = package["version"]
    if isinstance(version, dict) and version.get("workspace") is True:
        version = workspace["workspace"]["package"]["version"]
    require(isinstance(version, str) and re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.+-]+)?", version) is not None,
            "invalid supervisor package version")
    protocol_source = regular_bytes(root / "crates/axocoatl-exec/src/protocol.rs", MAX_SOURCE_BYTES).decode()
    matches = re.findall(r"^pub const PROTOCOL_VERSION: u32 = ([0-9]+);$", protocol_source, re.MULTILINE)
    require(len(matches) == 1 and int(matches[0]) > 0, "cannot identify exact supervisor protocol version")
    return {"package_version": version, "protocol_version": int(matches[0]),
            "helper_source": helper_source(root / "crates/axocoatl-exec", version),
            "build_inputs": relevant_build_inputs(root, workspace),
            "source_files": entries, "source_sha256": digest(encoded(entries))}


def names(architecture: str) -> tuple[str, str]:
    require(architecture in ARCHITECTURES, "unsupported architecture")
    binary = f"axocoatl-exec-supervisor-linux-{architecture}"
    return binary, f"{binary}.manifest.json"


def write_file(path: Path, data: bytes, mode: int) -> None:
    with path.open("xb") as destination:
        destination.write(data)
        destination.flush()
        os.fchmod(destination.fileno(), mode)
        os.fsync(destination.fileno())


def package(root: Path, snapshot_path: Path, binary: Path, architecture: str, output: Path, rustc_path: Path, linker: Path) -> None:
    before = json.loads(regular_bytes(snapshot_path, MAX_MANIFEST_BYTES))
    require(source_snapshot(root) == before, "source changed during supervisor build")
    payload = regular_bytes(binary, MAX_BINARY_BYTES)
    elf = inspect_elf(payload, architecture)
    rustc = regular_bytes(rustc_path, 4096).decode()
    require(re.search(r"^release: 1\.95\.0$", rustc, re.MULTILINE) is not None, "build did not use pinned Rust 1.95.0")
    linker_data = regular_bytes(linker, MAX_BINARY_BYTES)
    binary_name, manifest_name = names(architecture)
    manifest = {"schema_version": SCHEMA_VERSION, "package": "axocoatl-exec", "binary": "axocoatl-exec-supervisor", **before,
                "build": {"toolchain": TOOLCHAIN, "rustc": rustc, "target": f"{architecture}-unknown-linux-musl", "profile": "release",
                          "locked": True, "offline": True, "jobs": 1, "crt_static": True, "remap_source_prefix": "/axocoatl-source",
                          "linker_name": linker.name, "linker_sha256": digest(linker_data)},
                "payload": {"name": binary_name, "bytes": len(payload), "sha256": digest(payload), "elf": elf},
                "verification": {"elf_structure": True, "container_execution": False}}
    data = encoded(manifest)
    require(len(data) <= MAX_MANIFEST_BYTES, "artifact manifest exceeds bound")
    # The output directory itself is the new publication boundary. Never replace
    # an existing artifact or silently combine two unrelated builds.
    output.mkdir(parents=False, exist_ok=False)
    try:
        write_file(output / binary_name, payload, 0o755)
        write_file(output / manifest_name, data, 0o644)
        verify(output, architecture, root)
        descriptor = os.open(output, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
    except BaseException:
        # These are only files created by this call in its new directory.
        for name in (binary_name, manifest_name):
            (output / name).unlink(missing_ok=True)
        output.rmdir()
        raise


def validate_source_entries(entries: object) -> None:
    require(isinstance(entries, list) and 1 <= len(entries) <= 4096, "invalid source inventory")
    source_paths = set()
    source_bytes = 0
    for entry in entries:
        require(set(entry) == {"path", "bytes", "sha256"} and isinstance(entry["path"], str), "invalid source record")
        path = Path(entry["path"])
        require(not path.is_absolute() and ".." not in path.parts and path.as_posix() == entry["path"] and path.parts
                and not any(character in entry["path"] for character in "\0\r\n"), "invalid source path")
        require(entry["path"] not in source_paths, "duplicate source path")
        source_paths.add(entry["path"])
        require(type(entry["bytes"]) is int and 0 <= entry["bytes"] <= MAX_SOURCE_BYTES
                and isinstance(entry["sha256"], str) and re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]) is not None, "invalid source size or hash")
        source_bytes += entry["bytes"]
        require(source_bytes <= MAX_SOURCE_BYTES, "source inventory exceeds byte bound")
    require([entry["path"] for entry in entries] == sorted(source_paths), "source inventory is not sorted")


def verify(output: Path, architecture: str, root: Path | None = None, *, shared_directory: bool = False) -> dict:
    require(output.is_dir() and not output.is_symlink(), "artifact directory is missing or symlinked")
    binary_name, manifest_name = names(architecture)
    if not shared_directory:
        require({entry.name for entry in output.iterdir()} == {binary_name, manifest_name}, "artifact directory has missing or extra entries")
    manifest = json.loads(regular_bytes(output / manifest_name, MAX_MANIFEST_BYTES))
    require(set(manifest) == {"schema_version", "package", "binary", "package_version", "protocol_version", "helper_source", "build_inputs", "source_files", "source_sha256", "build", "payload", "verification"}, "unexpected manifest fields")
    require(manifest["schema_version"] == SCHEMA_VERSION and manifest["package"] == "axocoatl-exec" and manifest["binary"] == "axocoatl-exec-supervisor", "wrong artifact identity")
    require(type(manifest["protocol_version"]) is int and manifest["protocol_version"] > 0, "invalid protocol version")
    require(isinstance(manifest["package_version"], str) and re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.+-]+)?", manifest["package_version"]) is not None, "invalid package version")
    build = manifest["build"]
    require(set(build) == {"toolchain", "rustc", "target", "profile", "locked", "offline", "jobs", "crt_static", "remap_source_prefix", "linker_name", "linker_sha256"}, "unexpected build manifest fields")
    require(build["toolchain"] == TOOLCHAIN and build["target"] == f"{architecture}-unknown-linux-musl"
            and build["profile"] == "release" and build["locked"] is True and build["offline"] is True
            and build["crt_static"] is True and build["jobs"] == 1 and build["remap_source_prefix"] == "/axocoatl-source", "unsupported build contract")
    require(isinstance(build["rustc"], str) and len(build["rustc"]) <= 4096
            and re.search(r"^release: 1\.95\.0$", build["rustc"], re.MULTILINE) is not None, "unsupported compiler provenance")
    require(isinstance(build["linker_name"], str) and 0 < len(build["linker_name"]) <= 256
            and Path(build["linker_name"]).name == build["linker_name"] and re.fullmatch(r"[0-9a-f]{64}", build["linker_sha256"]) is not None,
            "invalid linker provenance")
    entries = manifest["source_files"]
    validate_source_entries(entries)
    require(digest(encoded(entries)) == manifest["source_sha256"], "source inventory digest differs")
    helper = manifest["helper_source"]
    require(set(helper) == {"files", "sha256"}, "unexpected helper source identity")
    validate_source_entries(helper["files"])
    helper_paths = {entry["path"] for entry in helper["files"]}
    require({"build.rs", "src/lib.rs", "src/main.rs", "src/protocol.rs", "src/supervisor.rs"} <= helper_paths
            and all(path == "build.rs" or (path.startswith("src/") and path.endswith(".rs")) for path in helper_paths),
            "helper source identity is incomplete or contains non-Rust inputs")
    require(helper_source_fingerprint(helper["files"], manifest["package_version"]) == helper["sha256"], "helper source digest differs")
    source_entries = {entry["path"]: entry for entry in entries}
    for entry in helper["files"]:
        path = f"crates/axocoatl-exec/{entry['path']}"
        require(source_entries.get(path) == {**entry, "path": path}, "helper identity differs from build source inventory")
    inputs = manifest["build_inputs"]
    require(set(inputs) == {"inputs", "sha256"} and isinstance(inputs["inputs"], dict)
            and inputs["inputs"].get("schema_version") == 1, "invalid relevant build-input identity")
    require(digest(encoded(inputs["inputs"])) == inputs["sha256"], "relevant build-input digest differs")
    payload = regular_bytes(output / binary_name, MAX_BINARY_BYTES)
    require(bool((output / binary_name).stat().st_mode & 0o111), "supervisor payload is not executable")
    expected = {"name": binary_name, "bytes": len(payload), "sha256": digest(payload), "elf": inspect_elf(payload, architecture)}
    require(manifest["payload"] == expected, "payload bytes, ELF properties or digest differ")
    require(manifest["verification"] == {"elf_structure": True, "container_execution": False}, "artifact metadata invents execution proof")
    if root is not None:
        current = source_snapshot(root)
        require(all(manifest[name] == current[name] for name in current), "artifact does not match current source/version/protocol")
    return manifest


def verify_embedded(root: Path) -> None:
    output = root / "crates/axocoatl-isolation/assets/exec-supervisor"
    require(output.is_dir() and not output.is_symlink(), "embedded supervisor directory is missing or symlinked")
    expected = {name for architecture in ARCHITECTURES for name in names(architecture)}
    require({entry.name for entry in output.iterdir()} == expected, "embedded supervisor set has missing or extra entries")
    current = source_snapshot(root)
    isolation_manifest = tomllib.loads(regular_bytes(root / "crates/axocoatl-isolation/Cargo.toml", MAX_SOURCE_BYTES).decode())
    require(isolation_manifest["dependencies"]["axocoatl-exec"]["version"] == f"={current['package_version']}",
            "isolation must pin the exact embedded supervisor package version")
    for architecture in ARCHITECTURES:
        manifest = verify(output, architecture, shared_directory=True)
        require(all(manifest[name] == current[name] for name in ("package_version", "protocol_version", "helper_source", "build_inputs")),
                "embedded artifact does not match current helper source/version/protocol/build inputs")


def unpack_crate(archive: Path, output: Path, expected_package: str) -> dict:
    """Read Cargo's archive without trusting paths, links, or archive sizes."""
    total = 0
    seen = set()
    prefix = None
    with tarfile.open(archive, "r:gz") as source:
        for member in source:
            path = Path(member.name)
            require(not path.is_absolute() and ".." not in path.parts and len(path.parts) >= 2
                    and path.as_posix() == member.name, "unsafe Cargo archive path")
            require(prefix is None or prefix == path.parts[0], "Cargo archive has multiple package roots")
            prefix = path.parts[0]
            relative = Path(*path.parts[1:])
            require(relative not in seen and len(seen) < 4096, "duplicate or excessive Cargo archive entries")
            seen.add(relative)
            require(member.isfile() and 0 <= member.size <= MAX_BINARY_BYTES, "Cargo archive entry must be a bounded regular file")
            total += member.size
            require(total <= 256 * 1024 * 1024, "Cargo archive exceeds byte bound")
            data_source = source.extractfile(member)
            require(data_source is not None, "Cargo archive file is unreadable")
            with data_source:
                data = data_source.read(MAX_BINARY_BYTES + 1)
            require(len(data) == member.size, "Cargo archive file size differs")
            destination = output / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            write_file(destination, data, 0o755 if member.mode & 0o111 else 0o644)
    package = tomllib.loads(regular_bytes(output / "Cargo.toml", MAX_SOURCE_BYTES).decode())["package"]
    require(package["name"] == expected_package and isinstance(package["version"], str), "wrong Cargo package identity")
    require(prefix == f"{expected_package}-{package['version']}", "Cargo package root differs from manifest")
    return package


def verify_packages(exec_archive: Path, isolation_archive: Path, root: Path) -> None:
    # Do not compare Cargo.toml or Cargo.lock with workspace bytes: Cargo's
    # normalization intentionally rewrites them. Verify shipped source identity
    # against shipped payloads, then compare payload bytes with the gated checkout.
    verify_embedded(root)
    with tempfile.TemporaryDirectory(prefix="axocoatl-exec-packages-") as directory:
        temporary = Path(directory)
        exec_package = temporary / "exec"
        isolation_package = temporary / "isolation"
        exec_package.mkdir()
        isolation_package.mkdir()
        identity = unpack_crate(exec_archive, exec_package, "axocoatl-exec")
        unpack_crate(isolation_archive, isolation_package, "axocoatl-isolation")
        original_manifest = regular_bytes(exec_package / "Cargo.toml.orig", MAX_SOURCE_BYTES)
        require(original_manifest == regular_bytes(root / "crates/axocoatl-exec/Cargo.toml", MAX_SOURCE_BYTES),
                "packaged original supervisor manifest differs from reviewed source")
        normalized = tomllib.loads(regular_bytes(exec_package / "Cargo.toml", MAX_SOURCE_BYTES).decode())
        workspace = tomllib.loads(regular_bytes(root / "Cargo.toml", MAX_SOURCE_BYTES).decode())
        require(resolved_dependency_groups(normalized, {}) == resolved_dependency_groups(tomllib.loads(original_manifest.decode()), workspace),
                "normalized supervisor dependencies differ from reviewed source")
        normalized_isolation = tomllib.loads(regular_bytes(isolation_package / "Cargo.toml", MAX_SOURCE_BYTES).decode())
        require(normalized_isolation["dependencies"]["axocoatl-exec"]["version"] == f"={identity['version']}",
                "normalized isolation package does not pin its exact supervisor version")
        packaged_source = helper_source(exec_package, identity["version"])
        protocol = regular_bytes(exec_package / "src/protocol.rs", MAX_SOURCE_BYTES).decode()
        versions = re.findall(r"^pub const PROTOCOL_VERSION: u32 = ([0-9]+);$", protocol, re.MULTILINE)
        require(len(versions) == 1, "packaged protocol version is missing or ambiguous")
        payloads = isolation_package / "assets/exec-supervisor"
        expected = {name for architecture in ARCHITECTURES for name in names(architecture)}
        require({entry.name for entry in payloads.iterdir()} == expected, "Cargo package omitted an embedded supervisor asset")
        for architecture in ARCHITECTURES:
            manifest = verify(payloads, architecture, shared_directory=True)
            require(manifest["helper_source"] == packaged_source and manifest["package_version"] == identity["version"]
                    and manifest["protocol_version"] == int(versions[0]), "packaged helper source/version/protocol differs from embedded payload")
            for name in names(architecture):
                require(regular_bytes(payloads / name, MAX_BINARY_BYTES) == regular_bytes(
                    root / "crates/axocoatl-isolation/assets/exec-supervisor" / name, MAX_BINARY_BYTES),
                    "packaged embedded payload or manifest differs from gated checkout")


def fixture_elf(architecture: str, *, interpreter: bool = False, needed: bool = False, pie: bool = False) -> bytes:
    """Structural ELF fixture, deliberately never executed or called a helper."""
    size = 512
    data = bytearray(size)
    ident = b"\x7fELF\x02\x01\x01" + bytes(9)
    extra = interpreter or needed or pie
    struct.pack_into("<16sHHIQQQIHHHHHH", data, 0, ident, 3 if pie else 2, ARCHITECTURES[architecture], 1,
                     0x400100, 64, 0, 0, 64, 56, 2 if extra else 1, 0, 0, 0)
    struct.pack_into("<IIQQQQQQ", data, 64, 1, 5, 0, 0x400000, 0, size, size, 4096)
    if interpreter:
        struct.pack_into("<IIQQQQQQ", data, 120, 3, 4, 320, 0x400140, 0, 8, 8, 1)
    elif extra:
        struct.pack_into("<IIQQQQQQ", data, 120, 2, 4, 320, 0x400140, 0, 32, 32, 8)
        struct.pack_into("<qQqQ", data, 320, 1 if needed else 0, 1, 0, 0)
    return bytes(data)


class ArtifactTests(unittest.TestCase):
    def write_fixture_artifact(self, output: Path) -> dict:
        binary_name, manifest_name = names("x86_64")
        payload = fixture_elf("x86_64")
        helper_entries = [{"path": path, "bytes": 0, "sha256": digest(b"")} for path in (
            "build.rs", "src/lib.rs", "src/main.rs", "src/protocol.rs", "src/supervisor.rs")]
        entries = [{**entry, "path": f"crates/axocoatl-exec/{entry['path']}"} for entry in helper_entries]
        manifest = {"schema_version": SCHEMA_VERSION, "package": "axocoatl-exec", "binary": "axocoatl-exec-supervisor",
                    "package_version": "0.0.0", "protocol_version": 1, "source_files": entries, "source_sha256": digest(encoded(entries)),
                    "helper_source": {"files": helper_entries, "sha256": helper_source_fingerprint(helper_entries, "0.0.0")},
                    "build_inputs": {"inputs": {"schema_version": 1}, "sha256": digest(encoded({"schema_version": 1}))},
                    "build": {"toolchain": TOOLCHAIN, "rustc": "release: 1.95.0\n", "target": "x86_64-unknown-linux-musl",
                              "profile": "release", "locked": True, "offline": True, "crt_static": True, "jobs": 1,
                              "remap_source_prefix": "/axocoatl-source", "linker_name": "fixture-not-a-linker", "linker_sha256": digest(b"")},
                    "payload": {"name": binary_name, "bytes": len(payload), "sha256": digest(payload), "elf": inspect_elf(payload, "x86_64")},
                    "verification": {"elf_structure": True, "container_execution": False}}
        write_file(output / binary_name, payload, 0o755)
        write_file(output / manifest_name, encoded(manifest), 0o644)
        return manifest

    def test_structural_static_and_static_pie_architectures(self):
        for architecture in ARCHITECTURES:
            for pie in (False, True):
                self.assertEqual(inspect_elf(fixture_elf(architecture, pie=pie), architecture)["architecture"], architecture)

    def test_wrong_architecture_interpreter_and_needed_are_refused(self):
        for data in (fixture_elf("aarch64"), fixture_elf("x86_64", interpreter=True), fixture_elf("x86_64", needed=True)):
            with self.assertRaises(ArtifactError):
                inspect_elf(data, "x86_64")

    def test_truncated_or_unbounded_program_headers_and_bad_entry_are_refused(self):
        variants = [fixture_elf("x86_64")[:80]]
        for offset, fmt, value in ((56, "H", 257), (32, "Q", 500), (24, "Q", 0), (96, "Q", 513), (104, "Q", 1)):
            data = bytearray(fixture_elf("x86_64"))
            struct.pack_into("<" + fmt, data, offset, value)
            variants.append(data)
        for data in variants:
            with self.assertRaises(ArtifactError):
                inspect_elf(data, "x86_64")

    def test_manifest_payload_tamper_wrong_arch_and_symlink_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            binary_name, manifest_name = names("x86_64")
            payload = fixture_elf("x86_64")
            self.write_fixture_artifact(output)
            # This verifies structural consistency only; fixture metadata itself
            # explicitly says no helper execution was proved.
            self.assertFalse(verify(output, "x86_64")["verification"]["container_execution"])
            for changed in (payload[:-1] + b"X", fixture_elf("aarch64")):
                (output / binary_name).write_bytes(changed)
                with self.assertRaises(ArtifactError):
                    verify(output, "x86_64")
            (output / binary_name).unlink()
            (output / binary_name).symlink_to("missing")
            with self.assertRaises(OSError):
                verify(output, "x86_64")

    def test_duplicate_and_escaping_source_paths_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            manifest = self.write_fixture_artifact(output)
            original = manifest["source_files"]
            variants = [original + original, [{"path": "../outside", "bytes": 0, "sha256": digest(b"")}],
                        [{"path": "/absolute", "bytes": 0, "sha256": digest(b"")}]]
            for entries in variants:
                manifest["source_files"] = entries
                manifest["source_sha256"] = digest(encoded(entries))
                (output / names("x86_64")[1]).write_bytes(encoded(manifest))
                with self.assertRaises(ArtifactError):
                    verify(output, "x86_64")

    def test_tampered_manifest_and_invented_execution_proof_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            manifest = self.write_fixture_artifact(output)
            original = encoded(manifest)
            for section, key, value in [("payload", "sha256", "0" * 64), ("verification", "container_execution", True),
                                        ("build", "target", "aarch64-unknown-linux-musl"), ("build", "locked", False)]:
                altered = json.loads(original)
                altered[section][key] = value
                (output / names("x86_64")[1]).write_bytes(encoded(altered))
                with self.assertRaises(ArtifactError):
                    verify(output, "x86_64")

    def test_helper_fingerprint_survives_manifest_normalization_but_detects_source_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory)
            (crate / "src").mkdir()
            (crate / "build.rs").write_text("// fingerprint algorithm\n")
            (crate / "src/lib.rs").write_text("pub mod protocol;\n")
            (crate / "src/protocol.rs").write_text("pub const PROTOCOL_VERSION: u32 = 1;\n")
            (crate / "Cargo.toml").write_text('[package]\nversion.workspace = true\n')
            before = helper_source(crate, "1.0.0")
            (crate / "Cargo.toml").write_text('# normalized by Cargo\n[package]\nversion = "1.0.0"\n')
            (crate / "Cargo.lock").write_text('# generated package lockfile\n')
            self.assertEqual(before, helper_source(crate, "1.0.0"))
            self.assertNotEqual(before, helper_source(crate, "1.0.1"))
            for relative in ("build.rs", "src/protocol.rs", "src/new.rs"):
                path = crate / relative
                old = path.read_bytes() if path.exists() else None
                path.write_bytes(b"// changed source\n")
                self.assertNotEqual(before, helper_source(crate, "1.0.0"))
                if old is None:
                    path.unlink()
                else:
                    path.write_bytes(old)

    def test_stale_helper_identity_cannot_be_detached_from_build_provenance(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            manifest = self.write_fixture_artifact(output)
            manifest["helper_source"]["files"][0]["sha256"] = digest(b"different source")
            manifest["helper_source"]["sha256"] = helper_source_fingerprint(
                manifest["helper_source"]["files"], manifest["package_version"])
            (output / names("x86_64")[1]).write_bytes(encoded(manifest))
            with self.assertRaisesRegex(ArtifactError, "differs from build source inventory"):
                verify(output, "x86_64")

    def test_normalized_package_archives_keep_exact_source_and_both_payloads(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            crate = root / "crates/axocoatl-exec"
            isolation = root / "crates/axocoatl-isolation"
            (crate / "src").mkdir(parents=True)
            (root / "scripts").mkdir()
            (root / "Cargo.toml").write_text('[workspace.package]\nversion = "1.0.0"\n')
            locked = ('version = 4\n[[package]]\nname = "axocoatl-exec"\nversion = "1.0.0"\n'
                      'dependencies = ["sha2"]\n[[package]]\nname = "sha2"\nversion = "0.10.9"\n'
                      'source = "registry+https://example.invalid/index"\nchecksum = "fixture-checksum"\n'
                      '[[package]]\nname = "axocoatl-cli"\nversion = "1.0.1"\n')
            (root / "Cargo.lock").write_text(locked)
            (crate / "Cargo.toml").write_text('[package]\nname = "axocoatl-exec"\nversion.workspace = true\n')
            for path in ("build.rs", "src/lib.rs", "src/main.rs", "src/supervisor.rs"):
                (crate / path).write_text("// fixture source, never compiled\n")
            (crate / "src/protocol.rs").write_text("pub const PROTOCOL_VERSION: u32 = 1;\n")
            for path in ("build-exec-supervisor.sh", "test-exec-supervisor-artifact.py"):
                (root / "scripts" / path).write_text("# fixture tooling, never executed\n")
            snapshot = source_snapshot(root)
            payloads = isolation / "assets/exec-supervisor"
            payloads.mkdir(parents=True)
            (isolation / "Cargo.toml").write_text('[package]\nname = "axocoatl-isolation"\nversion = "1.0.0"\n'
                                                  '[dependencies.axocoatl-exec]\nversion = "=1.0.0"\n')
            for architecture in ARCHITECTURES:
                staging = root / architecture
                staging.mkdir()
                manifest = self.write_fixture_artifact(staging)
                manifest.update(snapshot)
                manifest["build"]["target"] = f"{architecture}-unknown-linux-musl"
                payload = fixture_elf(architecture)
                binary, record = names(architecture)
                manifest["payload"] = {"name": binary, "bytes": len(payload), "sha256": digest(payload),
                                       "elf": inspect_elf(payload, architecture)}
                write_file(payloads / binary, payload, 0o755)
                write_file(payloads / record, encoded(manifest), 0o644)
            verify_embedded(root)
            (root / "Cargo.lock").write_text(locked.replace('version = "1.0.1"', 'version = "1.0.2"'))
            verify_embedded(root)  # A CLI-only version bump does not rebuild a Linux helper.
            (root / "Cargo.lock").write_text(locked.replace('version = "0.10.9"', 'version = "0.10.10"'))
            with self.assertRaisesRegex(ArtifactError, "current helper source/version/protocol/build inputs"):
                verify_embedded(root)
            (root / "Cargo.lock").write_text(locked)

            def archive(source: Path, package_name: str, *, omit: str | None = None, changed_protocol: bool = False) -> Path:
                import io
                output = root / f"{package_name}-1.0.0.crate"
                with tarfile.open(output, "w:gz") as result:
                    for path in sorted(source.rglob("*")):
                        if not path.is_file():
                            continue
                        relative = path.relative_to(source).as_posix()
                        if relative == omit:
                            continue
                        data = path.read_bytes()
                        if relative == "Cargo.toml":
                            original = tarfile.TarInfo(f"{package_name}-1.0.0/Cargo.toml.orig")
                            original.size = len(data)
                            result.addfile(original, io.BytesIO(data))
                            data = f'# Cargo normalized this manifest\n[package]\nname = "{package_name}"\nversion = "1.0.0"\n'.encode()
                            if package_name == "axocoatl-isolation":
                                data += b'[dependencies.axocoatl-exec]\nversion = "=1.0.0"\n'
                        if changed_protocol and relative == "src/protocol.rs":
                            data = b"pub const PROTOCOL_VERSION: u32 = 2;\n"
                        item = tarfile.TarInfo(f"{package_name}-1.0.0/{relative}")
                        item.size = len(data)
                        item.mode = path.stat().st_mode & 0o777
                        result.addfile(item, io.BytesIO(data))
                return output

            exec_archive = archive(crate, "axocoatl-exec")
            isolation_archive = archive(isolation, "axocoatl-isolation")
            verify_packages(exec_archive, isolation_archive, root)
            archive(crate, "axocoatl-exec", changed_protocol=True)
            with self.assertRaisesRegex(ArtifactError, "packaged helper source/version/protocol differs"):
                verify_packages(exec_archive, isolation_archive, root)
            archive(crate, "axocoatl-exec")
            archive(isolation, "axocoatl-isolation", omit=f"assets/exec-supervisor/{names('aarch64')[0]}")
            with self.assertRaisesRegex(ArtifactError, "omitted an embedded supervisor asset"):
                verify_packages(exec_archive, isolation_archive, root)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    snapshot = commands.add_parser("snapshot")
    snapshot.add_argument("root", type=Path)
    snapshot.add_argument("output", type=Path)
    package_command = commands.add_parser("package", help="internal build-wrapper step; not an execution attestation")
    for name in ("root", "snapshot", "binary"):
        package_command.add_argument(name, type=Path)
    package_command.add_argument("architecture", choices=ARCHITECTURES)
    for name in ("output", "rustc", "linker"):
        package_command.add_argument(name, type=Path)
    verification = commands.add_parser("verify")
    verification.add_argument("output", type=Path)
    verification.add_argument("architecture", choices=ARCHITECTURES)
    verification.add_argument("--source-root", type=Path)
    embedded = commands.add_parser("verify-embedded", help="check both embedded architectures against the current locked workspace")
    embedded.add_argument("root", type=Path)
    packages = commands.add_parser("verify-packages", help="check normalized Cargo archives retain matching helper source and payloads")
    packages.add_argument("exec_archive", type=Path)
    packages.add_argument("isolation_archive", type=Path)
    packages.add_argument("--source-root", required=True, type=Path)
    commands.add_parser("self-test")
    args = parser.parse_args()
    try:
        if args.command == "snapshot":
            write_file(args.output, encoded(source_snapshot(args.root)), 0o600)
        elif args.command == "package":
            package(args.root, args.snapshot, args.binary, args.architecture, args.output, args.rustc, args.linker)
        elif args.command == "verify":
            manifest = verify(args.output, args.architecture, args.source_root)
            print(f"Verified ELF structure and hashes: {manifest['payload']['name']} (protocol {manifest['protocol_version']}; execution not tested)")
        elif args.command == "verify-embedded":
            verify_embedded(args.root)
            print("Embedded supervisor source, protocol, version and artifact hashes: PASS (execution not tested)")
        elif args.command == "verify-packages":
            verify_packages(args.exec_archive, args.isolation_archive, args.source_root)
            print("Normalized Cargo packages retain exact supervisor source and payloads: PASS (execution not tested)")
        else:
            suite = unittest.defaultTestLoader.loadTestsFromTestCase(ArtifactTests)
            result = unittest.TextTestRunner(verbosity=2).run(suite)
            raise SystemExit(0 if result.wasSuccessful() else 1)
    except (ArtifactError, OSError, KeyError, TypeError, json.JSONDecodeError, tomllib.TOMLDecodeError, struct.error, tarfile.TarError) as problem:
        raise SystemExit(f"exec-supervisor-artifact: {problem}") from problem


if __name__ == "__main__":
    main()
