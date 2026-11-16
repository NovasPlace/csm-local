//! Which model a session is running as. A host can switch models without
//! starting a new session, so the model is read again at every hook event and
//! written on each record, not kept once per session.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::capture::Host;
use crate::redact::redact;
use crate::store::Store;
use crate::Result;

pub const MAX_MODEL_BYTES: usize = 100;
/// Tables whose rows say which model recorded them.
const STAMPED: [&str; 5] = ["events", "memories", "csm_peers", "csm_decisions", "csm_file_claims"];
/// How much of the end of a transcript is read to find the newest reply:
/// usually the last line or two, rarely behind a few large tool results.
const TAIL_BYTES: [u64; 2] = [256 * 1024, 2 * 1024 * 1024];
/// What Claude Code names as the model of a reply it wrote itself.
const SYNTHETIC: &str = "<synthetic>";

impl Store {
    /// Adds the `model` column to every table that records one, an extension
    /// next to the core schema: older binaries never name the column, so
    /// what they record has no model.
    pub(crate) fn init_models(&self) -> Result<()> {
        let missing = |conn: &Connection| -> Result<Vec<&str>> {
            let mut stmt = conn.prepare(
                "SELECT t.name FROM sqlite_master t JOIN pragma_table_info(t.name) c
                 WHERE t.type = 'table' AND c.name = 'model'",
            )?;
            let have: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            Ok(STAMPED.into_iter().filter(|table| !have.iter().any(|h| h == table)).collect())
        };
        if missing(&self.conn)?.is_empty() {
            return Ok(());
        }
        let tx = self.immediate()?;
        for table in missing(&tx)? {
            tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN model TEXT"))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Records the model a session is running as from now on.
    pub fn set_peer_model(&self, id: &str, model: &str) -> Result<()> {
        self.conn.execute("UPDATE csm_peers SET model = ?2 WHERE id = ?1 AND model IS NOT ?2", params![id, model])?;
        Ok(())
    }

    /// The model on the newest event from one agent in one session.
    pub fn session_model(&self, host: &str, session_id: &str, agent_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT model FROM events WHERE session_id = ?2 AND host = ?1 AND agent_id = ?3
                 ORDER BY id DESC LIMIT 1",
                params![host, session_id, agent_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Stamps what a csm tool call just wrote in `project` with the model
    /// that made the call, going by the tool's reply. The MCP server knows
    /// the model only of a session that connected, and the command line
    /// knows none; the hook that sees the call knows it for every session.
    /// A record that already has a model keeps it. Returns how many records
    /// were stamped.
    pub fn stamp_saved(&self, project: &str, reply: &Value, model: &str) -> Result<usize> {
        let Some(model) = clean(model) else {
            return Ok(0);
        };
        let reply = texts(reply);
        let id = |made: &Regex| made.captures(&reply).and_then(|c| c[1].parse::<i64>().ok());
        let mut stamped = 0;
        if let Some(memory) = id(&SAVED_MEMORY) {
            stamped += self.conn.execute(
                "UPDATE memories SET model = ?1 WHERE id = ?2 AND project = ?3 AND (model IS NULL OR model = '')",
                params![model, memory, project],
            )?;
        }
        // A checkpoint that sets or closes a goal writes those rows at the
        // same instant.
        if let Some(checkpoint) = id(&SAVED_CHECKPOINT) {
            stamped += self.conn.execute(
                "UPDATE events SET model = ?1
                 WHERE project = ?3 AND kind IN ('checkpoint', 'goal', 'goal_done') AND (model IS NULL OR model = '')
                   AND ts = (SELECT ts FROM events WHERE id = ?2 AND project = ?3 AND kind = 'checkpoint')",
                params![model, checkpoint, project],
            )?;
        }
        if let Some(decision) = id(&SAVED_DECISION) {
            stamped += self.conn.execute(
                "UPDATE csm_decisions SET model = ?1 WHERE id = ?2 AND project = ?3 AND (model IS NULL OR model = '')",
                params![model, decision, project],
            )?;
        }
        Ok(stamped)
    }
}

/// What csm's tools and command line say when they have written a record.
/// "Already saved as #4" and "Already on the board as #4" wrote nothing.
static SAVED_MEMORY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bSaved #(\d+)").unwrap());
static SAVED_CHECKPOINT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bCheckpoint #(\d+) recorded").unwrap());
static SAVED_DECISION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bRecorded standing decision #(\d+)").unwrap());

/// Every piece of text in a tool's reply, whatever shape the host passes it
/// in: the text itself, content blocks, or a shell's output fields.
fn texts(reply: &Value) -> String {
    fn collect(value: &Value, depth: usize, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Array(items) if depth < 4 => items.iter().for_each(|item| collect(item, depth + 1, out)),
            Value::Object(fields) if depth < 4 => fields.values().for_each(|field| collect(field, depth + 1, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    collect(reply, 0, &mut out);
    out.join("\n")
}

/// A model name fit to record: one line within the size limit that is not a
/// placeholder and holds nothing that looks like a credential.
pub fn clean(model: &str) -> Option<String> {
    let model = model.trim();
    let fits = !model.is_empty() && model.len() <= MAX_MODEL_BYTES && !model.chars().any(char::is_control);
    (fits && model != SYNTHETIC && redact(model).count == 0).then(|| model.to_string())
}

/// " (claude-opus-5-5)" after a host name, or nothing without a model.
pub fn suffix(model: Option<&str>) -> String {
    model.map(|m| format!(" ({m})")).unwrap_or_default()
}

/// The model behind a hook event, when the host says. Codex names it in
/// every payload. Claude names it only at session start, and `/model` changes
/// it with no event of its own, so otherwise the newest reply in the
/// transcript says who is answering. A subagent can run a different model
/// from its session, and is read from its own transcript only.
pub fn observed(host: Host, payload: &Value) -> Option<String> {
    let field = |key: &str| payload.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let named = payload.get("model").and_then(|m| m.as_str().or_else(|| m.get("id")?.as_str())).and_then(clean);
    if named.is_some() || host == Host::Codex {
        return named;
    }
    match field("agent_id") {
        None => newest_reply(Path::new(field("transcript_path")?), false),
        Some(agent) => {
            let own = field("agent_transcript_path").map(PathBuf::from).or_else(|| {
                let session = Path::new(field("transcript_path")?);
                Some(session.with_extension("").join("subagents").join(format!("agent-{agent}.jsonl")))
            })?;
            newest_reply(&own, true)
        }
    }
}

/// The model of the newest reply in a Claude Code transcript (JSON lines).
/// A session's own transcript can hold its subagents' replies too; those are
/// passed over unless `subagent` says the transcript is one's own.
fn newest_reply(transcript: &Path, subagent: bool) -> Option<String> {
    let mut file = File::open(transcript).ok()?;
    let len = file.metadata().ok()?.len();
    for window in TAIL_BYTES {
        let start = len.saturating_sub(window);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut tail = Vec::new();
        file.by_ref().take(window).read_to_end(&mut tail).ok()?;
        // Cut mid-line unless the read began at the start of the file.
        let whole = match tail.iter().position(|b| *b == b'\n') {
            Some(end) if start > 0 => &tail[end + 1..],
            _ => &tail[..],
        };
        let found = String::from_utf8_lossy(whole).lines().rev().find_map(|line| reply_model(line, subagent));
        if found.is_some() || start == 0 {
            return found;
        }
    }
    None
}

fn reply_model(line: &str, subagent: bool) -> Option<String> {
    // Most lines are something else, and some are large: look before parsing.
    if !line.contains("\"assistant\"") {
        return None;
    }
    let entry: Value = serde_json::from_str(line).ok()?;
    let sidechain = entry.get("isSidechain").and_then(Value::as_bool).unwrap_or(false);
    if entry.get("type").and_then(Value::as_str) != Some("assistant") || (sidechain && !subagent) {
        return None;
    }
    clean(entry.pointer("/message/model")?.as_str()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reply(model: &str, sidechain: bool) -> String {
        json!({ "type": "assistant", "isSidechain": sidechain, "message": { "role": "assistant", "model": model } })
            .to_string()
    }

    fn user(text: &str) -> String {
        json!({ "type": "user", "message": { "role": "user", "content": text } }).to_string()
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("csm-model-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn transcript(dir: &Path, name: &str, lines: &[String]) -> String {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_model_name_is_one_short_line_and_not_a_placeholder() {
        assert_eq!(clean("  claude-opus-5-5 ").as_deref(), Some("claude-opus-5-5"));
        assert_eq!(clean("GPT-5.2 Codex").as_deref(), Some("GPT-5.2 Codex"));
        for bad in ["", "  ", SYNTHETIC, "a\nb", &"m".repeat(MAX_MODEL_BYTES + 1), "sk-proj-AbCdEfGhIjKlMnOpQrStUv12"] {
            assert_eq!(clean(bad), None, "{bad:?}");
        }
        assert_eq!(suffix(Some("gpt-5")), " (gpt-5)");
        assert_eq!(suffix(None), "");
    }

    #[test]
    fn codex_names_its_model_and_claude_is_read_from_its_newest_reply() {
        let dir = temp("newest");
        assert_eq!(observed(Host::Codex, &json!({ "model": "gpt-5.2-codex" })).as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(observed(Host::Codex, &json!({ "model": { "id": "gpt-5" } })).as_deref(), Some("gpt-5"));
        assert_eq!(observed(Host::Codex, &json!({ "transcript_path": null })), None);

        let lines = [
            reply("claude-opus-5-5", false),
            user("/model"),
            reply("claude-fable-5-2", false),
            reply("claude-haiku-5-5", true),
            reply(SYNTHETIC, false),
            user(&format!("a tool result that quotes {}", reply("not-this-one", false))),
        ];
        let path = transcript(&dir, "s1.jsonl", &lines);
        let stop = json!({ "hook_event_name": "Stop", "transcript_path": path });
        assert_eq!(observed(Host::Claude, &stop).as_deref(), Some("claude-fable-5-2"), "the newest reply of its own");
        let start = json!({ "hook_event_name": "SessionStart", "transcript_path": path, "model": "claude-opus-5-5" });
        assert_eq!(observed(Host::Claude, &start).as_deref(), Some("claude-opus-5-5"), "the host's word comes first");

        let fresh = transcript(&dir, "s2.jsonl", &[user("hello")]);
        assert_eq!(observed(Host::Claude, &json!({ "transcript_path": fresh })), None);
        assert_eq!(observed(Host::Claude, &json!({ "transcript_path": &dir.join("gone.jsonl") })), None);
        assert_eq!(observed(Host::Claude, &json!({ "hook_event_name": "Stop" })), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_subagent_is_read_from_its_own_transcript() {
        let dir = temp("subagent");
        let session = transcript(&dir, "s1.jsonl", &[reply("claude-opus-5-5", false)]);
        let own = transcript(&dir, "s1/subagents/agent-a1.jsonl", &[reply("claude-haiku-5-5", true)]);
        let named = json!({ "agent_id": "a9", "transcript_path": session, "agent_transcript_path": own });
        assert_eq!(observed(Host::Claude, &named).as_deref(), Some("claude-haiku-5-5"));
        let found = json!({ "agent_id": "a1", "transcript_path": session });
        assert_eq!(observed(Host::Claude, &found).as_deref(), Some("claude-haiku-5-5"), "beside its session's");
        let unknown = json!({ "agent_id": "a2", "transcript_path": session });
        assert_eq!(observed(Host::Claude, &unknown), None, "never its session's model");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_reply_behind_large_tool_results_is_still_found() {
        let dir = temp("tail");
        let big = user(&"x".repeat(200 * 1024));
        let path = transcript(&dir, "s1.jsonl", &[reply("claude-opus-5-5", false), big.clone(), big]);
        let stop = json!({ "transcript_path": path });
        assert_eq!(observed(Host::Claude, &stop).as_deref(), Some("claude-opus-5-5"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_column_is_added_to_a_database_made_before_it() {
        let store = Store::open_in_memory().unwrap();
        for table in STAMPED {
            store.conn.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN model")).unwrap();
        }
        store.init_models().unwrap();
        store.init_models().unwrap();
        for table in STAMPED {
            store.conn.query_row(&format!("SELECT COUNT(model) FROM {table}"), [], |r| r.get::<_, i64>(0)).unwrap();
        }
    }

    #[test]
    fn what_a_tool_call_wrote_is_stamped_with_the_callers_model() {
        use crate::journal::Checkpoint;
        use crate::store::NewMemory;
        const P: &str = "C:/work/app";
        let store = Store::open_in_memory().unwrap();
        let remember = |content: &str| {
            let new = NewMemory {
                project: P.into(),
                kind: "lesson".into(),
                content: content.into(),
                tags: vec![],
                importance: 3,
                source: None,
                model: None,
            };
            store.save(new).unwrap().memory.id
        };
        let stamped = |sql: &str| -> i64 { store.conn.query_row(sql, [], |r| r.get(0)).unwrap() };

        // A memory, from an MCP reply's content blocks.
        let lto = remember("Release builds need LTO");
        let reply = json!([{ "type": "text", "text": format!("Saved #{lto} (lesson).") }]);
        assert_eq!(store.stamp_saved(P, &reply, "claude-opus-5-5").unwrap(), 1);
        assert_eq!(store.get(lto).unwrap().unwrap().model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(store.stamp_saved(P, &reply, "claude-fable-5-2").unwrap(), 0, "the model it has stays");
        assert_eq!(store.get(lto).unwrap().unwrap().model.as_deref(), Some("claude-opus-5-5"));

        // Nothing was written, or not here, or by no model worth naming.
        let dup = remember("Tests need the network off");
        let already = json!(format!("Already saved as #{dup}; nothing added."));
        assert_eq!(store.stamp_saved(P, &already, "claude-opus-5-5").unwrap(), 0);
        let saved = json!(format!("Saved #{dup} (lesson)."));
        assert_eq!(store.stamp_saved("C:/work/other", &saved, "claude-opus-5-5").unwrap(), 0, "another project");
        assert_eq!(store.stamp_saved(P, &saved, SYNTHETIC).unwrap(), 0, "a placeholder");
        assert_eq!(store.get(dup).unwrap().unwrap().model, None);

        // A checkpoint and the goal it set, from a shell's output.
        let goal = Checkpoint {
            project: P.into(),
            host: "mcp".into(),
            summary: "Hooks wired".into(),
            goal: Some("Ship v1".into()),
            ..Default::default()
        };
        let made = store.checkpoint(goal).unwrap();
        let shell =
            json!({ "stdout": format!("Checkpoint #{} recorded. Goal set.\n", made.checkpoint_id), "stderr": "" });
        assert_eq!(store.stamp_saved(P, &shell, "gpt-5.2-codex").unwrap(), 2);
        assert_eq!(
            stamped("SELECT COUNT(*) FROM events WHERE model = 'gpt-5.2-codex' AND kind IN ('checkpoint', 'goal')"),
            2
        );

        // A standing decision, from a reply wrapped in a result object.
        let decided = store.decide(P, Some("Saves don't matter"), None, "mcp", 10).unwrap();
        let reply = json!({ "content": [{ "type": "text", "text": decided.summary() }] });
        assert_eq!(store.stamp_saved(P, &reply, "claude-opus-5-5").unwrap(), 1);
        assert_eq!(stamped("SELECT COUNT(*) FROM csm_decisions WHERE model = 'claude-opus-5-5'"), 1);
    }
}
