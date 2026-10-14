#!/usr/bin/env python3
"""Collect release dependency notices from the locked, offline Cargo graph.

Uses only Python's standard library, Cargo, rustc, and files already installed
by Cargo/rustup. Run with the same toolchain, manifest, target, and default
features used for the release build. Build dependencies are included
conservatively; dev-only and other-target dependency edges are excluded.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys


def command(args: list[str], cwd: Path) -> str:
    result = subprocess.run(args, cwd=cwd, capture_output=True, check=False)
    if result.returncode:
        raise RuntimeError(
            f"{args[0]} failed ({result.returncode}): "
            + result.stderr.decode("utf-8", errors="replace").strip()
        )
    return result.stdout.decode("utf-8").strip()


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def release_packages(metadata: dict) -> list[dict]:
    resolve = metadata.get("resolve")
    if not resolve or not resolve.get("root"):
        raise RuntimeError("Expected a single root Cargo package")
    nodes = {node["id"]: node for node in resolve["nodes"]}
    packages = {package["id"]: package for package in metadata["packages"]}
    visited: set[str] = set()
    pending = [resolve["root"]]
    while pending:
        package_id = pending.pop()
        if package_id in visited:
            continue
        visited.add(package_id)
        for dependency in nodes[package_id]["deps"]:
            if any(kind["kind"] != "dev" for kind in dependency["dep_kinds"]):
                pending.append(dependency["pkg"])
    visited.remove(resolve["root"])
    result = []
    for package_id in visited:
        package = dict(packages[package_id])
        package["resolved_features"] = sorted(nodes[package_id]["features"])
        result.append(package)
    return sorted(result, key=lambda package: (package["name"], package["version"]))


def is_notice(path: Path) -> bool:
    name = path.name.lower()
    return name.startswith(("license", "licence", "copying", "notice", "unlicense")) or name in {
        "copyright", "copyright.txt", "copyright.md"
    }


def package_notices(package: dict) -> list[tuple[str, bytes]]:
    root = Path(package["manifest_path"]).parent.resolve()
    paths = {path for path in root.rglob("*") if path.is_file() and is_notice(path)}
    declared_file = package.get("license_file")
    if declared_file:
        declared_path = (root / declared_file).resolve()
        if not declared_path.is_relative_to(root) or not declared_path.is_file():
            raise RuntimeError(f"Missing or out-of-package license file: {package['name']}")
        paths.add(declared_path)
    # SQLCipher sources ship in this crate, but are not built by CSM.
    if package["name"] == "libsqlite3-sys":
        if any("sqlcipher" in feature for feature in package["resolved_features"]):
            raise RuntimeError("SQLCipher enabled: review its additional bundled notices")
        paths = {path for path in paths if "sqlcipher" not in path.relative_to(root).parts}
    if not paths or not any(
        path.name.lower().startswith(("license", "licence", "copying", "unlicense"))
        or (declared_file and path == (root / declared_file).resolve())
        for path in paths
    ):
        raise RuntimeError(f"No license text found for {package['name']} {package['version']}")
    result = []
    for path in sorted(paths, key=lambda path: path.relative_to(root).as_posix()):
        data = path.read_bytes()
        if not data.strip():
            raise RuntimeError(f"Empty notice for {package['name']}: {path.name}")
        data.decode("utf-8")  # Fail rather than silently alter an unknown encoding.
        result.append((path.relative_to(root).as_posix(), data))
    if package["name"] == "libsqlite3-sys":
        if "bundled" not in package["resolved_features"]:
            raise RuntimeError("Expected bundled SQLite; review external-library notices")
        header = (root / "sqlite3" / "sqlite3.h").read_bytes()
        end = header.find(b"*/")
        if not header.startswith(b"/*") or end < 0:
            raise RuntimeError("SQLite header notice could not be located")
        notice = header[:end + 2]
        if b"The author disclaims copyright to this source code." not in notice:
            raise RuntimeError("SQLite copyright disclaimer changed; review it")
        result.append(("sqlite3/sqlite3.h (opening copyright notice)", notice))
    return result


def collect(manifest: Path, target: str, output: Path) -> int:
    manifest = manifest.resolve(strict=True)
    cwd = manifest.parent
    metadata = json.loads(command([
        "cargo", "metadata", "--offline", "--locked", "--format-version", "1",
        "--filter-platform", target, "--manifest-path", str(manifest),
    ], cwd))
    packages = release_packages(metadata)
    root_id = metadata["resolve"]["root"]
    root_package = next(package for package in metadata["packages"] if package["id"] == root_id)
    rust_version = command(["rustc", "--version"], cwd)
    sysroot = Path(command(["rustc", "--print", "sysroot"], cwd))
    rust_notices = (sysroot / "share" / "doc" / "rust" / "COPYRIGHT-library.html").read_bytes()
    if b"Copyright notices for The Rust Standard Library" not in rust_notices:
        raise RuntimeError("Rust standard-library notices are missing or unrecognized")
    parts = [(
        f"CSM LOCAL {root_package['version']} - THIRD-PARTY NOTICES\n"
        f"Target: {target}\nToolchain: {rust_version}\n\n"
        "Collected from the locked Cargo normal/build dependency graph with default\n"
        "features. Dev-only and other-target edges are excluded. Build dependencies\n"
        "are included conservatively. License expressions are upstream declarations;\n"
        "the original notice texts below are reproduced without alteration.\n"
        "Nested notices (including Unicode data) are retained. SQLCipher is not built.\n"
        "Bundled SQLite's opening copyright disclaimer is included separately.\n"
        "Rust standard-library notices are in RUST-STANDARD-LIBRARY-NOTICES.html.\n"
        "CSM's own license is provided separately in LICENSE.\n\n"
    ).encode("utf-8")]
    entries = []
    for package in packages:
        if not package.get("license") and not package.get("license_file"):
            raise RuntimeError(f"No declared license for {package['name']} {package['version']}")
        files = package_notices(package)
        parts.append((
            "=" * 78 + "\n"
            f"{package['name']} {package['version']}\n"
            f"Declared license: {package.get('license') or 'See upstream license file'}\n\n"
        ).encode("utf-8"))
        records = []
        for name, data in files:
            parts.extend([f"--- {name} ---\n".encode("utf-8"), data, b"\n\n"])
            records.append({"file": name, "sha256": sha256(data)})
        entry = {
            "name": package["name"], "version": package["version"],
            "license": package.get("license"),
            "features": package["resolved_features"], "notices": records,
        }
        if package["name"] == "libsqlite3-sys":
            header = (Path(package["manifest_path"]).parent / "sqlite3" / "sqlite3.h").read_text("utf-8")
            match = re.search(r'^#define SQLITE_VERSION\s+"([^"]+)"', header, re.MULTILINE)
            if not match:
                raise RuntimeError("Cannot determine bundled SQLite version")
            entry["bundled_sqlite_version"] = match.group(1)
        entries.append(entry)
    combined = b"".join(parts)
    dependency_manifest = {
        "package": root_package["name"], "version": root_package["version"],
        "target": target, "toolchain": rust_version,
        "scope": "default features; normal/build edges; includes build dependencies",
        "notices_sha256": sha256(combined),
        "rust_standard_library_notices_sha256": sha256(rust_notices),
        "dependencies": entries,
    }
    # Finish validation before creating any output; never leave partial success.
    output.mkdir(parents=True, exist_ok=True)
    (output / "THIRD-PARTY-NOTICES.txt").write_bytes(combined)
    (output / "RUST-STANDARD-LIBRARY-NOTICES.html").write_bytes(rust_notices)
    (output / "dependency-manifest.json").write_text(
        json.dumps(dependency_manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(f"Collected notices for {len(packages)} crates, bundled SQLite, and Rust's standard library.")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest-path", type=Path, default=Path(__file__).resolve().parents[1] / "Cargo.toml")
    parser.add_argument("--target", default="x86_64-pc-windows-msvc")
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    output = args.output_dir or args.manifest_path.resolve().parent / "target" / "notices"
    try:
        return collect(args.manifest_path, args.target, output)
    except (OSError, ValueError, RuntimeError, KeyError) as error:
        print(f"Notice collection failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
