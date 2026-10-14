//! Local fake for `claude mcp`: no Claude installation, account, or model is used.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

fn config_backups(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(config_backups(&path));
            } else if path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with("claude") && !name.starts_with("claude-settings") && name.ends_with(".json")
            }) {
                found.push(path);
            }
        }
    }
    found
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    assert_eq!(args.first().map(String::as_str), Some("mcp"));
    assert_eq!(args.get(2).map(String::as_str), Some("csm"));
    assert_eq!(&args[args.len() - 2..], ["--scope", "user"]);
    let home = PathBuf::from(std::env::var_os("CSM_STUB_HOME").unwrap());
    let config = home.join(".claude.json");
    let expected = home.join("expected.json");
    let operation = args[1].as_str();
    writeln!(OpenOptions::new().create(true).append(true).open(home.join("calls.log")).unwrap(), "{operation}")
        .unwrap();
    if operation == "remove" {
        let backups = config_backups(&home.join(".csm/backups"));
        if expected.exists() {
            let bytes = fs::read(&expected).unwrap();
            assert_eq!(fs::read(&config).unwrap(), bytes, "unexpected starting config");
            assert!(
                backups.iter().any(|path| fs::read(path).unwrap() == bytes),
                "original .claude.json was not backed up before the first CLI mutation"
            );
        } else {
            assert!(!config.exists(), "expected a fresh config");
            assert!(backups.is_empty(), "a missing config must not produce a fake backup");
        }
    }
    match operation {
        "remove" if config.exists() => {
            fs::write(config, fs::read(home.join("removed.json")).unwrap()).unwrap();
        }
        "remove" => std::process::exit(1), // The real CLI can report that no registration exists.
        "add-json" => {
            if std::env::var_os("CSM_STUB_FAIL_ADD").is_some() {
                eprintln!("intentional fake registration failure");
                std::process::exit(17);
            }
            assert_eq!(args.len(), 6);
            fs::write(config, fs::read(home.join("configured.json")).unwrap()).unwrap();
        }
        _ => panic!("unexpected CLI operation: {operation}"),
    }
}
