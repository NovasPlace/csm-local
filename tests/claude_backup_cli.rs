//! Exercises the Claude CLI setup path with a local Rust stub and isolated homes.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

const ORIGINAL: &str = "\u{feff}{\r\n  \"theme\": \"snowman ☃\",\r\n  \"mcpServers\": {\r\n    \"other\": {\"command\": \"unrelated\"},\r\n    \"csm\": {\"command\": \"old-csm\", \"args\": [\"mcp\"]}\r\n  }\r\n}\r\n";

struct Sandbox {
    root: PathBuf,
    temp: PathBuf,
    stub: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = temp.join(format!("csm-claude-backup-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let stub = root.join(if cfg!(windows) { "claude-stub.exe" } else { "claude-stub" });
        let compiled = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition=2021", "--crate-name", "csm_claude_stub"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_stub.rs"))
            .arg("-o")
            .arg(&stub)
            .output()
            .unwrap();
        assert!(compiled.status.success(), "{compiled:?}");
        Self { root, temp, stub }
    }

    fn home(&self, name: &str, existing: bool) -> PathBuf {
        let home = self.root.join(name);
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir(home.join("repo")).unwrap();
        if existing {
            fs::write(home.join(".claude.json"), ORIGINAL).unwrap();
            fs::write(home.join("expected.json"), ORIGINAL).unwrap();
        }
        let removed = json!({ "theme": "snowman ☃", "mcpServers": { "other": { "command": "unrelated" } } });
        let mut configured = removed.clone();
        configured["mcpServers"]["csm"] = json!({
            "type": "stdio",
            "command": home.join(".csm/bin").join(if cfg!(windows) { "csm.exe" } else { "csm" }).to_string_lossy().replace('\\', "/"),
            "args": ["mcp"]
        });
        fs::write(home.join("removed.json"), removed.to_string()).unwrap();
        fs::write(home.join("configured.json"), configured.to_string()).unwrap();
        home
    }

    fn setup(&self, home: &Path, args: &[&str], fail_add: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_csm"));
        command
            .args(["setup", "--only", "claude"])
            .args(args)
            .current_dir(home.join("repo"))
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("CSM_CLAUDE_CLI", &self.stub)
            .env("CSM_STUB_HOME", home)
            .env_remove("CSM_HOME")
            .env_remove("CSM_DB")
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("CSM_STUB_FAIL_ADD")
            .stdin(Stdio::null());
        if fail_add {
            command.env("CSM_STUB_FAIL_ADD", "1");
        }
        command.output().unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Ok(root) = fs::canonicalize(&self.root) {
            if root.parent() == Some(self.temp.as_path())
                && root.file_name().is_some_and(|name| name.to_string_lossy().starts_with("csm-claude-backup-"))
            {
                let _ = fs::remove_dir_all(root);
            }
        }
    }
}

fn backup_files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(backup_files(&path));
            } else {
                files.push((path.clone(), fs::read(path).unwrap()));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn config_backups(home: &Path) -> Vec<Vec<u8>> {
    backup_files(&home.join(".csm/backups"))
        .into_iter()
        .filter(|(path, _)| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with("claude") && !name.starts_with("claude-settings") && name.ends_with(".json")
        })
        .map(|(_, bytes)| bytes)
        .collect()
}

fn output(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

#[test]
fn claude_cli_mutations_require_recoverable_config_backups() {
    let sandbox = Sandbox::new();
    let home = sandbox.home("flow", true);
    let dry = sandbox.setup(&home, &["--dry-run"], false);
    assert!(dry.status.success(), "{}", output(&dry));
    assert!(!home.join("calls.log").exists());
    assert!(!home.join(".csm/backups").exists());
    assert_eq!(fs::read(home.join(".claude.json")).unwrap(), ORIGINAL.as_bytes());

    let installed = sandbox.setup(&home, &["--yes"], false);
    assert!(installed.status.success(), "{}", output(&installed));
    assert_eq!(fs::read_to_string(home.join("calls.log")).unwrap(), "remove\nadd-json\n");
    assert_eq!(config_backups(&home), vec![ORIGINAL.as_bytes().to_vec()]);
    let configured = fs::read(home.join("configured.json")).unwrap();
    assert_eq!(fs::read(home.join(".claude.json")).unwrap(), configured);
    let backups = backup_files(&home.join(".csm/backups"));
    for args in [&["--dry-run"][..], &["--yes"][..]] {
        let again = sandbox.setup(&home, args, false);
        assert!(again.status.success(), "{}", output(&again));
        assert_eq!(fs::read_to_string(home.join("calls.log")).unwrap(), "remove\nadd-json\n");
        assert_eq!(backup_files(&home.join(".csm/backups")), backups);
    }

    fs::write(home.join("expected.json"), &configured).unwrap();
    let removed = sandbox.setup(&home, &["--uninstall", "--yes"], false);
    assert!(removed.status.success(), "{}", output(&removed));
    assert_eq!(fs::read_to_string(home.join("calls.log")).unwrap(), "remove\nadd-json\nremove\n");
    let snapshots = config_backups(&home);
    assert_eq!(snapshots.len(), 2);
    assert!(snapshots.iter().any(|bytes| bytes == ORIGINAL.as_bytes()));
    assert!(snapshots.contains(&configured));
    assert_eq!(fs::read(home.join(".claude.json")).unwrap(), fs::read(home.join("removed.json")).unwrap());

    let blocked = sandbox.home("blocked", true);
    fs::create_dir(blocked.join(".csm")).unwrap();
    fs::write(blocked.join(".csm/backups"), "occupied by a file").unwrap();
    let failed = sandbox.setup(&blocked, &["--yes"], false);
    assert!(!failed.status.success(), "backup failure was ignored: {}", output(&failed));
    // Settings were absent: their earlier creation requires no backup and must succeed first.
    assert!(blocked.join(".claude/settings.json").is_file());
    assert!(!blocked.join("calls.log").exists(), "CLI ran without a backup");
    assert_eq!(fs::read(blocked.join(".claude.json")).unwrap(), ORIGINAL.as_bytes());

    let failure = sandbox.home("cli-failure", true);
    let failed = sandbox.setup(&failure, &["--yes"], true);
    assert!(!failed.status.success(), "{}", output(&failed));
    assert!(output(&failed).contains("intentional fake registration failure"), "{}", output(&failed));
    assert_eq!(fs::read_to_string(failure.join("calls.log")).unwrap(), "remove\nadd-json\n");
    assert_eq!(config_backups(&failure), vec![ORIGINAL.as_bytes().to_vec()]);

    let fresh = sandbox.home("fresh", false);
    let installed = sandbox.setup(&fresh, &["--yes"], false);
    assert!(installed.status.success(), "{}", output(&installed));
    assert_eq!(fs::read(fresh.join(".claude.json")).unwrap(), fs::read(fresh.join("configured.json")).unwrap());
    assert!(config_backups(&fresh).is_empty(), "missing config acquired a fake backup");
}
