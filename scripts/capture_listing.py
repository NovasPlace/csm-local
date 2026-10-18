"""Render listing panels from isolated release output; never capture the real desktop.

Headlines are editorial. Terminal panels are real CLI/MCP/hook output for a demo
fixture. Only temporary roots are mapped to a buyer-style profile for display.
Requires Pillow for authoring; the installed runtime does not require Python.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

from PIL import Image, ImageDraw, ImageFont


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--binary", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    binary = args.binary.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    captures = {}
    with tempfile.TemporaryDirectory(prefix="csm-listing-") as temp:
        base = Path(temp)
        home = base / "Users/dev"
        runtime = home / "Downloads/csm-local-0.3.0-windows-x64"
        project = home / "Projects/CSV Export"
        scratch = base / "Scratch"
        for path in [home / ".claude", home / ".codex", runtime, project / ".git", scratch]:
            path.mkdir(parents=True, exist_ok=True)
        exe = runtime / "csm.exe"
        exe.write_bytes(binary.read_bytes())
        env = os.environ.copy()
        for key in ["CSM_HOME", "CSM_DB", "CODEX_HOME", "CLAUDE_CONFIG_DIR", "CLAUDE_PROJECT_DIR",
                    "CODEX_APP_TOOLS_PIPE_PATH", "CODEX_THREAD_ID", "CODEX_SESSION_ID"]:
            env.pop(key, None)
        env.update(USERPROFILE=str(home), HOME=str(home), TEMP=str(scratch), TMP=str(scratch), CSM_CLAUDE_CLI="none")
        db = home / ".csm/csm.db"

        def run(*words, data=None, cwd=runtime):
            result = subprocess.run([str(exe), *words], cwd=cwd, env=env, input=data,
                                    capture_output=True, timeout=30)
            if result.returncode:
                raise RuntimeError("Listing fixture command failed")
            return result.stdout.decode("utf-8-sig")

        def clean(text):
            for actual, display in [(str(home), "C:/Users/dev"), (home.as_posix(), "C:/Users/dev"),
                                    (str(base), "C:/Demo"), (base.as_posix(), "C:/Demo")]:
                text = text.replace(actual, display)
            text = text.replace("\\", "/")
            if re.search(r"(?:Donovan|AppData/Local/Temp|binding_token)", text, re.I):
                raise ValueError("Private data in listing capture")
            return text.strip()

        captures["dry_run"] = clean(run("setup", "--dry-run"))
        captures["install"] = clean(run("setup", "--yes"))
        captures["status"] = clean(run("setup", "--status"))

        def hook(host, event, session, **extra):
            payload = {"hook_event_name": event, "session_id": session, "cwd": str(project), **extra}
            return run("hook", host, data=json.dumps(payload).encode(), cwd=project)

        hook("claude", "SessionStart", "demo-claude", source="startup")
        hook("claude", "UserPromptSubmit", "demo-claude", prompt="Finish CSV export and check Unicode filenames.")
        source = project / "export.py"
        source.write_text("# Demo export fixture\n", encoding="utf-8")
        hook("claude", "PostToolUse", "demo-claude", tool_name="Write",
             tool_input={"file_path": str(source)}, tool_response={"success": True})
        hook("claude", "Stop", "demo-claude", last_assistant_message="CSV export is implemented; Unicode filename verification remains.")
        requests = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "listing-fixture", "version": "1"}}},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "csm_checkpoint", "arguments": {
                "project": str(project), "summary": "CSV export implemented; Unicode filename verification remains.",
                "next": ["Run the export checks with Unicode filenames."], "blockers": []}}},
        ]
        replies = run("mcp", "--project", str(project),
                      data=("\n".join(json.dumps(r) for r in requests) + "\n").encode(), cwd=project)
        checkpoint = next(json.loads(line) for line in replies.splitlines() if json.loads(line).get("id") == 2)["result"]
        if checkpoint.get("isError"):
            raise ValueError("MCP demo checkpoint failed")
        captures["mcp_checkpoint"] = clean(checkpoint["content"][0]["text"])
        briefing = json.loads(hook("codex", "SessionStart", "demo-codex", source="startup"))
        captures["briefing"] = clean(briefing["hookSpecificOutput"]["additionalContext"])
        assert "CSV export implemented" in captures["briefing"]
        assert "Run the export checks" in captures["briefing"]
        assert db.exists()

    version = subprocess.check_output([str(binary), "--version"], text=True).strip()
    captures["version"] = version
    (output / "captured-output.txt").write_text("\n\n".join(f"[{key}]\n{value}" for key, value in captures.items()) + "\n", encoding="utf-8")
    fonts = Path(os.environ.get("WINDIR", "C:/Windows")) / "Fonts"

    def font(name, size):
        return ImageFont.truetype(str(fonts / name), size)

    def wrapped(draw, value, face, width):
        lines = []
        for line in value.splitlines():
            if not line:
                lines.append("")
                continue
            while draw.textlength(line, font=face) > width:
                cut = len(line)
                while draw.textlength(line[:cut], font=face) > width:
                    cut -= 1
                split = line.rfind(" ", 0, cut)
                if split > cut // 2:
                    cut = split
                lines.append(line[:cut])
                line = "  " + line[cut:].lstrip()
            lines.append(line)
        return lines

    panels = [
        ("1-cover.png", "Your next session starts informed.", "Private project memory for Claude Code and Codex.",
         "A REAL DEMO BRIEFING", "\n".join(captures["briefing"].splitlines()[:13])),
        ("2-briefing.png", "Pick up where the last agent stopped.", "A checkpoint saved through MCP, then delivered by a startup hook.",
         "CODEX STARTUP / DEMO PROJECT", captures["briefing"]),
        ("3-setup-dry-run.png", "See the changes before installing.", "A local preview. Configuration files stay unchanged.",
         r"PS C:/Users/dev/Downloads/csm-local-0.3.0-windows-x64> .\csm.exe setup --dry-run", captures["dry_run"]),
        ("4-install.png", "Install with backups. Check the result.", "Registration status is separate from live model receipt.",
         "RELEASE EXECUTABLE / ISOLATED BUYER PROFILE",
         ".\\csm.exe setup --yes\n" + captures["install"] + "\n\n.\\csm.exe setup --status\n" + captures["status"]),
    ]
    manifest = {"runtime_version": version, "runtime_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "source": "isolated CLI, MCP checkpoint, and startup hook; demo fixture", "personal_paths": False,
                "live_model_used": False, "images": []}
    for filename, title, subtitle, label, body in panels:
        canvas = Image.new("RGB", (1600, 100), "#0d1626")
        draw = ImageDraw.Draw(canvas)
        face = font("consola.ttf", 22)
        lines = wrapped(draw, body, face, 1392)
        height = max(700, 410 + len(lines) * 29)
        canvas = Image.new("RGB", (1600, height), "#0d1626")
        draw = ImageDraw.Draw(canvas)
        draw.rectangle((0, 0, 1600, 10), fill="#6ae0c8")
        draw.text((72, 48), "CSM LOCAL 0.3.0  /  WINDOWS x64", font=font("segoeuib.ttf", 24), fill="#6ae0c8")
        draw.text((72, 102), title, font=font("segoeuib.ttf", 48), fill="#f2f6ff")
        draw.text((72, 176), subtitle, font=font("segoeui.ttf", 27), fill="#b1c0d9")
        bottom = height - 75
        draw.rounded_rectangle((70, 248, 1530, bottom), radius=18, fill="#162439", outline="#30435f", width=2)
        draw.text((100, 270), label, font=font("consola.ttf", 18), fill="#6ae0c8")
        y = 318
        for line in lines:
            draw.text((100, y), line, font=face, fill="#e0e9f7")
            y += 29
        assert y <= bottom + 4, "Terminal panel clipped"
        draw.text((72, height - 48), "Real release output • isolated demo data • profile paths mapped for display", font=font("segoeui.ttf", 18), fill="#a6b8d1")
        destination = output / filename
        canvas.save(destination)
        manifest["images"].append({"file": filename, "width": 1600, "height": height,
                                   "sha256": hashlib.sha256(destination.read_bytes()).hexdigest()})
    (output / "capture-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(manifest, indent=2))


if __name__ == "__main__":
    main()
