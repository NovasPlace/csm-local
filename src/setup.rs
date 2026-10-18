//! `csm setup`: installs the binary under `~/.csm/bin` and registers csm's
//! hooks and MCP server with Claude Code and Codex, at user scope only.
//!
//! Nothing is written into any repository. Every file is backed up before
//! it changes. Edits replace csm's own entries in place, so re-running is
//! harmless and other hooks keep their position (Codex ties hook trust to
//! position). `--uninstall` removes exactly what setup adds.

use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::time::{iso, now_ms};
use crate::{paths, Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub const ALL: [Agent; 2] = [Agent::Claude, Agent::Codex];

    pub fn parse(name: &str) -> Option<Agent> {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
        }
    }

    fn host(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

/// A hook csm registers: event, tool matcher, timeout in seconds.
type HookSpec = (&'static str, Option<&'static str>, u64);

const CLAUDE_TOOLS: &str = "Bash|PowerShell|Edit|MultiEdit|Write|NotebookEdit";
const CLAUDE_HOOKS: &[HookSpec] = &[
    ("SessionStart", None, 10),
    ("UserPromptSubmit", None, 5),
    ("PostToolUse", Some(CLAUDE_TOOLS), 5),
    ("PostToolUseFailure", Some(CLAUDE_TOOLS), 5),
    ("Stop", None, 5),
    ("SessionEnd", None, 2),
];
/// Codex runs every hook through PowerShell (about 200 ms each), so only
/// the tools csm records trigger one.
const CODEX_HOOKS: &[HookSpec] = &[
    ("SessionStart", None, 10),
    ("UserPromptSubmit", None, 5),
    ("PostToolUse", Some("^(Bash|apply_patch)$"), 5),
    ("Stop", None, 5),
    ("SessionEnd", None, 2),
];

const MCP_NAME: &str = "csm";

static CSM_HOOK_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)^\s*(?:&\s+)?(?:"(?:[^"\r\n]*[\\/])?csm(?:\.exe)?"|'(?:[^'\r\n]*[\\/])?csm(?:\.exe)?'|(?:[^\s"'&;|<>]*[\\/])?csm(?:\.exe)?)\s+hook\s+(?:claude|codex)(?:\s+--db\s+(?:"[^"\r\n]+"|'[^'\r\n]+'|[^\s"'&;|<>]+))?(?:\s+--messaging)?\s*$"#).unwrap()
});
static CSM_WAKE_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        &CSM_HOOK_COMMAND
            .as_str()
            .replace(r"\s+hook\s+(?:claude|codex)", r"\s+wake-hook\s+claude")
            .replace(r"(?:\s+--messaging)?", ""),
    )
    .unwrap()
});

/// Where things are, resolved once. Fields are public so tests can point
/// setup at a fake home directory.
pub struct SetupEnv {
    pub home: PathBuf,
    pub data_dir: PathBuf,
    pub db: PathBuf,
    /// The binary being installed (normally the running one).
    pub exe: PathBuf,
    /// The `claude` command line, used to register the MCP server the
    /// supported way. `None` falls back to editing `~/.claude.json`.
    pub claude_cli: Option<PathBuf>,
    pub stamp: String,
}

impl SetupEnv {
    pub fn detect(db: Option<PathBuf>) -> Result<Self> {
        let home = paths::home_dir().ok_or_else(|| Error::Invalid("cannot find the home directory".into()))?;
        let db = match db {
            Some(db) if !db.is_absolute() => return Err(Error::Invalid("--db must be an absolute path".into())),
            Some(db) => db,
            None => paths::default_db_path()?,
        };
        let claude_cli = match std::env::var("CSM_CLAUDE_CLI") {
            Ok(v) if v.trim().eq_ignore_ascii_case("none") => None,
            Ok(v) if !v.trim().is_empty() => Some(PathBuf::from(v.trim())),
            _ => find_on_path("claude"),
        };
        Ok(SetupEnv {
            home,
            data_dir: paths::data_dir()?,
            db,
            exe: std::env::current_exe()?,
            claude_cli,
            stamp: iso(now_ms()).replace(':', "-"),
        })
    }

    pub fn bin(&self) -> PathBuf {
        self.data_dir.join("bin").join(if cfg!(windows) { "csm.exe" } else { "csm" })
    }

    /// `--db <path>` for hook and MCP commands, needed only when the hosts
    /// would not find the database on their own (they run without CSM_HOME).
    fn db_args(&self) -> Vec<String> {
        let default = self.home.join(".csm").join("csm.db");
        if self.db == default {
            Vec::new()
        } else {
            vec!["--db".into(), slash(&self.db)]
        }
    }

    fn hook_command(&self, agent: Agent) -> String {
        let mut words = vec![slash(&self.bin()), "hook".into(), agent.host().into()];
        words.extend(self.db_args());
        let powershell = cfg!(windows) && agent == Agent::Codex;
        let call_operator = powershell && words[0].contains(' ');
        let command = words.iter().map(|word| quote_hook_word(word, powershell)).collect::<Vec<_>>().join(" ");
        if call_operator {
            format!("& {command}")
        } else {
            command
        }
    }

    fn messaging_hook_command(&self, agent: Agent, enabled: bool) -> String {
        let command = self.hook_command(agent);
        if enabled {
            format!("{command} --messaging")
        } else {
            command
        }
    }

    fn mcp_args(&self) -> Vec<String> {
        let mut args = self.db_args();
        args.push("mcp".into());
        args
    }
}

#[derive(Debug, Default)]
pub struct Options {
    pub dry_run: bool,
    pub yes: bool,
    pub uninstall: bool,
    /// Opt in to automatic session inbox delivery at supported hook boundaries.
    pub messaging: bool,
    pub wakeup: bool,
    /// Restrict to these agents; empty means every one found.
    pub only: Vec<Agent>,
}

/// A config file setup would rewrite, with its full new contents.
#[derive(Debug)]
pub struct FileEdit {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: String,
    pub describe: String,
    backup_name: &'static str,
}

#[derive(Debug)]
pub enum Step {
    CopyBinary {
        from: PathBuf,
        to: PathBuf,
    },
    Edit(FileEdit),
    /// Register (`Some(config)`) or remove (`None`) the MCP server through
    /// the `claude` command line.
    ClaudeMcp {
        cli: PathBuf,
        config: Option<Value>,
    },
}

#[derive(Debug, Default)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub notes: Vec<String>,
    /// Lines describing each agent and what will change for it.
    pub summary: Vec<String>,
    codex_hooks_changed: bool,
}

/// Works out every change without making any.
pub fn plan(env: &SetupEnv, opts: &Options) -> Result<Plan> {
    let mut plan = Plan::default();
    if !opts.uninstall {
        check_command_word(&slash(&env.bin()))?;
        for arg in env.db_args() {
            check_command_word(&arg)?;
        }
        if needs_copy(&env.exe, &env.bin())? {
            plan.steps.push(Step::CopyBinary { from: env.exe.clone(), to: env.bin() });
            plan.summary.push(format!("Binary: install {} -> {}", slash(&env.exe), slash(&env.bin())));
        } else {
            plan.summary.push(format!("Binary: {} is up to date", slash(&env.bin())));
        }
    }
    for agent in Agent::ALL {
        if !opts.only.is_empty() && !opts.only.contains(&agent) {
            continue;
        }
        let dir = env.home.join(match agent {
            Agent::Claude => ".claude",
            Agent::Codex => ".codex",
        });
        if !dir.is_dir() {
            if opts.only.contains(&agent) {
                plan.notes.push(format!(
                    "{} is not installed here ({} not found); skipped.",
                    agent.label(),
                    slash(&dir)
                ));
            }
            continue;
        }
        plan.summary.push(format!("{} ({}):", agent.label(), slash(&dir)));
        let before = plan.steps.len();
        match agent {
            Agent::Claude => plan_claude(env, opts, &mut plan)?,
            Agent::Codex => plan_codex(env, opts, &mut plan)?,
        }
        if plan.steps.len() == before {
            plan.summary.push(if opts.uninstall {
                "  nothing of csm's is configured".into()
            } else {
                "  already set up".into()
            });
        }
    }
    if plan.summary.iter().all(|l| l.starts_with("Binary")) {
        plan.notes.push("Neither Claude Code (~/.claude) nor Codex (~/.codex) was found.".into());
    }
    Ok(plan)
}

fn plan_claude(env: &SetupEnv, opts: &Options, plan: &mut Plan) -> Result<()> {
    let settings = env.home.join(".claude").join("settings.json");
    let before = read_optional(&settings)?;
    let mut root = parse_json_object(before.as_deref(), &settings)?;
    let wakeup = opts.wakeup || has_wake_hooks(&root);
    let messaging = opts.messaging || wakeup || has_messaging_hooks(&root);
    let expanded = inbox_hooks(CLAUDE_HOOKS, messaging);
    let wanted = if opts.uninstall { &[][..] } else { &expanded };
    merge_hooks(&mut root, wanted, &env.messaging_hook_command(Agent::Claude, messaging))?;
    if wakeup && !opts.uninstall {
        let command = env.hook_command(Agent::Claude).replacen(" hook claude", " wake-hook claude", 1);
        if let Some(groups) = root.get_mut("hooks").and_then(|h| h.get_mut("Stop")).and_then(Value::as_array_mut) {
            if let Some(group) = groups.iter_mut().find(|g| is_csm_group(g)) {
                group["hooks"]
                    .as_array_mut()
                    .expect("CSM group")
                    .push(json!({"type":"command","command":command,"asyncRewake":true,"timeout":604800}));
            }
        }
    }
    let after = render_json(&root, before.as_deref());
    if before.as_deref() != Some(after.as_str()) && !(before.is_none() && opts.uninstall) {
        let describe = if opts.uninstall {
            "remove csm's hooks".to_string()
        } else {
            format!("add hooks for {}", events(CLAUDE_HOOKS))
        };
        plan.summary.push(format!("  {}: {describe}", slash(&settings)));
        plan.steps.push(Step::Edit(FileEdit {
            path: settings,
            before,
            after,
            describe,
            backup_name: "claude-settings.json",
        }));
    }

    // The MCP server lives in ~/.claude.json, which Claude itself rewrites
    // constantly; its own command line is the safe way to change it.
    let claude_json = env.home.join(".claude.json");
    let current_text = read_optional(&claude_json)?;
    let mut current = parse_json_object(current_text.as_deref(), &claude_json)?;
    let existing = current.get("mcpServers").and_then(|s| s.get(MCP_NAME)).cloned();
    let config = json!({ "type": "stdio", "command": slash(&env.bin()), "args": env.mcp_args() });
    let up_to_date = existing
        .as_ref()
        .is_some_and(|e| e.get("command") == config.get("command") && e.get("args") == config.get("args"));
    let action = match (opts.uninstall, existing.is_some(), up_to_date) {
        (true, true, _) => Some(None),
        (false, _, false) => Some(Some(config)),
        _ => None,
    };
    let Some(config) = action else {
        return Ok(());
    };
    let describe =
        if config.is_some() { "register the csm MCP server (user scope)" } else { "remove the csm MCP server" };
    match &env.claude_cli {
        Some(cli) => {
            plan.summary.push(format!("  {describe}, using `claude mcp`"));
            plan.steps.push(Step::ClaudeMcp { cli: cli.clone(), config });
        }
        None => {
            let servers = current.entry("mcpServers").or_insert_with(|| Value::Object(Map::new()));
            let Value::Object(servers) = servers else {
                return Err(Error::Invalid(format!("\"mcpServers\" in {} is not an object", slash(&claude_json))));
            };
            match config {
                Some(config) => {
                    servers.insert(MCP_NAME.into(), config);
                }
                None => {
                    servers.shift_remove(MCP_NAME);
                }
            }
            let after = render_json(&current, current_text.as_deref());
            plan.summary.push(format!("  {}: {describe}", slash(&claude_json)));
            plan.steps.push(Step::Edit(FileEdit {
                path: claude_json,
                before: current_text,
                after,
                describe: describe.into(),
                backup_name: "claude.json",
            }));
        }
    }
    Ok(())
}

fn plan_codex(env: &SetupEnv, opts: &Options, plan: &mut Plan) -> Result<()> {
    let dir = env.home.join(".codex");
    let config_path = dir.join("config.toml");
    let config_before = read_optional(&config_path)?;
    let config = match &config_before {
        Some(text) => toml::from_str::<toml::Table>(text).map_err(|e| {
            Error::Invalid(format!("{} does not parse, so csm will not touch it: {e}", slash(&config_path)))
        })?,
        None => toml::Table::new(),
    };

    // Hooks defined inside config.toml would be a second definition next to
    // hooks.json in the same layer.
    let inline_hooks =
        config.get("hooks").and_then(toml::Value::as_table).is_some_and(|h| h.keys().any(|k| k != "state"));
    let hooks_path = dir.join("hooks.json");
    if inline_hooks && !opts.uninstall {
        plan.notes.push(format!(
            "{} defines hooks itself, so csm did not add {} (Codex wants one place per layer). Move those hooks into hooks.json and run setup again.",
            slash(&config_path),
            slash(&hooks_path)
        ));
    } else {
        let before = read_optional(&hooks_path)?;
        if before.is_some() || !opts.uninstall {
            let mut root = parse_json_object(before.as_deref(), &hooks_path)?;
            let messaging = opts.messaging || has_messaging_hooks(&root);
            let expanded = inbox_hooks(CODEX_HOOKS, messaging);
            let wanted = if opts.uninstall { &[][..] } else { &expanded };
            merge_hooks(&mut root, wanted, &env.messaging_hook_command(Agent::Codex, messaging))?;
            let after = render_json(&root, before.as_deref());
            if before.as_deref() != Some(after.as_str()) {
                let describe = if opts.uninstall {
                    "remove csm's hooks".to_string()
                } else {
                    format!("add hooks for {}", events(CODEX_HOOKS))
                };
                plan.summary.push(format!("  {}: {describe}", slash(&hooks_path)));
                plan.codex_hooks_changed = !opts.uninstall;
                plan.steps.push(Step::Edit(FileEdit {
                    path: hooks_path,
                    before,
                    after,
                    describe,
                    backup_name: "codex-hooks.json",
                }));
            }
        }
    }

    let table = format!("mcp_servers.{MCP_NAME}");
    let body = (!opts.uninstall).then(|| {
        format!(
            "command = {}\nargs = {}",
            toml::Value::String(slash(&env.bin())),
            toml::Value::Array(env.mcp_args().into_iter().map(toml::Value::String).collect())
        )
    });
    let text = config_before.clone().unwrap_or_default();
    let after = upsert_toml_table(&text, &table, body.as_deref());
    verify_toml_edit(&config, &after, body.is_some(), &config_path)?;
    let changed = config_before.as_deref() != Some(after.as_str()) && !(config_before.is_none() && body.is_none());
    if changed && normalize_newlines(&text) != normalize_newlines(&after) {
        let describe = if body.is_some() { format!("add [{table}]") } else { format!("remove [{table}]") };
        plan.summary.push(format!("  {}: {describe}", slash(&config_path)));
        plan.steps.push(Step::Edit(FileEdit {
            path: config_path,
            before: config_before,
            after,
            describe,
            backup_name: "codex-config.toml",
        }));
    }
    Ok(())
}

fn inbox_hooks(specs: &[HookSpec], enabled: bool) -> Vec<HookSpec> {
    specs
        .iter()
        .map(|&(event, matcher, timeout)| {
            (event, if enabled && event == "PostToolUse" { None } else { matcher }, timeout)
        })
        .collect()
}

fn has_messaging_hooks(root: &Map<String, Value>) -> bool {
    root.get("hooks").and_then(Value::as_object).is_some_and(|events| {
        events
            .values()
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|g| g.get("hooks").and_then(Value::as_array))
            .flatten()
            .any(|handler| {
                is_csm_handler(handler)
                    && handler
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|command| command.ends_with(" --messaging"))
            })
    })
}

/// Replaces csm's handlers in a hooks object (`{"hooks": {Event: [group]}}`)
/// with one group per wanted event. An existing csm group keeps its index;
/// other groups and handlers are left exactly where they are.
fn merge_hooks(root: &mut Map<String, Value>, wanted: &[HookSpec], command: &str) -> Result<()> {
    let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(hooks) = hooks else {
        return Err(Error::Invalid("\"hooks\" is not an object".into()));
    };
    let mut touched: Vec<String> = Vec::new();
    for (event, groups) in hooks.iter_mut() {
        if wanted.iter().any(|w| w.0 == event.as_str()) {
            continue;
        }
        if strip_csm_handlers(groups) {
            touched.push(event.clone());
        }
    }
    for &(event, matcher, timeout) in wanted {
        let groups = hooks.entry(event).or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(list) = groups else {
            return Err(Error::Invalid(format!("hooks.{event} is not an array")));
        };
        let ours: Vec<usize> = list.iter().enumerate().filter(|(_, g)| is_csm_group(g)).map(|(i, _)| i).collect();
        for group in list.iter_mut().filter(|g| !is_csm_group(g)) {
            strip_csm_handlers_in_group(group);
        }
        let group = csm_group(command, matcher, timeout);
        match ours.split_first() {
            Some((&first, rest)) => {
                list[first] = group;
                for &i in rest.iter().rev() {
                    list.remove(i);
                }
            }
            None => list.push(group),
        }
    }
    for event in touched {
        if hooks.get(&event).is_some_and(|v| v.as_array().is_some_and(Vec::is_empty)) {
            hooks.shift_remove(&event);
        }
    }
    if hooks.is_empty() {
        root.shift_remove("hooks");
    }
    Ok(())
}

fn csm_group(command: &str, matcher: Option<&str>, timeout: u64) -> Value {
    let mut group = Map::new();
    if let Some(m) = matcher {
        group.insert("matcher".into(), Value::String(m.into()));
    }
    group.insert("hooks".into(), json!([{ "type": "command", "command": command, "timeout": timeout }]));
    Value::Object(group)
}

fn is_csm_handler(handler: &Value) -> bool {
    handler
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| CSM_HOOK_COMMAND.is_match(c) || CSM_WAKE_COMMAND.is_match(c))
}

fn has_wake_hooks(root: &Map<String, Value>) -> bool {
    root.get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|h| h.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|g| g.get("hooks").and_then(Value::as_array))
        .flatten()
        .any(|h| h.get("command").and_then(Value::as_str).is_some_and(|s| CSM_WAKE_COMMAND.is_match(s)))
}

fn is_csm_group(group: &Value) -> bool {
    group.get("hooks").and_then(Value::as_array).is_some_and(|h| !h.is_empty() && h.iter().all(is_csm_handler))
}

/// Removes csm handlers from a group; returns whether any were removed.
fn strip_csm_handlers_in_group(group: &mut Value) -> bool {
    match group.get_mut("hooks").and_then(Value::as_array_mut) {
        Some(handlers) => {
            let before = handlers.len();
            handlers.retain(|h| !is_csm_handler(h));
            handlers.len() != before
        }
        None => false,
    }
}

/// Removes csm handlers from every group of an event, dropping groups that
/// only held csm handlers. Returns whether anything changed.
fn strip_csm_handlers(groups: &mut Value) -> bool {
    let Some(list) = groups.as_array_mut() else {
        return false;
    };
    let before = list.len();
    list.retain(|g| !is_csm_group(g));
    let mut changed = list.len() != before;
    for group in list.iter_mut() {
        changed |= strip_csm_handlers_in_group(group);
    }
    changed
}

/// Rewrites one TOML table (and its subtables) in place, or appends it, or
/// removes it when `body` is `None`. Everything else is kept line for line.
fn upsert_toml_table(text: &str, table: &str, body: Option<&str>) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let header = format!("[{table}]");
    let sub_prefix = format!("[{table}.");
    let mut out: Vec<String> = Vec::new();
    let mut insert_at = None;
    let mut skipping = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            skipping = trimmed == header || trimmed.starts_with(&sub_prefix);
            if skipping && insert_at.is_none() {
                insert_at = Some(out.len());
            }
        }
        if !skipping {
            out.push(line.to_string());
        }
    }
    if let Some(body) = body {
        let mut block: Vec<String> = std::iter::once(header).chain(body.lines().map(str::to_string)).collect();
        match insert_at {
            Some(i) => {
                if out.get(i).is_some_and(|l| !l.trim().is_empty()) {
                    block.push(String::new());
                }
                out.splice(i..i, block);
            }
            None => {
                if out.last().is_some_and(|l| !l.trim().is_empty()) {
                    out.push(String::new());
                }
                out.extend(block);
            }
        }
    } else if let Some(i) = insert_at {
        // Removing the block can leave two blank lines in a row.
        if i > 0 && i < out.len() && out[i].trim().is_empty() && out[i - 1].trim().is_empty() {
            out.remove(i);
        }
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) && out.len() > 1 {
        out.pop();
    }
    let mut result = out.join(newline);
    if !result.is_empty() {
        result.push_str(newline);
    }
    result
}

/// The edited config must parse, contain (or not) csm's table, and be
/// otherwise identical to the original.
fn verify_toml_edit(original: &toml::Table, edited: &str, want_csm: bool, path: &Path) -> Result<()> {
    let fail = |why: String| Error::Invalid(format!("refusing to edit {}: {why}", slash(path)));
    let edited: toml::Table = toml::from_str(edited).map_err(|e| fail(format!("the edit would not parse ({e})")))?;
    let strip = |t: &toml::Table| {
        let mut t = t.clone();
        if let Some(toml::Value::Table(servers)) = t.get_mut("mcp_servers") {
            servers.remove(MCP_NAME);
            if servers.is_empty() {
                t.remove("mcp_servers");
            }
        }
        t
    };
    let has_csm = edited.get("mcp_servers").and_then(|s| s.get(MCP_NAME)).is_some();
    if has_csm != want_csm {
        return Err(fail(format!(
            "csm's MCP entry is defined in a form setup does not edit (e.g. inline); change [mcp_servers.{MCP_NAME}] by hand"
        )));
    }
    if strip(original) != strip(&edited) {
        return Err(fail("the edit would change other settings".into()));
    }
    Ok(())
}

/// Applies a plan: binary first (then a smoke test of it), then each file
/// with a backup, then the Claude command line. Returns report lines.
pub fn apply(env: &SetupEnv, plan: &Plan) -> Result<Vec<String>> {
    let mut report = Vec::new();
    let backups = env.data_dir.join("backups").join(&env.stamp);
    for step in &plan.steps {
        if let Step::CopyBinary { from, to } = step {
            install_binary(from, to, &env.stamp)?;
            report.push(format!("Installed {}", slash(to)));
        }
    }
    if plan.steps.iter().any(|s| !matches!(s, Step::CopyBinary { .. })) && env.bin().exists() {
        smoke_test(env)?;
        report.push("Checked the installed binary: hooks exit cleanly.".into());
    }
    for step in &plan.steps {
        match step {
            Step::CopyBinary { .. } => {}
            Step::Edit(edit) => {
                // Someone (the agent itself, the user) may have changed the
                // file since it was read; never overwrite that blindly.
                if read_optional(&edit.path)? != edit.before {
                    return Err(Error::Invalid(format!(
                        "{} changed while setup was running; nothing more was written. Run setup again.",
                        slash(&edit.path)
                    )));
                }
                if let Some(before) = &edit.before {
                    backup_config(&backups, edit.backup_name, before.as_bytes())?;
                }
                write_atomic(&edit.path, &edit.after)?;
                report.push(format!("{}: {}", slash(&edit.path), edit.describe));
            }
            Step::ClaudeMcp { cli, config } => {
                // Claude rewrites this file itself. Capture the latest bytes
                // immediately before its first mutating command, not at plan time.
                let before = read_optional(&env.home.join(".claude.json"))?;
                let backup = before
                    .as_ref()
                    .map(|contents| backup_config(&backups, "claude.json", contents.as_bytes()))
                    .transpose()?;
                run_claude_mcp(cli, config.as_ref()).map_err(|error| match &backup {
                    Some(path) => Error::Invalid(format!("{error}. Previous Claude config saved to {}.", slash(path))),
                    None => error,
                })?;
                report.push(match config {
                    Some(_) => "Registered the csm MCP server with Claude Code (user scope).".into(),
                    None => "Removed the csm MCP server from Claude Code.".into(),
                });
            }
        }
    }
    if backups.exists() {
        report.push(format!("Backups of the previous files: {}", slash(&backups)));
    }
    Ok(report)
}

/// `csm setup`: plan, show, confirm, apply.
pub fn run(opts: Options, db: Option<PathBuf>) -> Result<()> {
    let env = SetupEnv::detect(db)?;
    let plan = plan(&env, &opts)?;
    println!("csm setup{}", if opts.uninstall { " --uninstall" } else { "" });
    println!("Database: {}", slash(&env.db));
    for line in &plan.summary {
        println!("{line}");
    }
    for note in &plan.notes {
        println!("Note: {note}");
    }
    if plan.steps.is_empty() {
        println!("\nNothing to change.");
        return Ok(());
    }
    if opts.dry_run {
        println!("\nDry run: nothing was changed.");
        return Ok(());
    }
    if !opts.yes {
        if !std::io::stdin().is_terminal() {
            println!("\nNothing was changed. Run `csm setup --yes` to apply.");
            return Ok(());
        }
        print!("\nApply these changes? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Nothing was changed.");
            return Ok(());
        }
    }
    println!();
    for line in apply(&env, &plan)? {
        println!("{line}");
    }
    if opts.uninstall && opts.only.is_empty() && env.db.exists() {
        crate::store::Store::open(&env.db)?.set_wake_enabled(false)?;
    }
    if !opts.uninstall {
        println!("\nNext:");
        println!("  - Restart Claude Code and Codex so they load the new configuration.");
        if plan.codex_hooks_changed {
            println!("  - Codex asks before running new hooks: in Codex, open /hooks and trust the csm hooks.");
        }
        println!("  - Check the setup any time with `csm setup --status`.");
    } else {
        println!("\nThe binary ({}) and your data ({}) were left in place.", slash(&env.bin()), slash(&env.data_dir));
    }
    Ok(())
}

/// What is installed right now, for `csm setup --status`.
pub fn status(db: Option<PathBuf>) -> Result<Vec<String>> {
    let env = SetupEnv::detect(db)?;
    let mut lines = Vec::new();
    let bin = env.bin();
    lines.push(match (bin.exists(), needs_copy(&env.exe, &bin)?) {
        (false, _) => format!("binary: not installed ({})", slash(&bin)),
        (true, true) => format!("binary: {} (differs from this csm; run `csm setup` to update)", slash(&bin)),
        (true, false) => format!("binary: {}", slash(&bin)),
    });
    for agent in Agent::ALL {
        let (dir, file, specs) = match agent {
            Agent::Claude => (".claude", ".claude/settings.json", CLAUDE_HOOKS),
            Agent::Codex => (".codex", ".codex/hooks.json", CODEX_HOOKS),
        };
        if !env.home.join(dir).is_dir() {
            lines.push(format!("{}: not found", agent.label()));
            continue;
        }
        let path = env.home.join(file);
        let root = parse_json_object(read_optional(&path)?.as_deref(), &path).unwrap_or_default();
        let installed = specs
            .iter()
            .filter(|(event, _, _)| {
                root.get("hooks")
                    .and_then(|h| h.get(*event))
                    .and_then(Value::as_array)
                    .is_some_and(|groups| groups.iter().any(is_csm_group))
            })
            .count();
        let mcp = match agent {
            Agent::Claude => {
                let path = env.home.join(".claude.json");
                parse_json_object(read_optional(&path)?.as_deref(), &path)
                    .ok()
                    .and_then(|r| r.get("mcpServers").and_then(|s| s.get(MCP_NAME)).cloned())
                    .is_some()
            }
            Agent::Codex => read_optional(&env.home.join(".codex").join("config.toml"))?
                .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
                .is_some_and(|t| t.get("mcp_servers").and_then(|s| s.get(MCP_NAME)).is_some()),
        };
        lines.push(format!(
            "{}: hooks {installed}/{} installed, MCP server {}",
            agent.label(),
            specs.len(),
            if mcp { "registered" } else { "not registered" }
        ));
        if agent == Agent::Claude {
            lines.push(format!(
                "  idle wake hook: {}",
                if has_wake_hooks(&root) {
                    "configured; host must load it, and csm wake enable must be opted in"
                } else {
                    "off; install with csm setup --wakeup"
                }
            ));
        }
        lines.push(format!(
            "  automatic messaging: {}",
            if has_messaging_hooks(&root) {
                "configured (host reload and hook trust still required)"
            } else {
                "off; opt in with csm setup --messaging"
            }
        ));
    }
    Ok(lines)
}

fn install_binary(from: &Path, to: &Path, stamp: &str) -> Result<()> {
    let dir = to.parent().ok_or_else(|| Error::Invalid("bad install path".into()))?;
    fs::create_dir_all(dir)?;
    // Leftovers from earlier upgrades, if nothing is running them any more.
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().contains(".old-") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    if to.exists() && fs::remove_file(to).is_err() {
        // A running csm (an MCP server) locks its file on Windows, but it
        // can still be renamed out of the way.
        let aside = to.with_extension(format!("old-{stamp}"));
        fs::rename(to, &aside).map_err(|e| {
            Error::Invalid(format!("cannot replace {} ({e}); close Claude Code and Codex and retry", slash(to)))
        })?;
    }
    fs::copy(from, to)?;
    Ok(())
}

/// Runs the installed binary the way a host would, before any host is
/// pointed at it: a broken binary must never end up in a hook config.
fn smoke_test(env: &SetupEnv) -> Result<()> {
    let bin = env.bin();
    let fail =
        |what: &str| Error::Invalid(format!("the installed binary failed a check ({what}); no config was changed"));
    let version = Command::new(&bin).arg("--version").output().map_err(|_| fail("could not run it"))?;
    if !version.status.success() || !String::from_utf8_lossy(&version.stdout).starts_with("csm ") {
        return Err(fail("--version"));
    }
    let mut child = Command::new(&bin)
        .arg("hook")
        .arg("claude")
        .args(env.db_args())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| fail("could not run it"))?;
    child.stdin.take().map(|mut s| s.write_all(b"{}")).transpose()?;
    let out = child.wait_with_output()?;
    if out.status.code() != Some(0) || !out.stdout.is_empty() {
        return Err(fail("hook"));
    }
    Ok(())
}

fn run_claude_mcp(cli: &Path, config: Option<&Value>) -> Result<()> {
    let run = |args: &[&str]| -> Result<std::process::Output> {
        Command::new(cli)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::Invalid(format!("could not run {}: {e}", slash(cli))))
    };
    // Re-adding fails if the name exists, so remove first; that fails
    // harmlessly when there is nothing to remove.
    let removed = run(&["mcp", "remove", MCP_NAME, "--scope", "user"])?;
    let Some(config) = config else {
        if !removed.status.success() {
            return Err(Error::Invalid(format!(
                "`claude mcp remove` failed: {}",
                String::from_utf8_lossy(&removed.stderr).trim()
            )));
        }
        return Ok(());
    };
    let added = run(&["mcp", "add-json", MCP_NAME, &config.to_string(), "--scope", "user"])?;
    if !added.status.success() {
        return Err(Error::Invalid(format!(
            "`claude mcp add-json` failed: {}",
            String::from_utf8_lossy(&added.stderr).trim()
        )));
    }
    Ok(())
}

fn needs_copy(exe: &Path, bin: &Path) -> Result<bool> {
    if !bin.exists() {
        return Ok(true);
    }
    if fs::canonicalize(exe).ok() == fs::canonicalize(bin).ok() {
        return Ok(false);
    }
    Ok(fs::read(exe)? != fs::read(bin)?)
}

/// Space-containing paths are quoted for the host shell. Other characters
/// with expansion or quoting semantics remain unsupported across cmd/bash/PowerShell.
fn check_command_word(word: &str) -> Result<()> {
    if let Some(c) =
        word.chars().find(|c| c.is_control() || (c.is_whitespace() && *c != ' ') || "'\"`$%&;|()<>^!".contains(*c))
    {
        return Err(Error::Invalid(format!(
            "{word} contains {c:?}, which is not supported in hook command paths; choose a path without shell metacharacters or control characters (spaces are supported)"
        )));
    }
    Ok(())
}

fn quote_hook_word(word: &str, powershell: bool) -> String {
    if !word.contains(' ') {
        return word.to_string();
    }
    let quote = if powershell { '\'' } else { '"' };
    format!("{quote}{word}{quote}")
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn parse_json_object(text: Option<&str>, path: &Path) -> Result<Map<String, Value>> {
    let text = text.unwrap_or("").trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(text) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(Error::Invalid(format!("{} is not a JSON object, so csm will not touch it", slash(path)))),
    }
}

fn render_json(root: &Map<String, Value>, before: Option<&str>) -> String {
    let mut text = serde_json::to_string_pretty(root).expect("JSON values serialize");
    if before.is_none_or(|b| b.ends_with('\n')) {
        text.push('\n');
    }
    if before.is_some_and(|b| b.contains("\r\n")) {
        text = text.replace('\n', "\r\n");
    }
    text
}

/// Keep every snapshot, even when two setup runs share a timestamp.
fn backup_config(dir: &Path, name: &str, contents: &[u8]) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let (stem, extension) = name.rsplit_once('.').unwrap_or((name, ""));
    let mut suffix = 0u64;
    loop {
        let filename = if suffix == 0 {
            name.to_string()
        } else if extension.is_empty() {
            format!("{stem}-{suffix}")
        } else {
            format!("{stem}-{suffix}.{extension}")
        };
        let path = dir.join(filename);
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(contents)?;
                file.sync_all()?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => suffix += 1,
            Err(error) => return Err(error),
        }
    }
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".csm-tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end().to_string()
}

fn events(specs: &[HookSpec]) -> String {
    specs.iter().map(|s| s.0).collect::<Vec<_>>().join(", ")
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(&file)).find(|p| p.is_file())
}

fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "C:/Users/me/.csm/bin/csm.exe hook claude";

    fn user_hook() -> Value {
        json!({ "matcher": "Bash", "hooks": [{ "type": "command", "command": "python lint.py" }] })
    }

    fn csm_events(root: &Map<String, Value>) -> Vec<String> {
        root.get("hooks")
            .and_then(Value::as_object)
            .map(|h| {
                h.iter()
                    .filter(|(_, g)| g.as_array().unwrap().iter().any(is_csm_group))
                    .map(|(e, _)| e.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn adds_one_group_per_event() {
        let mut root = Map::new();
        merge_hooks(&mut root, CLAUDE_HOOKS, CMD).unwrap();
        assert_eq!(csm_events(&root), CLAUDE_HOOKS.iter().map(|h| h.0).collect::<Vec<_>>());
        let post = &root["hooks"]["PostToolUse"][0];
        assert_eq!(post["matcher"], CLAUDE_TOOLS);
        assert_eq!(post["hooks"][0], json!({ "type": "command", "command": CMD, "timeout": 5 }));
        assert!(root["hooks"]["Stop"][0].get("matcher").is_none());
    }

    #[test]
    fn keeps_other_hooks_in_place_and_is_idempotent() {
        let mut root = Map::new();
        root.insert("model".into(), json!("opus"));
        root.insert("hooks".into(), json!({ "PostToolUse": [user_hook()], "Notification": [user_hook()] }));
        merge_hooks(&mut root, CLAUDE_HOOKS, CMD).unwrap();
        assert_eq!(root["hooks"]["PostToolUse"][0], user_hook(), "user hook stays first");
        assert!(is_csm_group(&root["hooks"]["PostToolUse"][1]));
        assert_eq!(root["hooks"]["Notification"], json!([user_hook()]));
        assert_eq!(root.keys().next().unwrap(), "model", "key order preserved");

        let once = root.clone();
        merge_hooks(&mut root, CLAUDE_HOOKS, CMD).unwrap();
        assert_eq!(root, once);

        // A new install path replaces csm's group where it was.
        root["hooks"]["PostToolUse"].as_array_mut().unwrap().push(user_hook());
        merge_hooks(&mut root, CLAUDE_HOOKS, "D:/csm/bin/csm.exe hook claude").unwrap();
        let post = root["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 3);
        assert_eq!(post[1]["hooks"][0]["command"], "D:/csm/bin/csm.exe hook claude");
        assert_eq!(post[2], user_hook());
    }

    #[test]
    fn uninstall_removes_only_csm() {
        let mut root = Map::new();
        root.insert("hooks".into(), json!({ "Stop": [user_hook()] }));
        merge_hooks(&mut root, CLAUDE_HOOKS, CMD).unwrap();
        let mixed =
            json!({ "hooks": [{ "type": "command", "command": "notify.exe" }, { "type": "command", "command": CMD }] });
        root["hooks"]["Notification"] = json!([mixed]);
        merge_hooks(&mut root, &[], CMD).unwrap();
        assert_eq!(
            root["hooks"],
            json!({ "Stop": [user_hook()], "Notification": [{ "hooks": [{ "type": "command", "command": "notify.exe" }] }] })
        );

        let mut only_csm = Map::new();
        merge_hooks(&mut only_csm, CODEX_HOOKS, CMD).unwrap();
        merge_hooks(&mut only_csm, &[], CMD).unwrap();
        assert!(only_csm.is_empty(), "{only_csm:?}");
    }

    #[test]
    fn recognizes_csm_commands_only() {
        let handler = |c: &str| json!({ "type": "command", "command": c });
        for c in [
            "C:/x/.csm/bin/csm.exe hook claude",
            "csm hook codex",
            "/home/me/.csm/bin/csm hook claude --db /x.db",
            r#""C:/Users/José Smith/.csm/bin/csm.exe" hook claude --db "C:/memory data/csm.db""#,
            "& 'C:/Users/José Smith/.csm/bin/csm.exe' hook codex --db 'C:/memory data/csm.db'",
            "C:/simple/csm.exe hook codex --db 'C:/memory data/csm.db'",
        ] {
            assert!(is_csm_handler(&handler(c)), "{c}");
        }
        for c in [
            "python csm_hook.py",
            "mycsm hook claude",
            "csm-helper hook claude",
            "notify.exe",
            "echo C:/x/csm.exe hook claude",
            r#"echo "C:/Users/José Smith/csm.exe" hook claude"#,
            "python C:/x/csm.exe hook codex",
            "csm hook claude && notify.exe",
            "& 'C:/Users/José Smith/csm.exe' hook codex; notify.exe",
            "'C:/Users/José Smith/csm.exe hook codex",
        ] {
            assert!(!is_csm_handler(&handler(c)), "{c}");
        }
    }

    const BODY: &str = "command = \"C:/x/csm.exe\"\nargs = [\"mcp\"]";

    #[test]
    fn toml_table_is_appended_replaced_and_removed_in_place() {
        let original = "model = \"gpt\"\n\n[mcp_servers.node_repl]\ncommand = \"node\"\n\n[features]\nhooks = true\n";
        let added = upsert_toml_table(original, "mcp_servers.csm", Some(BODY));
        assert_eq!(added, format!("{original}\n[mcp_servers.csm]\n{BODY}\n"));
        assert_eq!(upsert_toml_table(&added, "mcp_servers.csm", Some(BODY)), added, "idempotent");

        let with_env =
            "[mcp_servers.csm]\ncommand = \"old\"\n\n[mcp_servers.csm.env]\nA = \"1\"\n\n[features]\nhooks = true\n";
        assert_eq!(
            upsert_toml_table(with_env, "mcp_servers.csm", Some(BODY)),
            format!("[mcp_servers.csm]\n{BODY}\n\n[features]\nhooks = true\n")
        );
        assert_eq!(upsert_toml_table(&added, "mcp_servers.csm", None), original);

        let crlf = original.replace('\n', "\r\n");
        assert!(upsert_toml_table(&crlf, "mcp_servers.csm", Some(BODY)).ends_with("args = [\"mcp\"]\r\n"));
    }

    #[test]
    fn toml_edits_that_would_break_the_config_are_refused() {
        let inline = "[mcp_servers]\ncsm = { command = \"x\" }\n";
        let original: toml::Table = toml::from_str(inline).unwrap();
        let edited = upsert_toml_table(inline, "mcp_servers.csm", Some(BODY));
        assert!(verify_toml_edit(&original, &edited, true, Path::new("config.toml")).is_err());

        let fine = "a = 1\n";
        let original: toml::Table = toml::from_str(fine).unwrap();
        let edited = upsert_toml_table(fine, "mcp_servers.csm", Some(BODY));
        assert!(verify_toml_edit(&original, &edited, true, Path::new("config.toml")).is_ok());
        assert!(
            verify_toml_edit(&original, "a = 2\n[mcp_servers.csm]\ncommand = \"x\"\n", true, Path::new("c")).is_err()
        );
    }

    #[test]
    fn command_paths_must_survive_every_shell() {
        for good in ["C:/Users/Donovan/.csm/bin/csm.exe", "C:/Users/José Smith 日本語/.csm/bin/csm.exe"] {
            assert!(check_command_word(good).is_ok(), "{good}");
        }
        for bad in [
            "C:/a$b/csm.exe",
            "C:/50%/csm.exe",
            "C:/a&b/csm.exe",
            "C:/a'b/csm.exe",
            "C:/a\"b/csm.exe",
            "C:/a!b/csm.exe",
            "C:/a`b/csm.exe",
            "C:/a\nb/csm.exe",
            "C:/a\tb/csm.exe",
        ] {
            assert!(check_command_word(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hook_commands_preserve_simple_paths_and_quote_only_when_needed() {
        let mut env = SetupEnv {
            home: PathBuf::from("C:/Users/example"),
            data_dir: PathBuf::from("C:/Users/example/.csm"),
            db: PathBuf::from("C:/Users/example/.csm/csm.db"),
            exe: PathBuf::new(),
            claude_cli: None,
            stamp: "unused".into(),
        };
        for agent in Agent::ALL {
            assert_eq!(env.hook_command(agent), format!("{} hook {}", slash(&env.bin()), agent.host()));
        }
        env.db = PathBuf::from("C:/memory data/csm.db");
        assert_eq!(
            env.hook_command(Agent::Claude),
            format!("{} hook claude --db \"C:/memory data/csm.db\"", slash(&env.bin()))
        );
        env.data_dir = PathBuf::from("C:/Users/José Smith/.csm");
        let codex = env.hook_command(Agent::Codex);
        let expected = if cfg!(windows) {
            format!("& '{}' hook codex --db 'C:/memory data/csm.db'", slash(&env.bin()))
        } else {
            format!("\"{}\" hook codex --db \"C:/memory data/csm.db\"", slash(&env.bin()))
        };
        assert_eq!(codex, expected);
        assert!(is_csm_handler(&json!({ "command": codex })));
    }

    fn fake_env(name: &str) -> (SetupEnv, PathBuf) {
        let root = std::env::temp_dir().join(format!("csm-setup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        let exe = root.join("csm-build.exe");
        fs::write(&exe, b"pretend binary").unwrap();
        let env = SetupEnv {
            data_dir: home.join(".csm"),
            db: home.join(".csm").join("csm.db"),
            home,
            exe,
            claude_cli: None,
            stamp: "stamp".into(),
        };
        (env, root)
    }

    fn apply_edits(env: &SetupEnv, plan: &Plan) {
        // The pretend binary cannot run, so apply the file edits directly.
        for step in &plan.steps {
            match step {
                Step::Edit(edit) => write_atomic(&edit.path, &edit.after).unwrap(),
                Step::CopyBinary { from, to } => install_binary(from, to, &env.stamp).unwrap(),
                Step::ClaudeMcp { .. } => unreachable!("no CLI in tests"),
            }
        }
    }

    #[test]
    fn plans_install_rerun_and_uninstall_against_a_fake_home() {
        let (env, root) = fake_env("plan");
        let settings = env.home.join(".claude").join("settings.json");
        fs::write(&settings, "{\n  \"model\": \"opus\"\n}\n").unwrap();
        let config = env.home.join(".codex").join("config.toml");
        let config_text = "model = \"gpt-5\"\n\n[mcp_servers.node_repl]\ncommand = \"node\"\n";
        fs::write(&config, config_text).unwrap();
        fs::write(env.home.join(".claude.json"), "{\"numStartups\": 3}").unwrap();

        let plan1 = plan(&env, &Options::default()).unwrap();
        assert_eq!(plan1.steps.len(), 5, "{:#?}", plan1.summary);
        apply_edits(&env, &plan1);
        assert!(env.bin().exists());
        let s: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(s["model"], "opus");
        assert_eq!(s["hooks"]["SessionStart"][0]["hooks"][0]["command"], format!("{} hook claude", slash(&env.bin())));
        let hooks: Value =
            serde_json::from_str(&fs::read_to_string(env.home.join(".codex/hooks.json")).unwrap()).unwrap();
        assert_eq!(hooks["hooks"]["PostToolUse"][0]["matcher"], "^(Bash|apply_patch)$");
        let toml_text = fs::read_to_string(&config).unwrap();
        assert!(toml_text.starts_with(config_text), "{toml_text}");
        assert!(toml_text.contains("[mcp_servers.csm]"));
        let claude_json: Value =
            serde_json::from_str(&fs::read_to_string(env.home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(claude_json["numStartups"], 3);
        assert_eq!(claude_json["mcpServers"]["csm"]["args"], json!(["mcp"]));
        assert!(plan1.codex_hooks_changed);

        let plan2 = plan(&env, &Options::default()).unwrap();
        assert!(plan2.steps.is_empty(), "re-run should change nothing: {:#?}", plan2.summary);

        let plan3 = plan(&env, &Options { uninstall: true, ..Default::default() }).unwrap();
        apply_edits(&env, &plan3);
        let s: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(s, json!({ "model": "opus" }));
        assert_eq!(fs::read_to_string(&config).unwrap(), config_text);
        let claude_json: Value =
            serde_json::from_str(&fs::read_to_string(env.home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(claude_json["mcpServers"], json!({}));
        assert!(plan(&env, &Options { uninstall: true, ..Default::default() }).unwrap().steps.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_custom_database_location_is_passed_to_hooks_and_server() {
        let (mut env, root) = fake_env("db");
        env.data_dir = root.join("csmdata");
        env.db = root.join("csmdata").join("csm.db");
        let plan = plan(&env, &Options { only: vec![Agent::Codex], ..Default::default() }).unwrap();
        let edit = plan.steps.iter().find_map(|s| match s {
            Step::Edit(e) if e.path.ends_with("hooks.json") => Some(e),
            _ => None,
        });
        assert!(edit.unwrap().after.contains(&format!("hook codex --db {}", slash(&env.db))));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn claude_cli_backup_uses_latest_file_and_preserves_same_stamp_snapshots() {
        let (mut env, root) = fake_env("claude-cli-backup");
        env.claude_cli = Some(root.join("missing-claude-cli"));
        let config = env.home.join(".claude.json");
        fs::write(&config, "{\"numStartups\": 1}").unwrap();
        let mut changes = plan(&env, &Options { only: vec![Agent::Claude], ..Default::default() }).unwrap();
        changes.steps.retain(|step| matches!(step, Step::ClaudeMcp { .. }));
        assert_eq!(changes.steps.len(), 1);

        // Claude can rewrite its own config after setup has planned the change.
        let latest = "{\r\n  \"numStartups\": 2, \"label\": \"caf\u{e9}\"\r\n}\r\n";
        fs::write(&config, latest).unwrap();
        let failed = apply(&env, &changes).unwrap_err().to_string();
        let first = env.data_dir.join("backups").join(&env.stamp).join("claude.json");
        assert_eq!(fs::read(&first).unwrap(), latest.as_bytes());
        assert!(failed.contains(&slash(&first)), "{failed}");
        assert_eq!(fs::read_to_string(&config).unwrap(), latest);

        // The timestamp has second precision; another run must not replace it.
        let newer = "{\"numStartups\": 3}";
        fs::write(&config, newer).unwrap();
        let failed_again = apply(&env, &changes).unwrap_err().to_string();
        let second = first.with_file_name("claude-1.json");
        assert_eq!(fs::read(&first).unwrap(), latest.as_bytes());
        assert_eq!(fs::read(&second).unwrap(), newer.as_bytes());
        assert!(failed_again.contains(&slash(&second)), "{failed_again}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refuses_configs_it_cannot_read_safely() {
        let (env, root) = fake_env("broken");
        fs::write(env.home.join(".codex").join("config.toml"), "this is = = not toml").unwrap();
        assert!(plan(&env, &Options { only: vec![Agent::Codex], ..Default::default() }).is_err());
        fs::write(env.home.join(".claude").join("settings.json"), "{ not json").unwrap();
        assert!(plan(&env, &Options { only: vec![Agent::Claude], ..Default::default() }).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
