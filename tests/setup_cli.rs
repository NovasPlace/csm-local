//! `csm setup` end to end against a fake home directory, including running
//! the installed hook commands through the shells the hosts use.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

struct FakeHome {
    root: PathBuf,
}

impl FakeHome {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("csm-setup-it-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home").join(".claude")).unwrap();
        fs::create_dir_all(root.join("home").join(".codex")).unwrap();
        fs::create_dir_all(root.join("repo").join(".git")).unwrap();
        FakeHome { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn command(&self, program: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(self.root.join("repo"))
            .env("USERPROFILE", self.home())
            .env("HOME", self.home())
            .env("CSM_CLAUDE_CLI", "none")
            .env_remove("CSM_HOME")
            .env_remove("CSM_DB")
            .env_remove("CLAUDE_PROJECT_DIR");
        cmd
    }

    fn csm(&self, args: &[&str]) -> Output {
        self.command(Path::new(env!("CARGO_BIN_EXE_csm"))).args(args).stdin(Stdio::null()).output().unwrap()
    }

    fn read_json(&self, rel: &str) -> Value {
        serde_json::from_str(&fs::read_to_string(self.home().join(rel)).unwrap()).unwrap()
    }
}

impl Drop for FakeHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// Runs a hook command line through a shell, as the host would.
fn run_in_shell(fake: &FakeHome, shell: &[&str], command_line: &str, payload: Value) -> Output {
    let mut command = fake.command(Path::new(shell[0]));
    #[cfg(windows)]
    if shell[0].eq_ignore_ascii_case("cmd.exe") {
        use std::os::windows::process::CommandExt;
        // cmd parses a command string, not C argv: `.arg` would insert literal backslash-quotes.
        command.args(["/D", "/S"]).args(&shell[1..]).raw_arg(format!("\"{command_line}\""));
    } else {
        command.args(&shell[1..]).arg(command_line);
    }
    #[cfg(not(windows))]
    command.args(&shell[1..]).arg(command_line);
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn install_status_rerun_and_uninstall() {
    let fake = FakeHome::new("flow");
    fs::write(fake.home().join(".claude").join("settings.json"), "{\n  \"theme\": \"dark\"\n}\n").unwrap();
    let config = "model = \"gpt-5\"\n\n[mcp_servers.node_repl]\ncommand = \"node\"\n";
    fs::write(fake.home().join(".codex").join("config.toml"), config).unwrap();

    let not_confirmed = fake.csm(&["setup"]);
    assert!(not_confirmed.status.success(), "{}", text(&not_confirmed));
    assert!(text(&not_confirmed).contains("Nothing was changed. Run `csm setup --yes`"), "{}", text(&not_confirmed));
    let dry = fake.csm(&["setup", "--dry-run"]);
    assert!(text(&dry).contains("Dry run: nothing was changed."), "{}", text(&dry));
    assert!(!fake.home().join(".codex").join("hooks.json").exists());

    let installed = fake.csm(&["setup", "--yes"]);
    let out = text(&installed);
    assert!(installed.status.success(), "{out}");
    assert!(out.contains("Checked the installed binary"), "{out}");
    assert!(out.contains("open /hooks and trust the csm hooks"), "{out}");
    assert!(fake.home().join(".csm").join("bin").join("csm.exe").exists() || !cfg!(windows));

    let settings = fake.read_json(".claude/settings.json");
    assert_eq!(settings["theme"], "dark");
    assert_eq!(settings["hooks"].as_object().unwrap().len(), 6);
    let claude_json = fake.read_json(".claude.json");
    assert_eq!(claude_json["mcpServers"]["csm"]["args"], json!(["mcp"]));
    let codex_config = fs::read_to_string(fake.home().join(".codex").join("config.toml")).unwrap();
    assert!(codex_config.starts_with(config) && codex_config.contains("[mcp_servers.csm]"), "{codex_config}");

    let status = text(&fake.csm(&["setup", "--status"]));
    assert!(status.contains("Claude Code: hooks 6/6 installed, MCP server registered"), "{status}");
    assert!(status.contains("Codex: hooks 5/5 installed, MCP server registered"), "{status}");

    let again = text(&fake.csm(&["setup", "--yes"]));
    assert!(again.contains("Nothing to change."), "{again}");

    let removed = fake.csm(&["setup", "--uninstall", "--yes"]);
    assert!(removed.status.success(), "{}", text(&removed));
    assert_eq!(fake.read_json(".claude/settings.json"), json!({ "theme": "dark" }));
    assert_eq!(fs::read_to_string(fake.home().join(".codex").join("config.toml")).unwrap(), config);
    let status = text(&fake.csm(&["setup", "--status"]));
    assert!(status.contains("Claude Code: hooks 0/6 installed, MCP server not registered"), "{status}");
    let backups = fake.home().join(".csm").join("backups");
    assert!(fs::read_dir(&backups).unwrap().count() >= 1, "backups were written");
}

#[cfg(windows)]
#[test]
fn installed_hook_commands_work_through_powershell_and_cmd() {
    let fake = FakeHome::new("shells");
    let installed = fake.csm(&["setup", "--yes"]);
    assert!(installed.status.success(), "{}", text(&installed));
    let repo = fake.root.join("repo").display().to_string();

    let codex_hooks = fake.read_json(".codex/hooks.json");
    let codex_cmd = codex_hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"].as_str().unwrap().to_string();
    let claude_cmd = fake.read_json(".claude/settings.json")["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    let powershell = ["powershell.exe", "-NoProfile", "-Command"];
    let cmd = ["cmd.exe", "/C"];

    let prompt = json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1", "cwd": repo, "turn_id": "t1",
                         "prompt": "wire up the installer — ünïcødé ok" });
    let out = run_in_shell(&fake, &powershell, &codex_cmd, prompt);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(out.stdout.is_empty(), "{}", text(&out));

    let stop = json!({ "hook_event_name": "Stop", "session_id": "s1", "cwd": repo, "stop_hook_active": false,
                       "last_assistant_message": "Installer done." });
    let out = run_in_shell(&fake, &cmd, &claude_cmd, stop);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));

    let start = json!({ "hook_event_name": "SessionStart", "session_id": "s2", "cwd": repo, "source": "startup",
                        "model": "m", "permission_mode": "default", "transcript_path": null });
    let start_cmd = codex_hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
    let out = run_in_shell(&fake, &powershell, start_cmd, start);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("not JSON: {}", text(&out)));
    let context = reply["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(context.contains("Last request: \"wire up the installer — ünïcødé ok\""), "{context}");
    assert!(context.contains("Last reply: \"Installer done.\""), "{context}");
}

#[cfg(windows)]
#[test]
fn installed_hooks_support_space_and_unicode_paths() {
    let powershell = ["powershell.exe", "-NoProfile", "-Command"];
    let git_bash = std::env::var_os("ProgramFiles").map(PathBuf::from).map(|p| p.join("Git/bin/bash.exe"));
    for custom_db in [false, true] {
        let fake = FakeHome::new(if custom_db { "custom spáces 日本語" } else { "default spáces 日本語" });
        let db = fake.root.join("memory data ü").join("session memory.db");
        let db_arg = db.to_str().unwrap();
        let run_setup = |flags: &[&str]| {
            let mut args = if custom_db { vec!["--db", db_arg, "setup"] } else { vec!["setup"] };
            args.extend_from_slice(flags);
            fake.csm(&args)
        };
        let unrelated = json!({ "hooks": [{ "type": "command", "command": "echo unrelated" }] });
        let settings = json!({ "theme": "dark", "hooks": { "Stop": [unrelated.clone()] } });
        let hooks = json!({ "hooks": { "Stop": [unrelated] } });
        fs::write(fake.home().join(".claude/settings.json"), settings.to_string()).unwrap();
        fs::write(fake.home().join(".codex/hooks.json"), hooks.to_string()).unwrap();
        let installed = run_setup(&["--yes"]);
        assert!(installed.status.success(), "{}", text(&installed));

        let codex_hooks = fake.read_json(".codex/hooks.json");
        let claude_hooks = fake.read_json(".claude/settings.json");
        let command =
            |root: &Value, event: &str| root["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap().to_string();
        let codex_command = command(&codex_hooks, "SessionStart");
        let claude_command = command(&claude_hooks, "UserPromptSubmit");
        let cwd = fake.root.join("repo").display().to_string();
        let prompt = "Remember the real work — 日本語 and ü survive spaced paths";
        let out = run_in_shell(
            &fake,
            &powershell,
            &codex_command,
            json!({ "hook_event_name": "UserPromptSubmit", "session_id": "work", "cwd": cwd, "prompt": prompt }),
        );
        assert_eq!(out.status.code(), Some(0), "{}", text(&out));
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{}", text(&out));

        let mut shells = vec![vec!["cmd.exe".to_string(), "/C".to_string()]];
        if let Some(bash) = git_bash.as_ref().filter(|p| p.is_file()) {
            shells.push(vec![bash.to_str().unwrap().to_string(), "--noprofile".into(), "--norc".into(), "-c".into()]);
        }
        for (index, shell) in shells.iter().enumerate() {
            let reply = format!("Spaced-path reply from shell {index}");
            let shell: Vec<&str> = shell.iter().map(String::as_str).collect();
            let out = run_in_shell(
                &fake,
                &shell,
                &claude_command,
                json!({ "hook_event_name": "Stop", "session_id": "work", "cwd": cwd,
                    "last_assistant_message": reply, "stop_hook_active": false, "turn_id": format!("t{index}") }),
            );
            assert_eq!(out.status.code(), Some(0), "{shell:?}: {}", text(&out));
            assert!(out.stderr.is_empty(), "{shell:?}: {}", text(&out));
            let out = run_in_shell(
                &fake,
                &powershell,
                &codex_command,
                json!({ "hook_event_name": "SessionStart", "session_id": format!("reader-{index}"), "cwd": cwd, "source": "startup" }),
            );
            let result: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{}", text(&out)));
            let context = result["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
            assert!(context.contains(prompt) && context.contains(&reply), "{shell:?}: {context}");
        }
        if custom_db {
            assert!(db.is_file());
            assert!(!fake.home().join(".csm/csm.db").exists(), "hook ignored the custom database");
        }
        let again = run_setup(&["--yes"]);
        assert!(again.status.success() && text(&again).contains("Nothing to change."), "{}", text(&again));
        assert_eq!(fake.read_json(".codex/hooks.json"), codex_hooks);
        assert_eq!(fake.read_json(".claude/settings.json"), claude_hooks);
        let status = text(&run_setup(&["--status"]));
        assert!(status.contains("hooks 6/6 installed") && status.contains("hooks 5/5 installed"), "{status}");
        let removed = run_setup(&["--uninstall", "--yes"]);
        assert!(removed.status.success(), "{}", text(&removed));
        assert_eq!(fake.read_json(".claude/settings.json"), settings);
        assert_eq!(fake.read_json(".codex/hooks.json"), hooks);
    }
}
