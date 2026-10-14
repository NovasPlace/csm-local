"""Verify packaged Windows runtime and text-only skill using isolated local data.

Run with Python 3.11+ and -B. No installed host, account, or model is contacted.
The report contains archive hashes and named assertions, never temporary paths,
configuration contents, or captured process output.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import re
import stat
import subprocess
import tempfile
import tomllib
from urllib.parse import unquote, urlsplit
import zipfile


class VerificationFailure(Exception):
    pass


class Verifier:
    def __init__(self):
        self.report = {"result": "running", "archives": [], "assertions": []}
        self.current = "initialization"

    def check(self, name, condition):
        self.current = name
        self.report["assertions"].append({"name": name, "passed": bool(condition)})
        if not condition:
            raise VerificationFailure(name)

    def command(self, name, args, cwd, env, input_bytes=b""):
        self.current = name
        result = subprocess.run(args, cwd=cwd, env=env, input=input_bytes,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                timeout=45, check=False)
        self.check(name, result.returncode == 0)
        return result.stdout.decode("utf-8-sig", errors="strict")


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def safe_name(name):
    parts = name.rstrip("/").split("/")
    reserved = {"CON", "PRN", "AUX", "NUL", *(f"COM{i}" for i in range(1, 10)),
                *(f"LPT{i}" for i in range(1, 10))}
    return (bool(parts) and not name.startswith("/") and "\\" not in name
            and all(part not in {"", ".", ".."} and ":" not in part
                    and not part.endswith((" ", "."))
                    and not any(ord(c) < 32 for c in part)
                    and part.split(".", 1)[0].upper() not in reserved for part in parts))


def archive_files(path, verify, label):
    verify.current = f"{label}_archive_readable"
    with zipfile.ZipFile(path) as archive:
        entries = archive.infolist()
        verify.check(f"{label}_archive_size_bounded", len(entries) <= 5000
                     and sum(item.file_size for item in entries) <= 256 * 1024 * 1024)
        seen = set()
        files = {}
        for item in entries:
            key = item.filename.rstrip("/").casefold()
            allowed = (safe_name(item.filename) and key not in seen
                       and not stat.S_ISLNK(item.external_attr >> 16)
                       and not item.flag_bits & 1)
            if not allowed:
                verify.check(f"{label}_archive_safe_paths", False)
            seen.add(key)
            if not item.is_dir():
                files[item.filename] = archive.read(item)
        verify.check(f"{label}_archive_safe_paths", True)
        return files


def parse_checksums(data):
    result = {}
    for line in data.decode("utf-8-sig").splitlines():
        if not line.strip():
            continue
        match = re.fullmatch(r"([a-fA-F0-9]{64})  (.+)", line)
        if not match or not safe_name(match[2]) or match[2] in result:
            raise VerificationFailure("checksum_manifest_format")
        result[match[2]] = match[1].lower()
    return result


def verify_skill(files, verify):
    verify.check("skill_single_expected_root", {name.split("/", 1)[0] for name in files} == {"csm-project-memory"})
    verify.check("skill_entrypoint_present", "csm-project-memory/SKILL.md" in files)
    texts = {}
    for name, data in files.items():
        verify.current = "skill_text_only"
        text = data.decode("utf-8-sig")
        verify.check("skill_text_only", PurePosixPath(name).suffix.lower()
                     in {".md", ".txt", ".json", ".toml", ".yaml", ".yml"}
                     and b"\0" not in data and not data.startswith((b"MZ", b"\x7fELF")))
        texts[name] = text
    combined = "\n".join(texts.values())
    personal_path = re.compile(r"(?:[A-Za-z]:[/\\]+Users[/\\]+[^\s/\\<>]+|/(?:Users|home)/[^\s/<>]+)")
    verify.check("skill_no_literal_user_profile_paths", not personal_path.search(combined))
    secrets = re.compile(
        r"(?:sk-(?:proj-|ant-)?[A-Za-z0-9_-]{20,}|gh[pousr]_[A-Za-z0-9]{20,}"
        r"|-----BEGIN (?:[A-Z ]+ )?PRIVATE KEY-----|Bearer\s+[A-Za-z0-9_.-]{24,}"
        r"|https?://[^\s/:@]+:[^\s/@]+@)", re.IGNORECASE)
    verify.check("skill_no_obvious_credentials", not secrets.search(combined))
    link_count = 0
    for name, text in texts.items():
        if not name.endswith(".md"):
            continue
        links = re.findall(r"!?\[[^\]]*\]\(([^)]+)\)", text)
        links += re.findall(r"(?m)^\s*\[[^\]]+\]:\s*(\S+)", text)
        for raw in links:
            target = raw.strip().strip("<>").split(' "', 1)[0]
            parsed = urlsplit(target)
            if parsed.scheme in {"https", "http", "mailto"}:
                continue
            verify.check("skill_links_are_relative", not parsed.scheme and not parsed.netloc
                         and not parsed.path.startswith(("/", "\\")))
            resolved = posixpath.normpath(posixpath.join(posixpath.dirname(name), unquote(parsed.path))) if parsed.path else name
            verify.check("skill_relative_link_targets_exist", resolved in files)
            if parsed.fragment and resolved.endswith(".md"):
                headings = re.findall(r"(?m)^#{1,6}\s+(.+?)\s*$", texts[resolved])
                slugs = {re.sub(r"[^\w\s-]", "", heading.lower()).replace(" ", "-") for heading in headings}
                verify.check("skill_relative_link_anchors_exist", unquote(parsed.fragment) in slugs)
            link_count += 1
    verify.report["skill_relative_links_checked"] = link_count
    return texts


def isolated_env(home, temp):
    env = os.environ.copy()
    for key in ("CSM_HOME", "CSM_DB", "CLAUDE_PROJECT_DIR", "CODEX_HOME", "CLAUDE_CONFIG_DIR",
                "CSM_CAPTURE_TEXT", "CSM_BRIEF_BYTES"):
        env.pop(key, None)
    env.update({"USERPROFILE": str(home), "HOME": str(home), "CSM_CLAUDE_CLI": "none",
                "TEMP": str(temp), "TMP": str(temp)})
    return env


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def verify_runtime(exe, base, skill, version, verify):
    home = base / "Buyer Home 日本語 ü"
    project = base / "Project with spaces 日本語"
    scratch = base / "Isolated scratch"
    for directory in (home / ".claude", home / ".codex", project / ".git", scratch):
        directory.mkdir(parents=True)
    db = base / "Private data ü" / "custom memory.db"
    env = isolated_env(home, scratch)
    other_hook = {"hooks": [{"type": "command", "command": "echo unrelated"}]}
    originals = {
        home / ".claude/settings.json": json.dumps({"theme": "dark", "hooks": {"Stop": [other_hook]}}).encode(),
        home / ".claude.json": json.dumps({"numStartups": 3, "mcpServers": {"other": {"command": "unrelated"}}}).encode(),
        home / ".codex/hooks.json": json.dumps({"hooks": {"Stop": [other_hook]}}).encode(),
        home / ".codex/config.toml": b'model = "verification-only"\n\n[mcp_servers.other]\ncommand = "unrelated"\n',
    }
    for path, data in originals.items():
        path.write_bytes(data)

    def csm(name, *args, binary=exe, input_bytes=b""):
        return verify.command(name, [str(binary), "--db", str(db), *args], project, env, input_bytes)

    verify.check("packaged_version_matches_manifest", csm("packaged_binary_runs", "--version").strip() == f"csm {version}")
    csm("setup_dry_run", "setup", "--dry-run")
    verify.check("dry_run_preserves_configs", all(path.read_bytes() == data for path, data in originals.items()))
    verify.check("dry_run_creates_no_installation", not (home / ".csm").exists() and not db.exists())
    csm("setup_installs_both_fake_hosts", "setup", "--yes")
    installed = home / ".csm/bin/csm.exe"
    verify.check("installed_binary_matches_archive", installed.read_bytes() == exe.read_bytes())
    status = csm("setup_status", "setup", "--status", binary=installed)
    verify.check("both_hosts_registered", "Claude Code: hooks 6/6 installed, MCP server registered" in status
                 and "Codex: hooks 5/5 installed, MCP server registered" in status)
    backup_root = home / ".csm/backups"
    backups = {p.relative_to(backup_root): p.read_bytes() for p in backup_root.rglob("*") if p.is_file()}
    verify.check("all_original_configs_backed_up", all(data in backups.values() for data in originals.values()))
    current = {path: path.read_bytes() for path in originals}
    rerun = csm("setup_rerun", "setup", "--yes")
    verify.check("setup_is_idempotent", "Nothing to change." in rerun
                 and all(path.read_bytes() == data for path, data in current.items()))
    verify.check("idempotent_setup_preserves_backups", backups == {
        p.relative_to(backup_root): p.read_bytes() for p in backup_root.rglob("*") if p.is_file()})
    config = tomllib.loads((home / ".codex/config.toml").read_text(encoding="utf-8"))
    verify.check("custom_database_recorded_in_mcp", config["mcp_servers"]["csm"]["args"] == ["--db", db.as_posix(), "mcp"])

    marker = "Release marker: violet-otter 日本語 ü."
    checkpoint = "Packaged continuity verified — 日本語 ü."
    next_step = "Review the isolated release evidence."
    csm("separate_process_save", "save", marker, "--kind", "fact", "--project", str(project), binary=installed)
    csm("separate_process_checkpoint", "checkpoint", checkpoint, "--next", next_step, "--project", str(project), binary=installed)
    search = csm("separate_process_search", "search", "violet-otter", "--project", str(project), binary=installed)
    verify.check("saved_unicode_memory_recalled", marker in search)
    brief = csm("separate_process_brief", "brief", "--project", str(project), binary=installed)
    verify.check("checkpoint_and_next_step_recalled", checkpoint in brief and next_step in brief)

    requests = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "release-verification", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "csm_brief", "arguments": {"project": str(project)}}},
    ]
    wire = ("\n".join(json.dumps(request, ensure_ascii=False) for request in requests) + "\n").encode("utf-8")
    replies = csm("packaged_mcp_stdio", "mcp", "--project", str(project), binary=installed, input_bytes=wire)
    messages = {item["id"]: item for item in map(json.loads, replies.splitlines())}
    expected_tools = {"csm_save", "csm_search", "csm_list", "csm_delete", "csm_status", "csm_brief", "csm_checkpoint"}
    verify.check("mcp_initialize_succeeds", messages[1]["result"]["serverInfo"]["version"] == version)
    verify.check("mcp_exposes_exactly_seven_tools", {tool["name"] for tool in messages[2]["result"]["tools"]} == expected_tools
                 and len(messages[2]["result"]["tools"]) == 7)
    result = messages[3]["result"]
    verify.check("mcp_recalls_unicode_checkpoint", not result.get("isError") and checkpoint in result["content"][0]["text"])

    hooks = read_json(home / ".codex/hooks.json")
    command = hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
    payload = {"hook_event_name": "SessionStart", "session_id": "release-verification", "cwd": str(project), "source": "startup"}
    hook_output = verify.command("generated_powershell_hook_runs", ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", command],
                                 project, env, json.dumps(payload, ensure_ascii=False).encode("utf-8"))
    context = json.loads(hook_output)["hookSpecificOutput"]["additionalContext"]
    verify.check("generated_hook_injects_unicode_checkpoint", checkpoint in context and next_step in context)
    verify.check("custom_database_used_exclusively", db.is_file() and not (home / ".csm/csm.db").exists())

    # Follow the documented smoke block itself, with TEMP and every profile path isolated.
    operations = skill["csm-project-memory/references/operations.md"]
    section = operations.split("## Isolated smoke test", 1)[1]
    smoke = re.search(r"```powershell\s*\n(.*?)\n```", section, re.DOTALL).group(1)
    invocations = [line.strip() for line in smoke.splitlines() if line.strip().startswith("&")]
    verify.check("skill_smoke_uses_explicit_temporary_database", len(invocations) == 4
                 and all(re.match(r"& \$csmExe --db \$csmTestDb (save|checkpoint|search|brief)\b", line) for line in invocations))
    escaped_exe = str(exe).replace("'", "''")
    output = verify.command("documented_skill_smoke_runs", ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                            f"$ErrorActionPreference = 'Stop'\n$csmExe = '{escaped_exe}'\n{smoke}"], project, env)
    verify.check("documented_skill_smoke_recalls_memory_and_handoff", "violet-otter" in output
                 and "The isolated smoke marker was saved." in output and "Recall the marker." in output)
    verify.check("documented_skill_smoke_is_isolated", len(list(scratch.glob("csm-smoke-*/smoke.db"))) == 1
                 and not (home / ".csm/csm.db").exists())

    csm("uninstall_removes_registrations", "setup", "--uninstall", "--yes", binary=installed)
    for path, data in originals.items():
        if path.suffix == ".toml":
            same = tomllib.loads(path.read_text(encoding="utf-8")) == tomllib.loads(data.decode())
        else:
            same = read_json(path) == json.loads(data)
        verify.check("uninstall_preserves_unrelated_config", same)
    verify.check("uninstall_retains_runtime_and_database", installed.is_file() and db.is_file())
    verify.check("memory_survives_uninstall", marker in csm("post_uninstall_recall", "search", "violet-otter", "--project", str(project), binary=installed))


def run(package_dir, verify):
    verify.check("windows_environment", os.name == "nt")
    verify.current = "outer_checksums_readable"
    checksums = parse_checksums((package_dir / "SHA256SUMS.txt").read_bytes())
    archives = sorted(package_dir.glob("*.zip"))
    verify.check("outer_checksum_archive_set_matches", {path.name for path in archives} == set(checksums))
    for path in archives:
        digest = sha256(path.read_bytes())
        verify.check("outer_archive_checksum_matches", digest == checksums[path.name])
        verify.report["archives"].append({"file": path.name, "sha256": digest, "bytes": path.stat().st_size})
    manifest = read_json(package_dir / "release-manifest.json")
    verify.check("release_manifest_hashes_match", manifest["archives"] == verify.report["archives"]
                 or sorted(manifest["archives"], key=lambda item: item["file"]) == verify.report["archives"])
    verify.report["version"] = manifest["version"]
    runtime = [path for path in archives if path.name.endswith("-windows-x64.zip")]
    skill = [path for path in archives if path.name.endswith("-agensi.zip")]
    verify.check("runtime_and_skill_archives_identified", len(runtime) == 1 and len(skill) == 1)
    runtime_files = archive_files(runtime[0], verify, "runtime")
    skill_files = archive_files(skill[0], verify, "skill")
    skill_text = verify_skill(skill_files, verify)
    root = runtime[0].stem
    verify.check("runtime_single_expected_root", {name.split("/", 1)[0] for name in runtime_files} == {root})
    expected = parse_checksums(runtime_files[f"{root}/SHA256SUMS.txt"])
    actual = {name[len(root) + 1:]: sha256(data) for name, data in runtime_files.items() if name != f"{root}/SHA256SUMS.txt"}
    verify.check("all_runtime_internal_checksums_match", actual == expected)
    verify.check("runtime_binary_matches_release_manifest", actual["csm.exe"] == manifest["runtime_sha256"])

    with tempfile.TemporaryDirectory(prefix="csm release vérification 日本語 ") as temporary:
        base = Path(temporary).resolve()
        verify.check("temporary_workspace_is_contained", base.parent == Path(tempfile.gettempdir()).resolve())
        extraction = base / "Extracted runtime 日本語"
        for name, data in runtime_files.items():
            destination = extraction.joinpath(*PurePosixPath(name).parts)
            verify.check("runtime_extraction_is_contained", destination.resolve().is_relative_to(extraction.resolve()))
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(data)
        verify_runtime(extraction / root / "csm.exe", base, skill_text, manifest["version"], verify)
    verify.check("temporary_workspace_removed", not base.exists())
    verify.report["scope"] = {"packaged_runtime": True, "isolated_cli_mcp_and_hook": True,
                              "documented_skill_smoke": True, "real_host_session": False,
                              "claude_account_or_model_used": False, "marketplace_submission": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package-dir", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    verify = Verifier()
    try:
        run(args.package_dir.resolve(), verify)
        verify.report["result"] = "passed"
        status = 0
    except Exception as error:
        verify.report["result"] = "failed"
        verify.report["failure"] = {"check": str(error) if isinstance(error, VerificationFailure) else verify.current,
                                    "type": type(error).__name__}
        status = 1
    args.report.parent.mkdir(parents=True, exist_ok=True)
    serialized = json.dumps(verify.report, ensure_ascii=True, indent=2) + "\n"
    args.report.write_text(serialized, encoding="utf-8")
    print(serialized, end="")
    return status


if __name__ == "__main__":
    raise SystemExit(main())
