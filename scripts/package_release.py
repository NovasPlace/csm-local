"""Build local review artifacts from an already-tested Windows release binary.

Does not publish, sign, install, access accounts, or copy user configuration/data.
Uses an explicit file allowlist; target/, .git/, caches and local databases never
enter the archives. Run with the same Rust toolchain used for the release build.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import zipfile

from pe_imports import inspect_pe, require_standalone

ROOT = Path(__file__).resolve().parents[1]
TEXT_EXTENSIONS = {".md", ".txt", ".json", ".toml", ".yaml", ".yml", ".py", ".rs"}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_bytes(path):
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"Expected a regular file: {path.name}")
    return path.read_bytes()


def checksums(files):
    return "".join(f"{digest(data)}  {name}\n" for name, data in sorted(files.items())).encode()


def archive(destination, root_name, files):
    # Stable ordering, timestamp, and permissions make identical inputs reproducible.
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for name, data in sorted(files.items()):
            parts = Path(name).parts
            if Path(name).is_absolute() or ".." in parts or "\\" in name:
                raise ValueError(f"Unsafe archive path: {name}")
            info = zipfile.ZipInfo(f"{root_name}/{name}", (2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = 0o100644 << 16
            z.writestr(info, data)
    with zipfile.ZipFile(destination) as z:
        if z.testzip() is not None:
            raise ValueError("Archive checksum validation failed")
        assert len(z.namelist()) == len(files)


def text_tree(directory):
    files = {}
    for path in sorted(directory.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"Symlink is not allowed: {path.name}")
        if not path.is_file():
            continue
        relative = path.relative_to(directory).as_posix()
        if any(part.startswith(".") or part == "__pycache__" for part in path.relative_to(directory).parts):
            raise ValueError(f"Hidden/cache file in package source: {relative}")
        if path.suffix.lower() not in TEXT_EXTENSIONS:
            raise ValueError(f"Unexpected skill file type: {relative}")
        data = file_bytes(path)
        data.decode("utf-8")
        if b"\0" in data:
            raise ValueError(f"Binary content in skill file: {relative}")
        files[relative] = data
    if "SKILL.md" not in files:
        raise ValueError("Missing skill entrypoint")
    return files


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True, help="Directory for review ZIPs and manifests")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/x86_64-pc-windows-msvc/release/csm.exe")
    args = parser.parse_args()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    runtime_name = f"csm-local-{version}-windows-x64"
    binary = file_bytes(args.binary)
    imports = inspect_pe(args.binary)
    if imports["architecture"] != "x64" or imports["format"] != "PE32+":
        raise ValueError("Runtime is not Windows x64")
    require_standalone(imports)
    actual = subprocess.check_output([str(args.binary.resolve()), "--version"], text=True).strip()
    if actual != f"csm {version}":
        raise ValueError(f"Release source and executable version differ: {actual}")

    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    names = [f"{runtime_name}.zip", f"csm-project-memory-{version}-agensi.zip", f"csm-local-{version}-source.zip"]
    if any((output / name).exists() for name in names):
        raise ValueError("Release ZIPs already exist; use a new output directory to preserve the previous build")

    runtime = {name: file_bytes(ROOT / name) for name in (
        "README.md", "LICENSE", "docs/PRIVACY.md", "docs/RECOVERY.md", "docs/RELEASE-NOTES.md",
        "docs/LIVE-MESSAGING.md", "docs/IDLE-WAKE.md", "docs/VERIFICATION.md"
    )}
    runtime["csm.exe"] = binary
    with tempfile.TemporaryDirectory(prefix="csm-notices-") as staging:
        subprocess.run([sys.executable, str(ROOT / "scripts/collect_notices.py"), "--output-dir", staging],
                       cwd=ROOT, check=True)
        for path in sorted(Path(staging).iterdir()):
            if path.is_file():
                runtime[f"notices/{path.name}"] = file_bytes(path)
    runtime["SHA256SUMS.txt"] = checksums(runtime)
    archive(output / names[0], runtime_name, runtime)

    skill = text_tree(ROOT / "packaging/agensi/csm-project-memory")
    skill["LICENSE.txt"] = file_bytes(ROOT / "LICENSE")
    archive(output / names[1], "csm-project-memory", skill)

    # Source companion supports auditability and rebuilding without shipping .git.
    source = {}
    for name in ["Cargo.toml", "Cargo.lock", "rustfmt.toml", "README.md", "LICENSE", ".gitignore"]:
        source[name] = file_bytes(ROOT / name)
    for directory in ["src", "tests", "scripts", "docs", "packaging"]:
        for path in sorted((ROOT / directory).rglob("*")):
            if not path.is_file() or "__pycache__" in path.parts:
                continue
            if path.suffix.lower() not in TEXT_EXTENSIONS:
                raise ValueError(f"Unexpected source file type: {path.name}")
            data = file_bytes(path)
            data.decode("utf-8")
            source[path.relative_to(ROOT).as_posix()] = data
    archive(output / names[2], f"csm-local-{version}-source", source)

    (output / "AGENSI-LISTING-DRAFT.md").write_bytes(file_bytes(ROOT / "packaging/AGENSI-LISTING.md"))
    manifest = {
        "version": version, "platform": "windows-x64", "runtime_sha256": digest(binary),
        "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "runtime_signed": False, "marketplace_submitted": False,
        "marketplace_runtime_delivery_confirmed": False,
        "repository_url": "https://github.com/NovasPlace/csm-local",
        "runtime_download_url": f"https://github.com/NovasPlace/csm-local/releases/download/v{version}/{runtime_name}.zip",
        "pe_dependencies": imports,
        "archives": [{"file": name, "sha256": digest((output / name).read_bytes()),
                      "bytes": (output / name).stat().st_size} for name in names],
        "skill_files": sorted(skill),
        "toolchain": subprocess.check_output(["rustc", "--version"], text=True).strip(),
    }
    (output / "release-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    (output / "SHA256SUMS.txt").write_text("".join(f"{row['sha256']}  {row['file']}\n" for row in manifest["archives"]), encoding="utf-8")
    print(json.dumps(manifest, indent=2))


if __name__ == "__main__":
    main()
