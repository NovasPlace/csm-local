//! Minimal MCP server over stdio: newline-delimited JSON-RPC 2.0, tools only.

use std::io::{self, BufRead, Write};
use std::path::Path;

use serde_json::{json, Value};

use crate::brief::{self, BriefRequest};
use crate::journal::{Checkpoint, MAX_HANDOFF_BYTES};
use crate::store::{Filter, NewMemory, Store, KINDS};
use crate::time::now_ms;
use crate::{project, Error, Result};

const LATEST_PROTOCOL: &str = "2025-06-18";
const SUPPORTED_PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

const INSTRUCTIONS: &str = "Private, local memory for this project that persists across sessions. \
csm_brief summarizes earlier sessions (hosts with csm hooks receive it automatically at session start). \
csm_search recalls saved decisions, lessons and facts; csm_save stores new ones, one idea per memory. \
csm_checkpoint records progress, next steps and the current goal for the next session. \
Obvious credentials are redacted before anything is stored.";

pub struct Server {
    store: Store,
    project: String,
}

impl Server {
    pub fn new(store: Store, project: String) -> Self {
        Server { store, project }
    }

    pub fn run_stdio(&mut self) -> io::Result<()> {
        let stdin = io::stdin();
        let mut out = io::stdout().lock();
        for line in stdin.lock().lines() {
            let line = line?;
            let line = line.trim_start_matches('\u{feff}').trim();
            if line.is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(line) {
                Ok(msg) => self.handle(&msg),
                Err(e) => Some(error_response(Value::Null, -32700, &format!("parse error: {e}"))),
            };
            if let Some(reply) = reply {
                serde_json::to_writer(&mut out, &reply)?;
                out.write_all(b"\n")?;
                out.flush()?;
            }
        }
        Ok(())
    }

    /// Handles one JSON-RPC message. Returns `None` for notifications and
    /// for responses sent by the client.
    pub fn handle(&mut self, msg: &Value) -> Option<Value> {
        let method = msg.get("method")?.as_str()?;
        let id = msg.get("id")?.clone();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => self.call_tool(&params),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error_response(id, code, &message),
        })
    }

    fn call_tool(&mut self, params: &Value) -> std::result::Result<Value, (i64, String)> {
        let name = params.get("name").and_then(Value::as_str).ok_or((-32602, "missing tool name".to_string()))?;
        let empty = json!({});
        let args = params.get("arguments").filter(|a| a.is_object()).unwrap_or(&empty);
        let outcome = match name {
            "csm_save" => self.save(args),
            "csm_search" => self.search(args),
            "csm_list" => self.list(args),
            "csm_delete" => self.delete(args),
            "csm_status" => self.status(args),
            "csm_brief" => self.brief(args),
            "csm_checkpoint" => self.checkpoint(args),
            _ => return Err((-32602, format!("unknown tool: {name}"))),
        };
        let (text, is_error) = match outcome {
            Ok(text) => (text, false),
            Err(e) => (e.to_string(), true),
        };
        Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
    }

    fn project_for(&self, args: &Value) -> String {
        str_arg(args, "project").map(|p| project::resolve(Path::new(p))).unwrap_or_else(|| self.project.clone())
    }

    fn save(&mut self, args: &Value) -> Result<String> {
        let saved = self.store.save(NewMemory {
            project: self.project_for(args),
            kind: str_arg(args, "kind").unwrap_or("note").to_string(),
            content: required_str(args, "content")?.to_string(),
            tags: list_arg(args, "tags"),
            importance: int_arg(args, "importance").unwrap_or(3),
            source: str_arg(args, "source").map(str::to_string),
        })?;
        let mut text = if saved.duplicate {
            format!("Already saved as #{}; nothing added.", saved.memory.id)
        } else {
            format!("Saved #{} ({}).", saved.memory.id, saved.memory.kind)
        };
        if saved.redacted > 0 {
            text.push_str(&format!(" Redacted {} secret(s) before saving.", saved.redacted));
        }
        Ok(text)
    }

    fn search(&mut self, args: &Value) -> Result<String> {
        let query = required_str(args, "query")?;
        let all_projects = args.get("all_projects").and_then(Value::as_bool).unwrap_or(false);
        let project = self.project_for(args);
        let hits = self.store.search(
            query,
            Filter {
                project: (!all_projects).then_some(project.as_str()),
                kind: str_arg(args, "kind"),
                limit: limit_arg(args, 10),
            },
        )?;
        if hits.is_empty() {
            return Ok("No matching memories.".into());
        }
        Ok(hits.iter().map(|h| h.memory.line(all_projects)).collect::<Vec<_>>().join("\n"))
    }

    fn list(&mut self, args: &Value) -> Result<String> {
        let project = self.project_for(args);
        let memories = self.store.list(Filter {
            project: Some(&project),
            kind: str_arg(args, "kind"),
            limit: limit_arg(args, 20),
        })?;
        if memories.is_empty() {
            return Ok("No memories saved for this project yet.".into());
        }
        Ok(memories.iter().map(|m| m.line(false)).collect::<Vec<_>>().join("\n"))
    }

    fn delete(&mut self, args: &Value) -> Result<String> {
        let id = int_arg(args, "id").ok_or_else(|| Error::Invalid("missing required argument: id".into()))?;
        let project = self.project_for(args);
        if self.store.delete(id, Some(&project))? {
            Ok(format!("Deleted #{id}."))
        } else {
            Err(Error::Invalid(format!("no memory #{id} in project {project}")))
        }
    }

    fn status(&mut self, args: &Value) -> Result<String> {
        let project = self.project_for(args);
        let stats = self.store.stats(Some(&project))?;
        let db = self.store.path().map_or("(in memory)".to_string(), |p| p.display().to_string());
        Ok(format!(
            "csm {}\nproject: {project}\ndatabase: {db}\nmemories in this project: {}\nmemories total: {} across {} project(s)\njournal events in this project: {}",
            env!("CARGO_PKG_VERSION"),
            stats.project_memories.unwrap_or(0),
            stats.memories,
            stats.projects,
            stats.events,
        ))
    }

    fn brief(&mut self, args: &Value) -> Result<String> {
        let project = self.project_for(args);
        let request = BriefRequest {
            project: &project,
            current_session: None,
            exclude_active: true,
            budget: brief::DEFAULT_BUDGET,
            now: now_ms(),
        };
        Ok(match brief::build(&self.store, &request)? {
            Some(b) => b.text,
            None => "Nothing recorded for this project yet.".into(),
        })
    }

    fn checkpoint(&mut self, args: &Value) -> Result<String> {
        let summary = required_str(args, "summary")?.to_string();
        let long = summary.len() > MAX_HANDOFF_BYTES;
        let result = self.store.checkpoint(Checkpoint {
            project: self.project_for(args),
            host: "mcp".into(),
            session_id: None,
            summary,
            next: list_arg(args, "next"),
            blockers: list_arg(args, "blockers"),
            goal: str_arg(args, "goal").map(str::to_string),
            goal_done: args.get("goal_done").and_then(Value::as_bool).unwrap_or(false),
        })?;
        let mut text = format!("Checkpoint #{} recorded.", result.checkpoint_id);
        if let Some(closed) = result.closed_goal {
            text.push_str(&format!(" Closed goal: {closed}."));
        }
        if result.goal_id.is_some() {
            text.push_str(" Goal set.");
        }
        if long {
            text.push_str(&format!(
                " The summary was cut to {MAX_HANDOFF_BYTES} bytes; next steps and blockers have their own fields."
            ));
        }
        Ok(text)
    }
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested.filter(|v| SUPPORTED_PROTOCOLS.contains(v)).unwrap_or(LATEST_PROTOCOL);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "csm", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn tools() -> Value {
    let project = json!({
        "type": "string",
        "description": "Project root path. Defaults to the project this server was started in."
    });
    json!([
        {
            "name": "csm_save",
            "description": "Save a memory for this project: a decision, lesson, fact or note worth knowing in a later session. Keep it to one idea.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "The memory itself, written to make sense with no other context." },
                    "kind": { "type": "string", "enum": KINDS, "default": "note" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "importance": { "type": "integer", "minimum": 1, "maximum": 5, "default": 3 },
                    "source": { "type": "string", "description": "Where this came from, e.g. a file path or command." },
                    "project": project
                },
                "required": ["content"]
            }
        },
        {
            "name": "csm_search",
            "description": "Search saved memories by keywords. Use before starting work to recall past decisions, lessons and facts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "kind": { "type": "string", "enum": KINDS },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 10 },
                    "all_projects": { "type": "boolean", "default": false, "description": "Search every project instead of only this one." },
                    "project": project
                },
                "required": ["query"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "csm_list",
            "description": "List the most recent memories for this project, newest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": KINDS },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 },
                    "project": project
                }
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "csm_delete",
            "description": "Permanently delete one memory from this project by id, e.g. when it is wrong or outdated.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "project": project
                },
                "required": ["id"]
            },
            "annotations": { "destructiveHint": true }
        },
        {
            "name": "csm_status",
            "description": "Show the csm version, database location and memory counts.",
            "inputSchema": { "type": "object", "properties": { "project": project } },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "csm_brief",
            "description": "Summarize earlier sessions in this project: active goal, last checkpoint, the previous session's request, changes and commands, the last unresolved failure, and key saved memories.",
            "inputSchema": { "type": "object", "properties": { "project": project } },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "csm_checkpoint",
            "description": "Record where the work stands so the next session can pick it up: what is done, next steps, blockers. Can also set the project's goal or mark it done.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "summary": { "type": "string", "description": "What is done and the current state, in a sentence or two." },
                    "next": { "type": "array", "items": { "type": "string" }, "description": "Next steps, most important first." },
                    "blockers": { "type": "array", "items": { "type": "string" } },
                    "goal": { "type": "string", "description": "Set the project's current goal." },
                    "goal_done": { "type": "boolean", "description": "Mark the current goal as done." },
                    "project": project
                },
                "required": ["summary"]
            }
        }
    ])
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    str_arg(args, key).ok_or_else(|| Error::Invalid(format!("missing required argument: {key}")))
}

/// A string array, or a single string treated as one item.
fn list_arg(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn int_arg(args: &Value, key: &str) -> Option<i64> {
    let value = args.get(key)?;
    value.as_i64().or_else(|| value.as_str()?.trim().parse().ok())
}

fn limit_arg(args: &Value, default: usize) -> usize {
    int_arg(args, "limit").map_or(default, |n| n.clamp(1, 100) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "C:/work/app";

    fn server() -> Server {
        Server::new(Store::open_in_memory().unwrap(), P.into())
    }

    fn request(server: &mut Server, id: i64, method: &str, params: Value) -> Value {
        server
            .handle(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .expect("requests get a response")
    }

    fn call(server: &mut Server, name: &str, arguments: Value) -> (String, bool) {
        let reply = request(server, 1, "tools/call", json!({ "name": name, "arguments": arguments }));
        let result = &reply["result"];
        (result["content"][0]["text"].as_str().unwrap().to_string(), result["isError"].as_bool().unwrap())
    }

    #[test]
    fn initializes_with_negotiated_version() {
        let mut s = server();
        let reply = request(&mut s, 1, "initialize", json!({ "protocolVersion": "2025-03-26" }));
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], "csm");
        assert!(reply["result"]["capabilities"]["tools"].is_object());

        let reply = request(&mut s, 2, "initialize", json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(reply["result"]["protocolVersion"], LATEST_PROTOCOL);
    }

    #[test]
    fn ignores_notifications_and_client_responses() {
        let mut s = server();
        assert!(s.handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());
        assert!(s.handle(&json!({ "jsonrpc": "2.0", "id": 7, "result": {} })).is_none());
    }

    #[test]
    fn lists_tools_with_schemas() {
        let mut s = server();
        let reply = request(&mut s, 1, "tools/list", json!({}));
        let tools = reply["result"]["tools"].as_array().unwrap();
        let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["csm_save", "csm_search", "csm_list", "csm_delete", "csm_status", "csm_brief", "csm_checkpoint"]
        );
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));
    }

    #[test]
    fn checkpoint_then_brief() {
        let mut s = server();
        let (text, _) = call(&mut s, "csm_brief", json!({}));
        assert_eq!(text, "Nothing recorded for this project yet.");

        let (text, err) = call(
            &mut s,
            "csm_checkpoint",
            json!({ "summary": "Hooks wired", "next": ["installer"], "blockers": "none yet", "goal": "Ship v1" }),
        );
        assert!(!err, "{text}");
        assert_eq!(text, "Checkpoint #2 recorded. Goal set.");

        let (text, _) = call(&mut s, "csm_brief", json!({}));
        assert!(text.contains("Goal (set just now via mcp): Ship v1"), "{text}");
        assert!(
            text.contains("Last checkpoint (just now, via mcp): Hooks wired\n  Blocker: none yet\n  Next: installer"),
            "{text}"
        );

        let (text, _) = call(&mut s, "csm_checkpoint", json!({ "summary": "Released", "goal_done": true }));
        assert_eq!(text, "Checkpoint #4 recorded. Closed goal: Ship v1.");

        let (text, err) = call(&mut s, "csm_checkpoint", json!({ "next": ["x"] }));
        assert!(err);
        assert_eq!(text, "missing required argument: summary");
    }

    #[test]
    fn save_search_list_delete_round_trip() {
        let mut s = server();
        let (text, err) = call(
            &mut s,
            "csm_save",
            json!({ "content": "Release builds need LTO for size", "kind": "lesson", "tags": ["build"] }),
        );
        assert!(!err, "{text}");
        assert_eq!(text, "Saved #1 (lesson).");

        let (text, _) = call(&mut s, "csm_save", json!({ "content": "Release builds need LTO for size" }));
        assert_eq!(text, "Already saved as #1; nothing added.");

        let (text, err) = call(&mut s, "csm_search", json!({ "query": "lto release" }));
        assert!(!err);
        assert!(text.starts_with("#1 [lesson]"), "{text}");
        assert!(text.contains("(tags: build)"));

        let (text, _) = call(&mut s, "csm_list", json!({}));
        assert!(text.contains("Release builds need LTO"));

        let (text, err) = call(&mut s, "csm_delete", json!({ "id": 1 }));
        assert!(!err);
        assert_eq!(text, "Deleted #1.");
        let (text, _) = call(&mut s, "csm_search", json!({ "query": "lto" }));
        assert_eq!(text, "No matching memories.");
    }

    #[test]
    fn tool_errors_are_results_not_protocol_errors() {
        let mut s = server();
        let (text, err) = call(&mut s, "csm_save", json!({ "kind": "note" }));
        assert!(err);
        assert_eq!(text, "missing required argument: content");

        let (text, err) = call(&mut s, "csm_delete", json!({ "id": 42 }));
        assert!(err);
        assert!(text.contains("no memory #42"));
    }

    #[test]
    fn reports_redactions() {
        let mut s = server();
        let (text, err) = call(&mut s, "csm_save", json!({ "content": "prod DB_PASSWORD=Tr0ub4dor&3xyz" }));
        assert!(!err);
        assert!(text.ends_with("Redacted 1 secret(s) before saving."), "{text}");
        let (text, _) = call(&mut s, "csm_list", json!({}));
        assert!(!text.contains("Tr0ub4dor"), "{text}");
    }

    #[test]
    fn protocol_errors() {
        let mut s = server();
        let reply = request(&mut s, 3, "resources/list", json!({}));
        assert_eq!(reply["error"]["code"], -32601);
        let reply = request(&mut s, 4, "tools/call", json!({ "name": "csm_nope" }));
        assert_eq!(reply["error"]["code"], -32602);
        assert_eq!(reply["id"], 4);
    }

    #[test]
    fn status_reports_counts() {
        let mut s = server();
        call(&mut s, "csm_save", json!({ "content": "one" }));
        call(&mut s, "csm_save", json!({ "content": "two", "project": "C:/somewhere/else" }));
        let (text, _) = call(&mut s, "csm_status", json!({}));
        assert!(text.contains("memories in this project: 1"), "{text}");
        assert!(text.contains("memories total: 2 across 2 project(s)"), "{text}");
    }
}
