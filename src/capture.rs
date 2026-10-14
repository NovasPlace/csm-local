//! Turns host hook payloads into journal entries. Pure functions, no I/O.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::redact::redact;
use crate::text::{one_line, tail, truncate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Claude,
    Codex,
}

impl Host {
    pub fn parse(name: &str) -> Option<Host> {
        match name {
            "claude" => Some(Host::Claude),
            "codex" => Some(Host::Codex),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Host::Claude => "claude",
            Host::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolEvent {
    pub kind: &'static str,
    pub tool: String,
    pub summary: String,
    pub files: Vec<String>,
    /// `None` when the host does not report the outcome (Codex shell commands).
    pub ok: Option<bool>,
    pub detail: Option<String>,
}

const MAX_COMMAND_BYTES: usize = 200;
const FAILURE_TAIL_BYTES: usize = 600;
const MAX_SUMMARY_FILES: usize = 3;

/// A test runner in command position: at the start of a line, or after
/// `;`, `&`, `|` or `(`, optionally behind env assignments and wrappers
/// such as `time`, `timeout 600`, `npx` or `cmd /c`.
static TEST_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)(?:^|[;&|(])\s*(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*(?:(?:time|sudo|nice|nohup|xvfb-run|env(?:\s+\w+=\S*)*|timeout\s+\S+|cross-env(?:\s+\w+=\S*)*|dotenv\s+--|cmd(?:\.exe)?\s+/c|npx|bunx|uv\s+run|poetry\s+run|pipenv\s+run|hatch\s+run|(?:yarn|pnpm|bun)(?:\s+(?:x|exec|dlx))?)\s+)*(?:cargo\s+(?:\+\S+\s+)?(?:test|nextest)|(?:npm|pnpm|yarn|bun)\s+(?:run\s+)?test(?::\S*)?|vitest|jest|playwright\s+test|pytest|python3?\s+-m\s+(?:pytest|unittest)|go\s+test|dotnet\s+test|mvn\s+(?:\S+\s+)*test|\.?[/\\]?gradlew?(?:\.bat)?\s+(?:\S+\s+)*test|rspec|phpunit|ctest|tsx\s+--test|node\s+--test|deno\s+test|mix\s+test|swift\s+test|make\s+(?:-\S+\s+)*(?:check|test))(?:$|[\s;&|)])",
    )
    .unwrap()
});

static EXIT_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?mi)^\s*(?:exit code|process exited with code)\s*:?\s*(-?\d+)").unwrap());

/// Splits a shell command line into its simple commands.
static COMMAND_SEPARATOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&&|\|\||[;|&\n]").unwrap());

/// Commands that only look at things. Codex has no file-reading tool, so a
/// third of its shell calls are reads like these; they are not work.
const READ_ONLY_COMMANDS: &[&str] = &[
    "cat",
    "type",
    "gc",
    "get-content",
    "head",
    "tail",
    "less",
    "more",
    "wc",
    "rg",
    "grep",
    "egrep",
    "fgrep",
    "findstr",
    "select-string",
    "sls",
    "ls",
    "dir",
    "gci",
    "get-childitem",
    "get-item",
    "gi",
    "tree",
    "find",
    "pwd",
    "get-location",
    "cd",
    "set-location",
    "pushd",
    "popd",
    "echo",
    "write-output",
    "write-host",
    "start-sleep",
    "sleep",
    "which",
    "where",
    "where.exe",
    "get-command",
    "test-path",
    "resolve-path",
    "measure-object",
    "select-object",
    "sort-object",
    "format-table",
    "format-list",
    "out-string",
    "get-date",
    "whoami",
    "hostname",
];
const READ_ONLY_GIT: &[&str] = &["status", "diff", "log", "show", "rev-parse", "ls-files", "blame", "describe", "grep"];
/// Git subcommands that only list when given nothing but these arguments.
const LISTING_GIT: &[(&str, &[&str])] = &[
    ("branch", &["-a", "-r", "-v", "-vv", "-l", "--list", "--all", "--remotes", "--verbose", "--show-current"]),
    ("remote", &["-v", "--verbose", "show", "get-url", "origin", "upstream"]),
    ("tag", &["-l", "--list", "-n"]),
];

/// Classifies a PostToolUse / PostToolUseFailure payload. Returns `None` for
/// tools not worth journaling: reads, searches, web tools and csm's own tools.
///
/// `roots` are the project key followed by any other spellings of the
/// project directory (e.g. the raw cwd), used to make paths relative.
pub fn tool_event(host: Host, payload: &Value, roots: &[String], failed: bool) -> Option<ToolEvent> {
    let tool = payload.get("tool_name")?.as_str()?;
    if is_own_tool(tool) {
        return None;
    }
    let project = roots.first()?;
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    let cwd = payload.get("cwd").and_then(Value::as_str).unwrap_or(project);
    let mut event = match tool {
        "Edit" | "MultiEdit" => file_event("edit", "Edit", input, "file_path", roots, cwd)?,
        "Write" => file_event("write", "Write", input, "file_path", roots, cwd)?,
        "NotebookEdit" => file_event("edit", "Edit", input, "notebook_path", roots, cwd)?,
        "Bash" | "PowerShell" | "shell" | "exec_command" | "local_shell" => command_event(input)?,
        "apply_patch" => patch_event(input, roots, cwd)?,
        _ => return None,
    };
    event.tool = tool.to_string();

    let response = payload.get("tool_response").unwrap_or(&Value::Null);
    if failed {
        if payload.get("is_interrupt").and_then(Value::as_bool).unwrap_or(false) {
            // Stopped by the user: says nothing about whether it works.
            event.ok = None;
            event.detail = Some("interrupted".into());
        } else {
            event.ok = Some(false);
            let error = payload.get("error").map(value_text).unwrap_or_default();
            event.detail = non_empty(tail(redact(error.trim()).text.trim(), FAILURE_TAIL_BYTES));
        }
    } else if host == Host::Codex && matches!(event.kind, "command" | "test") {
        // Codex reports only the command's output; the exit code is usually
        // not in it, so the outcome is unknown unless it says otherwise.
        let output = value_text(response);
        event.ok = exit_code(&output).map(|code| code == 0);
        if event.ok == Some(false) {
            event.detail = non_empty(tail(redact(output.trim()).text.trim(), FAILURE_TAIL_BYTES));
        }
    } else if host == Host::Claude {
        let backgrounded = response.get("backgroundTaskId").and_then(Value::as_str).is_some_and(|id| !id.is_empty());
        let interrupted = response.get("interrupted").and_then(Value::as_bool).unwrap_or(false);
        if backgrounded {
            // Still running; its result never reaches a hook.
            event.summary.push_str(" (background)");
        } else if !interrupted {
            // Claude routes failed tool calls to PostToolUseFailure.
            event.ok = Some(true);
        }
    }
    Some(event)
}

/// csm's own MCP tools, under any server alias that contains "csm".
pub fn is_own_tool(tool: &str) -> bool {
    tool.starts_with("mcp__csm__") || (tool.starts_with("mcp__") && tool.contains("_csm__"))
}

fn file_event(
    kind: &'static str,
    verb: &str,
    input: &Value,
    key: &str,
    roots: &[String],
    cwd: &str,
) -> Option<ToolEvent> {
    let path = input.get(key)?.as_str()?;
    let rel = rel_path(roots, cwd, path);
    Some(ToolEvent {
        kind,
        tool: String::new(),
        summary: format!("{verb} {rel}"),
        files: vec![rel],
        ok: None,
        detail: None,
    })
}

fn command_event(input: &Value) -> Option<ToolEvent> {
    let command = match input.get("command").or_else(|| input.get("cmd"))? {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
        _ => return None,
    };
    let flat = one_line(&redact(&command).text);
    if flat.is_empty() || is_read_only(&command) {
        return None;
    }
    let kind = if TEST_COMMAND.is_match(&command) { "test" } else { "command" };
    Some(ToolEvent {
        kind,
        tool: String::new(),
        summary: format!("Run: {}", truncate(&flat, MAX_COMMAND_BYTES)),
        files: Vec::new(),
        ok: None,
        detail: None,
    })
}

/// True when every simple command in the line only reads or inspects.
fn is_read_only(command: &str) -> bool {
    COMMAND_SEPARATOR.split(command).map(str::trim).filter(|s| !s.is_empty()).all(|segment| {
        // Redirection, method calls and find actions all change things.
        if segment.contains('>')
            || segment.contains(").")
            || segment.contains("::")
            || ["-delete", "-exec", "-ok", "-fprint", "-fls"].iter().any(|a| segment.contains(a))
        {
            return false;
        }
        let words: Vec<&str> = segment.split_whitespace().collect();
        let first = words.first().unwrap_or(&"").to_ascii_lowercase();
        let first = first.rsplit(['/', '\\']).next().unwrap_or(&first).trim_end_matches(".exe").to_string();
        match first.as_str() {
            "git" => {
                let rest: Vec<&str> = words[1..].iter().copied().skip_while(|w| w.starts_with('-')).collect();
                let Some((sub, args)) = rest.split_first() else {
                    return false;
                };
                READ_ONLY_GIT.contains(sub)
                    || LISTING_GIT.iter().any(|(name, allowed)| sub == name && args.iter().all(|a| allowed.contains(a)))
            }
            // `-i`, `-Ei`, `--in-place` edit files.
            "sed" => !words[1..].iter().any(|w| {
                w.starts_with("--in-place") || (w.starts_with('-') && !w.starts_with("--") && w.contains('i'))
            }),
            name => READ_ONLY_COMMANDS.contains(&name),
        }
    })
}

fn patch_event(input: &Value, roots: &[String], cwd: &str) -> Option<ToolEvent> {
    let patch = match input {
        Value::String(s) => s.as_str(),
        _ => ["command", "patch", "input"].iter().find_map(|k| input.get(*k)?.as_str())?,
    };
    let ops = patch_files(patch);
    let files: Vec<String> = ops.iter().map(|(_, f)| rel_path(roots, cwd, f)).collect();
    let all = |op: PatchOp| !ops.is_empty() && ops.iter().all(|(o, _)| *o == op);
    let (kind, verb) = if all(PatchOp::Add) {
        ("write", "Write")
    } else if all(PatchOp::Delete) {
        ("delete", "Delete")
    } else {
        ("edit", "Edit")
    };
    let shown: Vec<&str> = files.iter().take(MAX_SUMMARY_FILES).map(String::as_str).collect();
    let mut summary = if shown.is_empty() { "Apply patch".to_string() } else { format!("{verb} {}", shown.join(", ")) };
    if files.len() > MAX_SUMMARY_FILES {
        summary.push_str(&format!(" (+{} more)", files.len() - MAX_SUMMARY_FILES));
    }
    Some(ToolEvent { kind, tool: String::new(), summary, files, ok: None, detail: None })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchOp {
    Add,
    Update,
    Delete,
}

/// Files named in a Codex `apply_patch` envelope, in order, deduplicated.
fn patch_files(patch: &str) -> Vec<(PatchOp, String)> {
    let mut out: Vec<(PatchOp, String)> = Vec::new();
    for line in patch.lines() {
        let line = line.trim_end();
        let entry = if let Some(f) = line.strip_prefix("*** Add File: ") {
            (PatchOp::Add, f)
        } else if let Some(f) = line.strip_prefix("*** Update File: ") {
            (PatchOp::Update, f)
        } else if let Some(f) = line.strip_prefix("*** Delete File: ") {
            (PatchOp::Delete, f)
        } else if let Some(f) = line.strip_prefix("*** Move to: ") {
            (PatchOp::Update, f)
        } else {
            continue;
        };
        let file = entry.1.trim().to_string();
        if !file.is_empty() && !out.iter().any(|(_, f)| *f == file) {
            out.push((entry.0, file));
        }
    }
    out
}

/// A path relative to the first of `roots` that contains it, with forward
/// slashes, or the normalized absolute path when it lies outside them all.
pub fn rel_path(roots: &[String], cwd: &str, path: &str) -> String {
    let path = normalize(path.trim());
    let abs = if is_absolute(&path) { path } else { format!("{}/{}", normalize(cwd).trim_end_matches('/'), path) };
    let abs = abs.replace("/./", "/");
    for root in roots {
        let root = format!("{}/", normalize(root).trim_end_matches('/'));
        let matches = if cfg!(windows) {
            abs.len() >= root.len() && abs.is_char_boundary(root.len()) && abs[..root.len()].eq_ignore_ascii_case(&root)
        } else {
            abs.starts_with(&root)
        };
        if matches {
            return abs[root.len()..].to_string();
        }
    }
    abs
}

/// Forward slashes, no verbatim prefix (`\\?\C:\x` -> `C:/x`,
/// `\\?\UNC\srv\share` -> `//srv/share`), no leading `./`.
pub fn normalize(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = match p.strip_prefix("//?/UNC/") {
        Some(rest) => format!("//{rest}"),
        None => p.strip_prefix("//?/").unwrap_or(&p).to_string(),
    };
    p.strip_prefix("./").unwrap_or(&p).to_string()
}

fn is_absolute(path: &str) -> bool {
    path.starts_with('/') || path.as_bytes().get(1) == Some(&b':')
}

fn exit_code(output: &str) -> Option<i64> {
    EXIT_CODE.captures(output)?.get(1)?.as_str().parse().ok()
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Object(map) => {
            // Claude shell tools respond with {stdout, stderr, ...}.
            let parts: Vec<&str> = ["stdout", "stderr", "output", "error"]
                .iter()
                .filter_map(|k| map.get(*k)?.as_str())
                .filter(|s| !s.is_empty())
                .collect();
            if parts.is_empty() {
                v.to_string()
            } else {
                parts.join("\n")
            }
        }
        other => other.to_string(),
    }
}

fn non_empty(s: String) -> Option<String> {
    (!s.trim().is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const P: &str = "C:/work/app";

    fn roots() -> Vec<String> {
        vec![P.to_string()]
    }

    fn claude(tool: &str, input: Value) -> Value {
        json!({ "hook_event_name": "PostToolUse", "session_id": "s", "cwd": r"C:\work\app",
                "tool_name": tool, "tool_input": input, "tool_response": { "stdout": "ok" }, "tool_use_id": "toolu_1" })
    }

    fn codex(tool: &str, command: &str, response: &str) -> Value {
        json!({ "hook_event_name": "PostToolUse", "session_id": "s", "cwd": "C:/work/app/sub", "turn_id": "t1",
                "tool_name": tool, "tool_input": { "command": command }, "tool_response": response, "tool_use_id": "call_1" })
    }

    fn event(host: Host, payload: &Value, failed: bool) -> Option<ToolEvent> {
        tool_event(host, payload, &roots(), failed)
    }

    #[test]
    fn claude_edits_become_relative_paths() {
        let e =
            event(Host::Claude, &claude("Edit", json!({ "file_path": r"C:\work\app\src\main.rs" })), false).unwrap();
        assert_eq!((e.kind, e.summary.as_str(), e.ok), ("edit", "Edit src/main.rs", Some(true)));
        assert_eq!(e.files, ["src/main.rs"]);

        let w = event(Host::Claude, &claude("Write", json!({ "file_path": "c:/work/app/README.md" })), false).unwrap();
        assert_eq!(w.kind, "write");
        if cfg!(windows) {
            assert_eq!(w.files, ["README.md"]);
        }

        let outside = event(Host::Claude, &claude("Edit", json!({ "file_path": r"D:\other\x.rs" })), false).unwrap();
        assert_eq!(outside.files, ["D:/other/x.rs"]);

        let nb = event(Host::Claude, &claude("NotebookEdit", json!({ "notebook_path": "nb/a.ipynb" })), false).unwrap();
        assert_eq!(nb.files, ["nb/a.ipynb"]);
    }

    #[test]
    fn classifies_commands_and_tests() {
        let run = event(Host::Claude, &claude("Bash", json!({ "command": "git commit\n  -am wip" })), false).unwrap();
        assert_eq!((run.kind, run.summary.as_str()), ("command", "Run: git commit -am wip"));

        for cmd in [
            "cargo test --release",
            "npm test",
            "npm run test -- -w",
            "npm run test:unit",
            "yarn test:ci",
            "cd x && pytest -q",
            "go test ./...",
            "npx vitest run",
            "./gradlew test",
            "make -j8 test",
            "RUST_BACKTRACE=1 cargo test",
            "cargo build; cargo test",
            "time cargo test",
            "timeout 600 cargo test",
            "yarn jest",
            "pnpm vitest run",
            "sudo pytest",
            "env CI=1 pytest",
            "pipenv run pytest",
            "cmd /c npm test",
            "cd app\nnpm test",
        ] {
            let e = event(Host::Claude, &claude("PowerShell", json!({ "command": cmd })), false).unwrap();
            assert_eq!(e.kind, "test", "{cmd}");
        }
        for cmd in [
            "cargo build",
            "echo jest-config > cfg.txt",
            "git commit -m 'add tests'",
            "pip install pytest",
            "git commit -m \"bump jest to 30\"",
            "npm i -D vitest",
        ] {
            let e = event(Host::Claude, &claude("Bash", json!({ "command": cmd })), false).unwrap();
            assert_eq!(e.kind, "command", "{cmd}");
        }
    }

    #[test]
    fn read_only_commands_are_not_journaled() {
        for cmd in [
            "Get-Content src/main.rs | Select-Object -Skip 240 -First 20",
            "rg -n json src",
            "git status --short",
            "git diff --stat; git log -3",
            "cd sub && ls",
            "Start-Sleep -Seconds 5",
            "sed -n 1,20p a.rs",
            "C:/tools/rg.exe foo",
            "git branch",
            "git branch -a",
            "git remote -v",
        ] {
            assert!(event(Host::Codex, &codex("Bash", cmd, ""), false).is_none(), "{cmd}");
        }
        for cmd in [
            "git commit -am x",
            "echo hi > out.txt",
            "find . -name '*.tmp' -delete",
            "sed -i s/a/b/ f",
            "sed -Ei s/a/b/ f",
            "sed --in-place s/a/b/ f",
            "cat a | node x.js",
            "git branch -D feature/parser",
            "git branch -m main trunk",
            "git remote set-url origin git@example.com:x.git",
            "git remote add upstream https://example.com/x.git",
            "(Get-Item x.txt).Delete()",
            "Get-Content x | Set-Content y",
            "& .\\script.ps1",
            "ls & npm install",
            "type nul > x",
        ] {
            assert!(event(Host::Codex, &codex("Bash", cmd, ""), false).is_some(), "{cmd}");
        }
    }

    #[test]
    fn claude_failures_keep_the_redacted_error_tail() {
        let mut payload = claude("Bash", json!({ "command": "cargo test" }));
        payload["error"] = json!(format!("{}\ntest result: FAILED. 1 failed", "noise ".repeat(500)));
        let e = event(Host::Claude, &payload, true).unwrap();
        assert_eq!(e.ok, Some(false));
        let detail = e.detail.unwrap();
        assert!(detail.ends_with("1 failed"), "{detail}");
        assert!(detail.len() <= FAILURE_TAIL_BYTES);

        let key: Vec<String> = (0..30).map(|i| format!("MIIEowIBAAKCAQEA{i:04}xk9QzLmNoPqRsTuVwXyZ01234567")).collect();
        payload["error"] = json!(format!(
            "could not parse key:\n-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----\nerror: bad key",
            key.join("\n")
        ));
        let e = event(Host::Claude, &payload, true).unwrap();
        let detail = e.detail.unwrap();
        assert!(!detail.contains("MIIEow"), "key material survived the tail: {detail}");
        assert!(detail.contains("error: bad key"));
    }

    #[test]
    fn interrupts_and_background_runs_have_unknown_outcome() {
        let mut payload = claude("Bash", json!({ "command": "npm run dev" }));
        payload["is_interrupt"] = json!(true);
        let e = event(Host::Claude, &payload, true).unwrap();
        assert_eq!((e.ok, e.detail.as_deref()), (None, Some("interrupted")));

        let mut bg = claude("Bash", json!({ "command": "cargo test" }));
        bg["tool_response"] = json!({ "stdout": "", "stderr": "", "interrupted": false, "backgroundTaskId": "bash_1" });
        let e = event(Host::Claude, &bg, false).unwrap();
        assert_eq!((e.ok, e.summary.as_str()), (None, "Run: cargo test (background)"));

        bg["tool_response"] = json!({ "stdout": "", "interrupted": true });
        assert_eq!(event(Host::Claude, &bg, false).unwrap().ok, None);
    }

    #[test]
    fn commands_are_redacted_before_being_cut() {
        let padding = "x".repeat(160);
        let cmd = format!("curl {padding} -H 'Authorization: token ghp_R4nd0mT0k3nV4lu3Abcdefghij0123456789'");
        let e = event(Host::Claude, &claude("Bash", json!({ "command": cmd })), false).unwrap();
        assert!(!e.summary.contains("ghp_R4nd0m"), "{}", e.summary);
    }

    #[test]
    fn codex_commands_have_unknown_outcome_unless_reported() {
        let e = event(Host::Codex, &codex("Bash", "cargo build", "Compiling csm v0.1.0"), false).unwrap();
        assert_eq!((e.kind, e.ok, e.detail.as_deref()), ("command", None, None));

        let failed =
            event(Host::Codex, &codex("Bash", "cargo test", "Exit code: 101\nerror: test failed"), false).unwrap();
        assert_eq!((failed.kind, failed.ok), ("test", Some(false)));
        assert!(failed.detail.unwrap().contains("test failed"));

        let passed = event(Host::Codex, &codex("Bash", "cargo test", "Exit code: 0\nok"), false).unwrap();
        assert_eq!((passed.ok, passed.detail), (Some(true), None));

        let argv = json!({ "tool_name": "shell", "tool_input": { "command": ["cargo", "build"] } });
        assert_eq!(event(Host::Codex, &argv, false).unwrap().summary, "Run: cargo build");
    }

    #[test]
    fn codex_patches_list_their_files() {
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n*** Add File: src/new.rs\n+x\n*** Delete File: old.rs\n*** Update File: src/a.rs\n*** Move to: src/b.rs\n*** End Patch";
        let e = event(Host::Codex, &codex("apply_patch", patch, "Success"), false).unwrap();
        assert_eq!(e.kind, "edit");
        assert_eq!(e.files, ["sub/src/lib.rs", "sub/src/new.rs", "sub/old.rs", "sub/src/a.rs", "sub/src/b.rs"]);
        assert_eq!(e.summary, "Edit sub/src/lib.rs, sub/src/new.rs, sub/old.rs (+2 more)");

        let add = event(Host::Codex, &codex("apply_patch", "*** Add File: a.txt\n+hi", ""), false).unwrap();
        assert_eq!((add.kind, add.summary.as_str()), ("write", "Write sub/a.txt"));
        let del = event(Host::Codex, &codex("apply_patch", "*** Delete File: C:/work/app/a.txt", ""), false).unwrap();
        assert_eq!((del.kind, del.summary.as_str()), ("delete", "Delete a.txt"));
    }

    #[test]
    fn ignores_reads_unknown_and_own_tools() {
        for tool in ["Read", "Grep", "Glob", "WebFetch", "Task", "mcp__csm__csm_save", "mcp__plugin_x_csm__csm_search"]
        {
            assert!(event(Host::Claude, &claude(tool, json!({ "file_path": "a" })), false).is_none(), "{tool}");
        }
        assert!(event(Host::Claude, &claude("Edit", json!({})), false).is_none());
        assert!(event(Host::Claude, &claude("Bash", json!({ "command": "   " })), false).is_none());
        assert!(event(Host::Claude, &json!({}), false).is_none());
    }

    #[test]
    fn rel_path_handles_separators_dots_and_alternate_roots() {
        let r = roots();
        assert_eq!(rel_path(&r, P, "./src/x.rs"), "src/x.rs");
        assert_eq!(rel_path(&r, "C:/work/app/a", "b/./c.rs"), "a/b/c.rs");
        assert_eq!(rel_path(&r, P, r"\\?\C:\work\app\x.rs"), "x.rs");
        assert_eq!(rel_path(&r, P, "C:/work/apple/x.rs"), "C:/work/apple/x.rs");
        assert_eq!(rel_path(&["/home/me/app".into()], "/home/me/app", "/home/me/app/src/m.rs"), "src/m.rs");

        let unc = vec!["//srv/share/repo".to_string(), "Z:/repo".to_string()];
        assert_eq!(rel_path(&unc, "Z:/repo", r"Z:\repo\src\m.rs"), "src/m.rs");
        assert_eq!(rel_path(&unc, "Z:/repo", r"\\srv\share\repo\src\m.rs"), "src/m.rs");
        assert_eq!(normalize(r"\\?\UNC\srv\share\repo"), "//srv/share/repo");
    }
}
