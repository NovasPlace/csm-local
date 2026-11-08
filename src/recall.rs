//! What the user asked before. When the user types a request, earlier
//! requests in the project that share its rarer words are shown with the
//! reply each got and what the user said next, so a session picks up where
//! the last one on the same thing left off instead of guessing again.
//!
//! A request that answers a reported result with "not fixed" or "you never
//! touched the legs" is pushback on that result; the briefing and the claim
//! check read those. Everything here is read from the journal, which keeps
//! requests and replies for [`RECALL_DAYS`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::brief::is_typed_prompt;
use crate::decisions::short_session;
use crate::journal::{Event, EventQuery, SUGGESTION_PREAMBLE};
use crate::store::Store;
use crate::text::{one_line, truncate};
use crate::time::ago;
use crate::Result;

const DAY_MS: i64 = 86_400_000;
pub const RECALL_DAYS: i64 = 90;
const MAX_CANDIDATES: usize = 2_000;
const MAX_RECALLED: usize = 3;
/// What a match must share: at least two words, weighted by how rare each is
/// among the project's requests. One word in common is too often chance.
const MIN_SHARED: usize = 2;
const MIN_SCORE: f64 = 6.0;
const MIN_REQUEST_TERMS: usize = 3;

/// The user saying a reported result is not so.
static PUSHBACK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(you(?:['’]re| are)? (?:lying|lied)|lie to me|lying to me|keep lying|not fixed|(?:is|was)n['’]?t fixed|still (?:broken|bugged|wrong|the same|happening|there|not (?:working|fixed|right))|(?:did|does)n['’]?t (?:work|fix|change|do anything)|nothing (?:has )?changed|no change|same (?:bug|issue|problem)|you never (?:touched|changed|fixed|did)|not what i (?:asked|wanted|said)|that['’]?s (?:wrong|not right)|wrong again|you broke)\b",
    )
    .expect("valid pushback pattern")
});

/// A sentence that reports a result.
static CLAIM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(fixed|works|resolved|landed|no longer|now (?:works|runs|passes|stays|shows|handles)|(?:tests?|checks?|regressions?) (?:now )?pass(?:es|ed)?|all \d+ \w+ pass(?:es|ed)?|verified|confirmed|is (?:now )?on live|is done|are done)\b",
    )
    .expect("valid claim pattern")
});

/// A sentence that says what was not checked, or that something is not fixed.
static HEDGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(not (?:yet )?(?:been )?(?:tested|run|verified|checked|played|confirmed|fixed)|untested|unverified|unchecked|(?:have|has)n['’]?t (?:yet )?(?:been )?(?:run|tested|played|checked|verified)|without (?:running|testing)|could(?:n['’]?t| not) (?:run|test|check|verify))\b",
    )
    .expect("valid hedge pattern")
});

/// Wording that looks like a claim and is not one.
static NOT_A_CLAIM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bhow (?:it|this|that|the \w+) works\b").expect("valid non-claim pattern"));

static SENTENCE_BREAK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[.!?]+\s+|\n+").expect("valid sentence pattern"));

/// `@file` attachments a host puts in front of a request.
static MENTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"@"[^"]*"|@\S+"#).expect("valid mention pattern"));

/// Words too common in requests and replies to say what one is about.
static STOPWORDS: LazyLock<BTreeSet<&str>> = LazyLock::new(|| {
    "able about add added after again all already also and another any anything are aren ask asked back bad been \
     before being best better bit both but call can cant come cool could did didn didnt does doesn doesnt doing \
     don done dont each even every everything few fix for from get give going gonna good got great guess had haha \
     has have hell her here hey his how idea ill into isn issue its just keep kind kinda know let lets like lil \
     little lol look lot made make maybe more most much nah need new nice not now off okay old once one only onto \
     other our out over own part please point pretty progress real really right said same say see seem should some \
     something still stuff such sure tell than that the their them then there these they thing think this those \
     though thought through too try under use very wait want wanna was wasn way well were what when where which \
     while who why will with work would yeah yes yet you your"
        .split_whitespace()
        .collect()
});

/// Distinctive words of `text`, lower case and roughly stemmed, so
/// "running legs" and "the leg runs" share "run" and "leg".
pub fn terms(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 3 && !w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| !STOPWORDS.contains(w.as_str()))
        .map(|w| stem(&w))
        .filter(|w| !STOPWORDS.contains(w.as_str()))
        .collect()
}

fn stem(word: &str) -> String {
    for suffix in ["ing", "ed"] {
        if let Some(base) = word.strip_suffix(suffix).filter(|b| b.chars().count() >= 3) {
            let chars: Vec<char> = base.chars().collect();
            let n = chars.len();
            // "running" -> "run", but "rolled" -> "roll".
            if chars[n - 1] == chars[n - 2] && !matches!(chars[n - 1], 'l' | 's' | 'z') {
                return chars[..n - 1].iter().collect();
            }
            return base.to_string();
        }
    }
    match word.strip_suffix('s') {
        Some(base) if base.chars().count() >= 3 && !base.ends_with('s') => base.to_string(),
        _ => word.to_string(),
    }
}

pub fn is_pushback(prompt: &str) -> bool {
    PUSHBACK.is_match(prompt)
}

/// Whether a reply says somewhere what it did not check.
pub fn hedged(reply: &str) -> bool {
    HEDGE.is_match(reply)
}

/// Up to two sentences of `reply` that report a result without hedging it.
pub fn claim_sentences(reply: &str) -> Vec<String> {
    let plain = reply.replace("**", "").replace('`', "");
    SENTENCE_BREAK
        .split(&plain)
        .map(|s| one_line(s.trim_start_matches(['-', '*', '#', '>', ' '])))
        .filter(|s| CLAIM.is_match(&NOT_A_CLAIM.replace_all(s, "")) && !HEDGE.is_match(s))
        .map(|s| s.trim_end_matches(['.', '!', '?', ' ']).to_string())
        .take(2)
        .collect()
}

/// Words the user wrote: not host text, a Codex attachment list, or a Codex
/// app request for a suggestion.
fn human(summary: &str) -> bool {
    is_typed_prompt(summary)
        && !summary.trim_start().starts_with('<')
        && !summary.starts_with('#')
        && !summary.starts_with(SUGGESTION_PREAMBLE)
}

/// A request with its `@file` attachments left out.
pub fn spoken(prompt: &str) -> String {
    one_line(&MENTION.replace_all(prompt, " "))
}

/// A reported result the user pushed back on.
#[derive(Debug, Clone)]
pub struct Pushback {
    /// The event of the user's answer.
    pub id: i64,
    pub ts: i64,
    pub host: String,
    pub session_id: String,
    /// What the reply claimed, in its own words.
    pub claim: String,
    /// What the user said back.
    pub words: String,
    reply: i64,
}

/// Results reported in `project` since `since` that the user pushed back
/// on, newest first, one per reply: the user's first answer to it.
pub fn pushbacks(store: &Store, project: &str, since: i64) -> Result<Vec<Pushback>> {
    let prompts = store.events(&EventQuery {
        project: Some(project),
        kinds: &["prompt"],
        exclude_suggestion_sessions: true,
        since_ts: Some(since),
        limit: MAX_CANDIDATES,
        ..Default::default()
    })?;
    let mut found: Vec<Pushback> = Vec::new();
    for p in prompts.iter().filter(|p| p.agent_id.is_empty() && human(&p.summary) && is_pushback(&p.summary)) {
        let Some(session) = p.session_id.as_deref() else {
            continue;
        };
        // The reply being answered: the newest before it, past any other
        // messages the user sent after that reply.
        let before = store.conversation_near(&p.host, session, p.id, false, 6)?;
        let Some(reply) = before.iter().find(|e| e.kind == "turn_end") else {
            continue;
        };
        let claims = claim_sentences(&reply.summary);
        if claims.is_empty() {
            continue;
        }
        let pushback = Pushback {
            id: p.id,
            ts: p.ts,
            host: p.host.clone(),
            session_id: session.to_string(),
            claim: claims.join(" "),
            words: spoken(&p.summary),
            reply: reply.id,
        };
        // Newest first, so an earlier answer to the same reply replaces a later one.
        match found.iter_mut().find(|f| f.reply == reply.id) {
            Some(f) => *f = pushback,
            None => found.push(pushback),
        }
    }
    Ok(found)
}

/// The newest request the user typed in a session, and when.
pub fn last_request(store: &Store, host: &str, session_id: &str) -> Result<Option<(String, i64)>> {
    let prompts = store.events(&EventQuery {
        session_id: Some(session_id),
        kinds: &["prompt"],
        limit: 20,
        ..Default::default()
    })?;
    Ok(prompts
        .into_iter()
        .find(|p| p.host == host && p.agent_id.is_empty() && human(&p.summary))
        .map(|p| (spoken(&p.summary), p.ts)))
}

/// Run at the end of a main-agent turn whose reply reports a result. When
/// code changed this turn and nothing ran after the last change, or changed
/// and the user pushed back on a similar result before, returns the reason
/// to send the agent back once to say what it verified. At most once per
/// request.
pub fn claim_check(
    store: &Store,
    host: &str,
    session_id: &str,
    project: &str,
    reply: &str,
    now: i64,
) -> Result<Option<String>> {
    let claims = claim_sentences(reply);
    if claims.is_empty() {
        return Ok(None);
    }
    let recent =
        store.events(&EventQuery { session_id: Some(session_id), limit: MAX_CANDIDATES, ..Default::default() })?;
    // This turn: everything since the user's request. Without captured text
    // every request is blank, and any one marks where the turn began.
    let mut turn = Vec::new();
    let mut request = 0;
    for e in recent.into_iter().filter(|e| e.host == host) {
        if e.kind == "prompt" && e.agent_id.is_empty() && (e.summary.is_empty() || is_typed_prompt(&e.summary)) {
            // The Codex app asking for a suggested next message: its reply is
            // a suggestion, not a result.
            if e.summary.starts_with(SUGGESTION_PREAMBLE) {
                return Ok(None);
            }
            request = e.id;
            break;
        }
        turn.push(e);
    }
    let mut gaps = Vec::new();
    let last_change = turn.iter().position(|e| {
        matches!(e.kind.as_str(), "edit" | "write") && e.ok != Some(false) && e.files.iter().any(|f| is_code(f))
    });
    if let Some(i) = last_change {
        let ran_after = turn[..i].iter().any(|e| matches!(e.kind.as_str(), "command" | "test"));
        if !ran_after && !hedged(reply) {
            let file = turn[i].files.iter().find(|f| is_code(f)).map(String::as_str).unwrap_or_default();
            let name = file.rsplit(['/', '\\']).next().unwrap_or(file);
            gaps.push(format!("No command or test ran after the last change to {name}."));
        }
    }
    // Only a turn that changed code can have produced a new result; without
    // one, a reply that sounds like an old claim is talking about it.
    let similar = match last_change {
        Some(_) => {
            let said = terms(&claims.join(" "));
            pushbacks(store, project, now - PUSHBACK_DAYS * DAY_MS)?.into_iter().find(|p| {
                let theirs = terms(&format!("{} {}", p.claim, p.words));
                said.iter().filter(|t| !CLAIM_WORDS.contains(&t.as_str()) && theirs.contains(*t)).count() >= 2
            })
        }
        None => None,
    };
    if let Some(p) = similar {
        gaps.push(format!(
            "{}, the user pushed back on a similar claim in this project: \"{}\" was answered \"{}\".",
            capitalized(&ago(now, p.ts)),
            truncate(&p.claim, 160),
            truncate(&p.words, 160)
        ));
    }
    if gaps.is_empty() {
        return Ok(None);
    }
    let reason = format!(
        "CSM claim check: your reply says \"{}\". {} Before you finish, add a short closing note: what exactly you \
         verified and how, the claim limited to that evidence, and what is still unverified. If the reply already \
         says all that, confirm it in one line.",
        truncate(&claims[0], 200),
        gaps.join(" ")
    );
    let trigger = format!("claim:{request}");
    if !store.claim_injection(project, host, session_id, "", &trigger, None, &reason, &[], now)? {
        return Ok(None);
    }
    Ok(Some(reason))
}

/// Words every report of a result uses; sharing them says nothing about
/// what the result was about.
const CLAIM_WORDS: &[&str] = &["pass", "passe", "test", "check", "verifi", "confirm", "resolv", "land", "longer"];
const PUSHBACK_DAYS: i64 = 30;

/// A change that something could be run against: not prose, not scratch.
fn is_code(path: &str) -> bool {
    let lower = path.to_ascii_lowercase().replace('\\', "/");
    let prose = [".md", ".markdown", ".txt", ".rst", ".adoc"].iter().any(|ext| lower.ends_with(ext));
    !prose && !lower.contains("/scratchpad/")
}

fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// Earlier requests in `project` from other sessions that look like
/// `prompt`, each with the reply it got and what the user said next. A
/// session is shown each earlier request once.
pub fn earlier_requests(
    store: &Store,
    host: &str,
    session_id: &str,
    project: &str,
    prompt: &str,
    now: i64,
) -> Result<Option<String>> {
    if prompt.starts_with(SUGGESTION_PREAMBLE) {
        return Ok(None);
    }
    // A request of a word or two answers the session's own last question.
    let wanted = terms(&spoken(prompt));
    if wanted.len() < MIN_REQUEST_TERMS {
        return Ok(None);
    }
    let candidates: Vec<Event> = store
        .events(&EventQuery {
            project: Some(project),
            kinds: &["prompt"],
            exclude_sessions: &[session_id],
            exclude_suggestion_sessions: true,
            since_ts: Some(now - RECALL_DAYS * DAY_MS),
            limit: MAX_CANDIDATES,
            ..Default::default()
        })?
        .into_iter()
        .filter(|e| e.agent_id.is_empty() && human(&e.summary))
        .collect();
    let sets: Vec<BTreeSet<String>> = candidates.iter().map(|e| terms(&spoken(&e.summary))).collect();
    let n = sets.len() as f64;
    let weight: BTreeMap<&str, f64> = wanted
        .iter()
        .map(|t| {
            let df = sets.iter().filter(|s| s.contains(t)).count() as f64;
            (t.as_str(), ((n + 1.0) / (df + 0.5)).ln())
        })
        .collect();
    // The best match in each session, best first.
    let mut best: Vec<(f64, &Event)> = Vec::new();
    for (e, set) in candidates.iter().zip(&sets) {
        let shared: Vec<f64> = set.iter().filter_map(|t| weight.get(t.as_str()).copied()).collect();
        let score: f64 = shared.iter().sum();
        if shared.len() < MIN_SHARED || score < MIN_SCORE {
            continue;
        }
        match best.iter_mut().find(|(_, b)| b.session_id == e.session_id) {
            Some(slot) if slot.0 < score => *slot = (score, e),
            Some(_) => {}
            None => best.push((score, e)),
        }
    }
    best.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.id.cmp(&a.1.id)));
    let mut shown = Vec::new();
    for (_, e) in best {
        if shown.len() == MAX_RECALLED {
            break;
        }
        if store.claim_injection(
            project,
            host,
            session_id,
            "",
            &format!("recall:{}", e.id),
            None,
            &e.summary,
            &[],
            now,
        )? {
            shown.push(e);
        }
    }
    if shown.is_empty() {
        return Ok(None);
    }
    shown.sort_by_key(|e| e.id);
    let mut lines = vec!["csm: earlier requests in this project that look related to this one, from other sessions \
         (the user's own words; they can be out of date):"
        .to_string()];
    for e in shown {
        let session = e.session_id.as_deref().unwrap_or_default();
        lines.push(format!(
            "  {}, {} session {}: \"{}\"",
            ago(now, e.ts),
            e.host,
            short_session(session),
            truncate(&spoken(&e.summary), 200)
        ));
        let after = store.conversation_near(&e.host, session, e.id, true, 6)?;
        let Some(reply) = after.iter().find(|a| a.kind == "turn_end" && !a.summary.is_empty()) else {
            continue;
        };
        lines.push(format!("    Reply: \"{}\"", truncate(&reply.summary, 200)));
        if let Some(next) = after.iter().find(|a| a.id > reply.id && a.kind == "prompt" && human(&a.summary)) {
            let label = if is_pushback(&next.summary) { "The user pushed back" } else { "Then the user" };
            lines.push(format!("    {label}: \"{}\"", truncate(&spoken(&next.summary), 160)));
        }
    }
    Ok(Some(lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::NewEvent;

    const P: &str = "C:/work/game";
    const NOW: i64 = 1_791_979_445_000;
    const MIN: i64 = 60_000;

    fn say(store: &Store, session: &str, kind: &str, text: &str, at: i64) {
        let e = NewEvent {
            project: P.into(),
            host: "claude".into(),
            session_id: Some(session.into()),
            kind: kind.into(),
            summary: text.into(),
            ..Default::default()
        };
        store.record_at(e, at).unwrap();
    }

    #[test]
    fn terms_drop_common_words_and_meet_in_the_middle() {
        let t: Vec<String> = terms("hey lets work on the mechs walking/running. Legs, knees!").into_iter().collect();
        assert_eq!(t, ["knee", "leg", "mech", "run", "walk"]);
        assert_eq!(terms("the leg runs"), terms("running legs"));
        assert!(terms("yes do it").is_empty());
    }

    #[test]
    fn pushback_and_claims_are_read_from_words() {
        for p in [
            "hey why did you lie to me? you never touched the legs",
            "LOL not fixed.",
            "yeah sure keep lying",
            "it didn't work, same bug",
            "you didn't do anything i've asked about the cartwheeling issue",
            "THE THING I ASKED YOU TO FIX BUT NO YOU LIED.",
        ] {
            assert!(is_pushback(p), "{p}");
        }
        for p in ["goddamm still running? :P", "the walk and jog look perfect", "is it still loading", "lie down"] {
            assert!(!is_pushback(p), "{p}");
        }
        let reply = "I fixed the running legs: the knees no longer kick out sideways. The clip above is the same run. \
                     You haven't played it yet. The A/D bug is not fixed yet.";
        assert_eq!(claim_sentences(reply), ["I fixed the running legs: the knees no longer kick out sideways"]);
        assert!(hedged(reply));
        assert!(claim_sentences("The EVADE trials are still running on all five trees.").is_empty());
        assert!(
            claim_sentences("Nothing in the game has changed. There are two candidate fixes for the legs.").is_empty()
        );
        assert!(claim_sentences("How it works on the phone\n1. Open the panel.").is_empty());
        assert_eq!(claim_sentences("Here is how it works: it works now."), ["Here is how it works: it works now"]);
        assert_eq!(
            claim_sentences("**All 207 native checks pass**; aiming is unchanged."),
            ["All 207 native checks pass; aiming is unchanged"]
        );
        assert_eq!(spoken("@\"C:\\Videos\\Screen 1.mp4\" hey why the legs"), "hey why the legs");
    }

    #[test]
    fn a_pushback_belongs_to_the_reply_it_answers() {
        let store = Store::open_in_memory().unwrap();
        say(&store, "s1", "prompt", "watch the legs too", NOW - 60 * MIN);
        say(&store, "s1", "turn_end", "I fixed the running legs: the knees no longer kick out.", NOW - 50 * MIN);
        say(&store, "s1", "prompt", "you never touched the legs", NOW - 40 * MIN);
        say(&store, "s1", "prompt", "yeah sure keep lying", NOW - 39 * MIN);
        say(&store, "s1", "turn_end", "Both bugs are not fixed yet; I am looking.", NOW - 30 * MIN);
        say(&store, "s1", "prompt", "still broken", NOW - 20 * MIN);
        let found = pushbacks(&store, P, NOW - 90 * MIN).unwrap();
        assert_eq!(found.len(), 1, "a reply that claims nothing has nothing to push back on: {found:?}");
        assert_eq!(found[0].claim, "I fixed the running legs: the knees no longer kick out");
        assert_eq!(found[0].words, "you never touched the legs", "the first answer to the reply");
    }

    #[test]
    fn recalls_related_requests_from_other_sessions_once() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..30 {
            say(&store, &format!("noise{i}"), "prompt", &format!("tune the portal refill number {i}"), NOW - 600 * MIN);
        }
        say(
            &store,
            "old",
            "prompt",
            "the walk and jog look perfect, the running legs needs a lil more love",
            NOW - 500 * MIN,
        );
        say(&store, "old", "turn_end", "Softened the running leg's final extension into the plant.", NOW - 490 * MIN);
        say(&store, "old", "prompt", "LOL not fixed", NOW - 480 * MIN);
        let ask = "watch the running legs too";
        say(&store, "now", "prompt", ask, NOW);

        let text = earlier_requests(&store, "claude", "now", P, ask, NOW).unwrap().expect("recalled");
        assert!(
            text.contains(
                "claude session old: \"the walk and jog look perfect, the running legs needs a lil more love\""
            ),
            "{text}"
        );
        assert!(text.contains("    Reply: \"Softened the running leg's final extension into the plant.\""), "{text}");
        assert!(text.contains("    The user pushed back: \"LOL not fixed\""), "{text}");
        assert!(!text.contains("portal"), "{text}");
        let again = "watch the running legs again";
        assert!(earlier_requests(&store, "claude", "now", P, again, NOW + MIN).unwrap().is_none(), "once");
        assert!(earlier_requests(&store, "claude", "new", P, again, NOW + MIN).unwrap().is_some(), "per session");
        let one = "the legs squeak, oil them";
        assert!(earlier_requests(&store, "claude", "now2", P, one, NOW).unwrap().is_none(), "one word in common");
        let common = "tune the portal refill";
        assert!(earlier_requests(&store, "claude", "other", P, common, NOW).unwrap().is_none(), "too common");
        let short = "running legs";
        assert!(
            earlier_requests(&store, "claude", "now3", P, short, NOW).unwrap().is_none(),
            "an answer, not a request"
        );
        let suggestion = format!("{SUGGESTION_PREAMBLE} Last request: watch the running legs");
        assert!(earlier_requests(&store, "codex", "sug", P, &suggestion, NOW).unwrap().is_none(), "not the user");
    }

    fn touch(store: &Store, session: &str, kind: &str, file: &str, at: i64) {
        let e = NewEvent {
            project: P.into(),
            host: "claude".into(),
            session_id: Some(session.into()),
            kind: kind.into(),
            summary: file.into(),
            files: vec![file.into()],
            ok: Some(true),
            ..Default::default()
        };
        store.record_at(e, at).unwrap();
    }

    #[test]
    fn a_result_claimed_after_an_unrun_change_is_sent_back_once() {
        let store = Store::open_in_memory().unwrap();
        say(&store, "s1", "prompt", "the dash is too short", NOW - 10 * MIN);
        touch(&store, "s1", "edit", "src/dash.rs", NOW - 9 * MIN);
        touch(&store, "s1", "command", "cargo test", NOW - 8 * MIN);
        touch(&store, "s1", "edit", "src/frame_visual.gd", NOW - 7 * MIN);
        touch(&store, "s1", "edit", "notes/plan.md", NOW - 6 * MIN);
        let reply = "Fixed: the dash now covers twice the distance.";
        let reason = claim_check(&store, "claude", "s1", P, reply, NOW).unwrap().expect("sent back");
        assert!(
            reason.starts_with("CSM claim check: your reply says \"Fixed: the dash now covers twice the distance\". ")
        );
        assert!(reason.contains("No command or test ran after the last change to frame_visual.gd."), "{reason}");
        assert!(claim_check(&store, "claude", "s1", P, reply, NOW).unwrap().is_none(), "once per request");

        say(&store, "s1", "prompt", "and the slide", NOW - 5 * MIN);
        touch(&store, "s1", "edit", "src/slide.rs", NOW - 4 * MIN);
        let hedged = "The slide now works the same way. I haven't run it yet.";
        assert!(claim_check(&store, "claude", "s1", P, hedged, NOW).unwrap().is_none(), "the reply says so");
        let plain = "The slide code is in place; next is the jump.";
        assert!(claim_check(&store, "claude", "s1", P, plain, NOW).unwrap().is_none(), "no result claimed");
        touch(&store, "s1", "test", "cargo test", NOW - 3 * MIN);
        assert!(claim_check(&store, "claude", "s1", P, "The slide is fixed.", NOW).unwrap().is_none(), "it ran");

        say(&store, "sug", "prompt", &format!("{SUGGESTION_PREAMBLE} Recent: the slide"), NOW - 2 * MIN);
        touch(&store, "sug", "edit", "src/slide.rs", NOW - MIN);
        let suggested = "Check the slide is fixed";
        assert!(claim_check(&store, "claude", "sug", P, suggested, NOW).unwrap().is_none(), "a suggested message");
    }

    #[test]
    fn a_claim_like_one_the_user_pushed_back_on_is_sent_back_even_when_tested() {
        let store = Store::open_in_memory().unwrap();
        const DAYS_3: i64 = 3 * 24 * 60 * MIN;
        say(&store, "old", "prompt", "the running legs need love", NOW - DAYS_3 - 2 * MIN);
        say(&store, "old", "turn_end", "I fixed the running legs: the knees no longer kick out.", NOW - DAYS_3 - MIN);
        say(&store, "old", "prompt", "you never touched the legs", NOW - DAYS_3);
        say(&store, "new", "prompt", "legs again", NOW - 10 * MIN);
        touch(&store, "new", "edit", "src/legs.gd", NOW - 9 * MIN);
        touch(&store, "new", "test", "godot --test", NOW - 8 * MIN);
        let reply = "The running legs are fixed: knees stay under the hips. All 12 checks pass.";
        let reason = claim_check(&store, "claude", "new", P, reply, NOW).unwrap().expect("sent back");
        assert!(!reason.contains("No command"), "{reason}");
        assert!(
            reason.contains(
                "3d ago, the user pushed back on a similar claim in this project: \"I fixed the running legs: the \
                 knees no longer kick out\" was answered \"you never touched the legs\"."
            ),
            "{reason}"
        );
        say(&store, "new", "prompt", "and the tests?", NOW - 5 * MIN);
        touch(&store, "new", "edit", "src/knee_check.gd", NOW - 4 * MIN);
        touch(&store, "new", "test", "godot --test", NOW - 3 * MIN);
        let other = "The knee check passes and the build is fixed.";
        assert!(claim_check(&store, "claude", "new", P, other, NOW).unwrap().is_none(), "one word in common");

        // A turn that changed no code reports no new result, however it is worded.
        say(&store, "new", "prompt", "what can the csm not cover?", NOW - 2 * MIN);
        touch(&store, "new", "edit", "notes/review.md", NOW - MIN);
        for past in [
            "What it can't cover is the 11-05 kind of off day, where I said the running legs were fixed and they weren't.",
            "The running legs fix works here, but the knees piece is missing from what a session sees.",
        ] {
            assert!(claim_check(&store, "claude", "new", P, past, NOW).unwrap().is_none(), "{past}");
        }
    }

    #[test]
    fn last_request_is_the_users_own() {
        let store = Store::open_in_memory().unwrap();
        say(&store, "s1", "prompt", "@\"C:/shots/a.png\" make the parser handle unicode", NOW - 3 * MIN);
        say(&store, "s1", "prompt", "<task-notification>done</task-notification>", NOW - 2 * MIN);
        let sub = NewEvent {
            project: P.into(),
            host: "claude".into(),
            session_id: Some("s1".into()),
            agent_id: "a1".into(),
            kind: "prompt".into(),
            summary: "Explore the parser".into(),
            ..Default::default()
        };
        store.record_at(sub, NOW - MIN).unwrap();
        let (text, ts) = last_request(&store, "claude", "s1").unwrap().unwrap();
        assert_eq!((text.as_str(), ts), ("make the parser handle unicode", NOW - 3 * MIN));
        assert!(last_request(&store, "codex", "s1").unwrap().is_none());
    }
}
