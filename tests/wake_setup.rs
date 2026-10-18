use csm_local::setup::{self, Options, SetupEnv, Step};
use serde_json::Value;
use std::fs;

#[test]
fn native_wake_hook_is_opt_in_preserved_on_update_and_removed_without_touching_other_hooks() {
    let root = std::env::temp_dir().join(format!("csm-wake-setup-{}", std::process::id()));
    fs::create_dir_all(root.join(".claude")).unwrap();
    fs::create_dir_all(root.join(".codex")).unwrap();
    let env = SetupEnv {
        home: root.clone(),
        data_dir: root.join("data"),
        db: root.join("data/mail.db"),
        exe: env!("CARGO_BIN_EXE_csm").into(),
        claude_cli: None,
        stamp: "wake-test".into(),
    };
    let settings = root.join(".claude/settings.json");
    fs::write(
        &settings,
        r#"{"theme":"dark","hooks":{"Stop":[{"hooks":[{"type":"command","command":"someone-else","async":true}]}]}}"#,
    )
    .unwrap();
    let plan = setup::plan(&env, &Options { wakeup: true, ..Default::default() }).unwrap();
    let claude = plan
        .steps
        .iter()
        .find_map(|step| match step {
            Step::Edit(e) if e.path == settings => Some(&e.after),
            _ => None,
        })
        .unwrap();
    let parsed: Value = serde_json::from_str(claude).unwrap();
    let stop = parsed["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop[0]["hooks"][0]["command"], "someone-else");
    let own = stop[1]["hooks"].as_array().unwrap();
    assert_eq!(own.len(), 2);
    assert_eq!(own[1]["asyncRewake"], true);
    assert_eq!(own[1]["timeout"], 604800);
    assert!(own[1]["command"].as_str().unwrap().contains("wake-hook claude"));
    for step in &plan.steps {
        if let Step::Edit(e) = step {
            fs::write(&e.path, &e.after).unwrap();
        }
    }
    let again = setup::plan(&env, &Options::default()).unwrap();
    assert!(!again.steps.iter().any(|s| matches!(s, Step::Edit(_))));
    let removed = setup::plan(&env, &Options { uninstall: true, ..Default::default() }).unwrap();
    let text = removed
        .steps
        .iter()
        .find_map(|s| match s {
            Step::Edit(e) if e.path == settings => Some(&e.after),
            _ => None,
        })
        .unwrap();
    assert!(text.contains("someone-else"));
    assert!(text.contains("dark"));
    assert!(!text.contains("wake-hook"));
    fs::remove_dir_all(root).unwrap();
}
