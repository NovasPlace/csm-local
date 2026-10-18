use csm_local::setup::{self, Options, SetupEnv, Step};
use serde_json::Value;
use std::fs;

#[test]
fn opt_in_hooks_preserve_other_handlers_and_survive_repeated_setup() {
    let root = std::env::temp_dir().join(format!("csm-mail-setup-{}", std::process::id()));
    fs::create_dir_all(root.join(".claude")).unwrap();
    fs::create_dir_all(root.join(".codex")).unwrap();
    let env = SetupEnv {
        home: root.clone(),
        data_dir: root.join("data"),
        db: root.join("data/csm.db"),
        exe: std::path::PathBuf::from(env!("CARGO_BIN_EXE_csm")),
        claude_cli: None,
        stamp: "mail-test".into(),
    };
    let settings = root.join(".claude/settings.json");
    fs::write(
        &settings,
        r#"{"hooks":{"PostToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"my-existing-hook"}]}]}}"#,
    )
    .unwrap();
    let changes = setup::plan(&env, &Options { messaging: true, ..Default::default() }).unwrap();
    for step in &changes.steps {
        if let Step::Edit(edit) = step {
            fs::write(&edit.path, &edit.after).unwrap();
        }
    }
    for path in [settings, root.join(".codex/hooks.json")] {
        let parsed: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let post = parsed["hooks"]["PostToolUse"].as_array().unwrap();
        let own: Vec<_> = post.iter().filter(|g| g["hooks"][0]["command"].as_str().unwrap().contains("csm")).collect();
        assert_eq!(own.len(), 1);
        assert!(own[0].get("matcher").is_none());
        assert!(own[0]["hooks"][0]["command"].as_str().unwrap().ends_with(" --messaging"));
    }
    let again = setup::plan(&env, &Options::default()).unwrap();
    assert!(!again.steps.iter().any(|s| matches!(s, Step::Edit(_))));
    let removed = setup::plan(&env, &Options { uninstall: true, ..Default::default() }).unwrap();
    let claude = removed
        .steps
        .iter()
        .find_map(|s| match s {
            Step::Edit(e) if e.path.ends_with("settings.json") => Some(&e.after),
            _ => None,
        })
        .unwrap();
    assert!(claude.contains("my-existing-hook"));
    assert!(!claude.contains("--messaging"));
    fs::remove_dir_all(root).unwrap();
}
