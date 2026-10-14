use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use csm_local::brief::{self, BriefRequest};
use csm_local::journal::{Checkpoint, EventQuery, EVENT_KINDS, MAX_HANDOFF_BYTES};
use csm_local::mcp::Server;
use csm_local::store::{Filter, NewMemory, Store};
use csm_local::{hook, paths, project, Error, Result};

#[derive(Parser)]
#[command(name = "csm", version, about = "Local-first, private cross-session memory for coding agents")]
struct Cli {
    /// Database file. Defaults to $CSM_DB, or csm.db in $CSM_HOME or ~/.csm.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Save a memory for the current project
    Save {
        content: String,
        /// note, lesson, decision or fact
        #[arg(long, default_value = "note")]
        kind: String,
        /// Comma-separated tags
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        /// 1 (trivia) to 5 (critical)
        #[arg(long, default_value_t = 3)]
        importance: i64,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Search memories by keywords
    Search {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        kind: Option<String>,
        /// Search every project
        #[arg(long)]
        all: bool,
        #[arg(long)]
        project: Option<PathBuf>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// List recent memories, newest first
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        kind: Option<String>,
        /// List every project
        #[arg(long)]
        all: bool,
        #[arg(long)]
        project: Option<PathBuf>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Permanently delete one memory by id
    Delete { id: i64 },
    /// Show the briefing an agent receives at session start
    Brief {
        #[arg(long)]
        project: Option<PathBuf>,
        /// Size limit in bytes
        #[arg(long, default_value_t = brief::DEFAULT_BUDGET)]
        budget: usize,
        /// Also list the sources used and anything trimmed to fit
        #[arg(long)]
        explain: bool,
    },
    /// Show the session journal recorded by hooks, newest first
    Journal {
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// Only events of this kind
        #[arg(long)]
        kind: Option<String>,
        /// Only this session
        #[arg(long)]
        session: Option<String>,
        /// Every project
        #[arg(long)]
        all: bool,
        #[arg(long)]
        project: Option<PathBuf>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Record where the work stands, for the next session
    Checkpoint {
        summary: String,
        /// A next step (repeatable)
        #[arg(long)]
        next: Vec<String>,
        /// A blocker (repeatable)
        #[arg(long)]
        blocker: Vec<String>,
        /// Set the project's goal
        #[arg(long)]
        goal: Option<String>,
        /// Mark the current goal done
        #[arg(long)]
        goal_done: bool,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Show database location and counts
    Status {
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Write memories and journal events as JSON to stdout (current project unless --all)
    Export {
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        all: bool,
    },
    /// Permanently delete every memory and journal event for a project
    Forget {
        #[arg(long)]
        project: PathBuf,
        /// Actually delete; without this only shows what would be deleted
        #[arg(long)]
        yes: bool,
    },
    /// Run the MCP server on stdin/stdout
    Mcp {
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Handle a Claude Code or Codex hook event (JSON on stdin); always exits 0
    Hook {
        /// claude or codex
        host: String,
    },
}

fn main() -> ExitCode {
    // Hooks bypass argument parsing entirely: a parse error would exit with
    // status 2, which both hosts treat as "block this prompt".
    let args: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    if let Some(hook_args) = hook_invocation(&args) {
        return hook::main(&hook_args);
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // Anything mentioning `hook` that clap cannot make sense of (stray
        // flags, unquoted paths with spaces, --help) is still a hook call
        // from a host config, and must not exit 2 or print.
        Err(e) => match args.iter().position(|a| a == "hook") {
            Some(at) => {
                let rest: Vec<String> = args[..at].iter().chain(&args[at + 1..]).cloned().collect();
                return hook::main(&rest);
            }
            None => e.exit(),
        },
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("csm: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `hook ...`, optionally preceded by `--db <path>` or `--db=<path>`.
/// Returns the arguments for [`hook::main`] with the `hook` word removed.
fn hook_invocation(args: &[String]) -> Option<Vec<String>> {
    let at = match args {
        [first, ..] if first == "hook" => 0,
        [flag, _, third, ..] if flag == "--db" && third == "hook" => 2,
        [flag, second, ..] if flag.starts_with("--db=") && second == "hook" => 1,
        _ => return None,
    };
    Some(args[..at].iter().chain(&args[at + 1..]).cloned().collect())
}

/// The stored project key `path` refers to. An exact stored key (as `csm
/// status` or `csm journal --all` print it) is used verbatim. Otherwise the
/// path is resolved; if that key is not stored and the path no longer
/// exists, a stored key that differs only in letter case is accepted.
/// Never a parent or child project: deleting the wrong one is unrecoverable.
fn stored_project_key(store: &Store, path: &std::path::Path) -> Result<String> {
    let keys = store.project_keys()?;
    let typed = path.to_string_lossy().replace('\\', "/");
    if let Some(exact) = keys.iter().find(|k| **k == typed) {
        return Ok(exact.clone());
    }
    let key = project::resolve(path);
    if keys.contains(&key) || path.exists() {
        return Ok(key);
    }
    // Windows paths are case-insensitive; elsewhere a case difference is a
    // different directory.
    let same_name: Vec<&String> = keys.iter().filter(|k| cfg!(windows) && k.eq_ignore_ascii_case(&key)).collect();
    if let [only] = same_name.as_slice() {
        return Ok((*only).clone());
    }
    let related: Vec<&str> = keys
        .iter()
        .map(String::as_str)
        .filter(|k| {
            let (k, key) = (k.to_lowercase(), key.to_lowercase());
            key.starts_with(&format!("{k}/")) || k.starts_with(&format!("{key}/")) || k == key
        })
        .collect();
    if related.is_empty() {
        Ok(key)
    } else {
        Err(Error::Invalid(format!(
            "nothing is stored under {key}; related stored projects: {}. Pass one of them exactly.",
            related.join(", ")
        )))
    }
}

fn resolve_project(path: Option<PathBuf>) -> Result<String> {
    let dir = match path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    Ok(project::resolve(&dir))
}

fn print_json<T: serde::Serialize>(value: &T) {
    println!("{}", serde_json::to_string_pretty(value).expect("serializable"));
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn run(cli: Cli) -> Result<()> {
    if let Command::Hook { host } = &cli.command {
        let mut args = vec![host.clone()];
        if let Some(db) = &cli.db {
            args.extend(["--db".to_string(), db.display().to_string()]);
        }
        hook::main(&args);
        return Ok(());
    }
    let db = match cli.db {
        Some(db) => db,
        None => paths::default_db_path()?,
    };
    let store = Store::open(&db)?;

    match cli.command {
        Command::Save { content, kind, tags, importance, project } => {
            let project = resolve_project(project)?;
            let saved = store.save(NewMemory { project, kind, content, tags, importance, source: None })?;
            if saved.duplicate {
                println!("Already saved as #{}.", saved.memory.id);
            } else {
                println!("Saved #{}.", saved.memory.id);
            }
            if saved.redacted > 0 {
                eprintln!("Redacted {} secret(s) before saving.", saved.redacted);
            }
        }
        Command::Search { query, limit, kind, all, project, json } => {
            let project = resolve_project(project)?;
            let filter = Filter { project: (!all).then_some(project.as_str()), kind: kind.as_deref(), limit };
            let hits = store.search(&query, filter)?;
            if json {
                print_json(&hits);
            } else if hits.is_empty() {
                println!("No matching memories.");
            } else {
                hits.iter().for_each(|h| println!("{}", h.memory.line(all)));
            }
        }
        Command::List { limit, kind, all, project, json } => {
            let project = resolve_project(project)?;
            let filter = Filter { project: (!all).then_some(project.as_str()), kind: kind.as_deref(), limit };
            let memories = store.list(filter)?;
            if json {
                print_json(&memories);
            } else if memories.is_empty() {
                println!("No memories.");
            } else {
                memories.iter().for_each(|m| println!("{}", m.line(all)));
            }
        }
        Command::Delete { id } => {
            if !store.delete(id, None)? {
                return Err(Error::Invalid(format!("no memory #{id}")));
            }
            println!("Deleted #{id}.");
        }
        Command::Brief { project, budget, explain } => {
            let project = resolve_project(project)?;
            let request =
                BriefRequest { project: &project, current_session: None, exclude_active: true, budget, now: now_ms() };
            match brief::build(&store, &request)? {
                Some(b) => {
                    println!("{}", b.text);
                    if explain {
                        println!("\n{} bytes; sources: {}", b.text.len(), b.sources.join(" "));
                        if !b.trimmed.is_empty() {
                            println!("trimmed to fit: {}", b.trimmed.join(", "));
                        }
                    }
                }
                None => println!("Nothing recorded for {project} yet."),
            }
        }
        Command::Journal { limit, kind, session, all, project, json } => {
            let project = resolve_project(project)?;
            if let Some(k) = kind.as_deref().filter(|k| !EVENT_KINDS.contains(k)) {
                return Err(Error::Invalid(format!("unknown kind {k}; one of: {}", EVENT_KINDS.join(", "))));
            }
            let kinds: Vec<&str> = kind.as_deref().into_iter().collect();
            let events = store.events(&EventQuery {
                project: (!all).then_some(project.as_str()),
                session_id: session.as_deref(),
                kinds: &kinds,
                limit,
                ..Default::default()
            })?;
            if json {
                print_json(&events);
            } else if events.is_empty() {
                println!("No journal events.");
            } else {
                for e in &events {
                    let ok = match e.ok {
                        Some(true) => " ok",
                        Some(false) => " FAILED",
                        None => "",
                    };
                    let session = e.session_id.as_deref().map_or("-".to_string(), |s| s.chars().take(8).collect());
                    let where_ = if all { format!("  @ {}", e.project) } else { String::new() };
                    println!(
                        "#{} {} {:<6} {:<8} {:<12} {}{ok}{where_}",
                        e.id,
                        &e.at[..16],
                        e.host,
                        session,
                        e.kind,
                        e.summary
                    );
                }
            }
        }
        Command::Checkpoint { summary, next, blocker, goal, goal_done, project } => {
            if summary.len() > MAX_HANDOFF_BYTES || goal.as_ref().is_some_and(|g| g.len() > MAX_HANDOFF_BYTES) {
                eprintln!("Note: the summary and goal are kept to {MAX_HANDOFF_BYTES} bytes each.");
            }
            let result = store.checkpoint(Checkpoint {
                project: resolve_project(project)?,
                host: "cli".into(),
                session_id: None,
                summary,
                next,
                blockers: blocker,
                goal,
                goal_done,
            })?;
            println!("Checkpoint #{} recorded.", result.checkpoint_id);
            if let Some(closed) = result.closed_goal {
                println!("Closed goal: {closed}");
            }
        }
        Command::Status { project } => {
            let project = resolve_project(project)?;
            let stats = store.stats(Some(&project))?;
            println!("csm {}", env!("CARGO_PKG_VERSION"));
            println!("database: {}", db.display());
            println!("project:  {project}");
            println!("memories in this project: {}", stats.project_memories.unwrap_or(0));
            println!("memories total: {} across {} project(s)", stats.memories, stats.projects);
            println!("journal events in this project: {}", stats.events);
            if let Some((host, bytes, _)) = store.last_injection()? {
                println!("last briefing injected: {bytes} bytes via {host}");
            }
            if let Some(line) = hook::last_failure() {
                println!("last hook error: {line}");
            }
        }
        Command::Export { project, all } => {
            let project = resolve_project(project)?;
            let scope = (!all).then_some(project.as_str());
            let events = store.events(&EventQuery { project: scope, limit: usize::MAX >> 1, ..Default::default() })?;
            print_json(&serde_json::json!({ "memories": store.export(scope)?, "events": events }));
        }
        Command::Forget { project, yes } => {
            let project = stored_project_key(&store, &project)?;
            let memories = store.count_project(&project)?;
            let events = store.stats(Some(&project))?.events;
            if yes {
                // Always scrub, even with nothing left to delete: a run that
                // was blocked by another process finishes here.
                let forgotten = store.forget_project(&project)?;
                if forgotten.memories + forgotten.events == 0 {
                    println!("Nothing is stored for {project}.");
                } else {
                    println!(
                        "Permanently deleted {} memories and {} journal events for {project}.",
                        forgotten.memories, forgotten.events
                    );
                }
                if !forgotten.scrubbed {
                    println!(
                        "Another csm process was using the database, so copies of deleted text may remain on disk \
                         (in csm.db and its write-ahead log) until it finishes. Run this command again afterwards \
                         to finish scrubbing."
                    );
                }
            } else if memories + events == 0 {
                println!("Nothing is stored for {project}.");
            } else {
                println!(
                    "Would permanently delete {memories} memories and {events} journal events for {project}. Re-run with --yes to do it."
                );
            }
        }
        Command::Mcp { project } => {
            let project = resolve_project(project)?;
            Server::new(store, project).run_stdio()?;
        }
        Command::Hook { .. } => unreachable!("handled before opening the store"),
    }
    Ok(())
}
