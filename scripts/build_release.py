"""Build the Windows x64 runtime with the MSVC runtime linked statically."""
import os
from pathlib import Path
import subprocess

from pe_imports import inspect_pe, require_standalone

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-pc-windows-msvc"

if __name__ == "__main__":
    env = os.environ.copy()
    # Use an explicit target so target flags do not leak into host/proc macros.
    env.pop("RUSTFLAGS", None)
    env.pop("CARGO_ENCODED_RUSTFLAGS", None)
    env["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS"] = "-C target-feature=+crt-static"
    subprocess.run(["cargo", "build", "--release", "--locked", "--target", TARGET,
                    "--target-dir", str(ROOT / "target")], cwd=ROOT, env=env, check=True)
    binary = ROOT / "target" / TARGET / "release/csm.exe"
    require_standalone(inspect_pe(binary))
    subprocess.run([str(binary), "--version"], check=True)
    print(binary)
