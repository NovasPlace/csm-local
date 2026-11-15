//! What the user asked before. When the user types a request, earlier
//! requests in the project that share its rarer words are shown with the
//! reply each got and what the user said next, so a session picks up where
//! the last one on the same thing left off instead of guessing again.
//!
//! A request that answers a reported result with "not fixed" or "you never
//! touched the legs" is pushback on that result; the briefing and the claim
//! check read those. Everything here is read from the journal, which keeps
//! requests and replies for [`RECALL_DAYS`].
//!
//! Saved memories come up the same way, so a session need not search for
//! them: the ones that share a request's rarer words when the user types it,
//! and the ones that name a file when an agent is about to change it. And a
//! session that has worked a while and saved nothing is sent back once to
//! save what the next one needs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::brief::is_typed_prompt;
use crate::decisions::short_session;
use crate::journal::{Event, EventQuery, SUGGESTION_PREAMBLE};
use crate::store::{Filter, Memory, Store, MAX_CARD_BYTES};
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
const MAX_MEMORY_CANDIDATES: usize = 1_000;
const MAX_MEMORIES: usize = 3;
const MEMORY_BYTES: usize = 300;
/// Of a long memory, how much of its opening is shown before the part that
/// matched, and how much before the match itself.
const LEAD_BYTES: usize = 80;
const CONTEXT_BYTES: usize = 40;
/// What a memory must score against a request, in words only it has and
/// says once.
const MIN_MEMORY_SCORE: f64 = 1.5;
/// How rare the most particular word shared has to be, against a word only
/// one memory has: at 0.4, in about one memory in six of a hundred.
const MIN_PARTICULAR: f64 = 0.4;
/// How much of a memory counts as its opening: about a sentence.
const OPENING_BYTES: usize = 120;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;
/// How many requests a session takes, changing code and saving nothing,
/// before it is sent back to save: enough that there is something to keep,
/// few enough that most sessions that never save are asked once.
const NUDGE_AFTER_REQUESTS: usize = 4;
const SAVE_NUDGE: &str = "nudge:save";

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

/// Everyday words, and the everyday words of working with a coding agent,
/// stemmed as [`terms`] stems them: they say what a request or a memory
/// does, not which of a project's things it is about. A memory that shares
/// only these with a request shares nothing in particular.
static EVERYDAY: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    "ability above accept access account across act action active actual actually address admit adult affect afraid \
     afternoon age ago agree ahead aim air alive allow almost alone along although always amount angle angry animal \
     answer anymore anyone anyway apart appear apply area argue arm around arrive art attack attention avoid aware \
     away awesome baby bag ball bar base basic basically beat beautiful became become bed began begin begun behind \
     believe below bend bent beside between beyond big bigger bind black block blow blue board boat body book born \
     bottom box boy brain break bright bring broke broken brother brought brown build building built burn busy buy \
     came camera car card care carry case catch caught cause center central certain chance change character charge \
     chase check child children choice choose chose chosen city claim class clean clear clearly click climb clock \
     close closer cold color combine common company compare complete completely concern condition connect consider \
     contain continue control cook copy corner correct cost count country couple course cover crazy create cross \
     cry current currently cut dark data date day dead deal dear death decide deep definitely degree deliver depend \
     describe design detail develop die difference different difficult direct direction directly discover discuss \
     distance doctor dog door double doubt down draw drawn dream dress drew drink drive driven drop drove dry due \
     during early earth easier easily east easy eat edge effect effort eight either else empty end enemy energy \
     enjoy enough enter entire entirely especially evening event eventually ever everyone exact exactly example \
     except excite exist expect experience explain extra eye face fact fail fair fall fallen false family far fast \
     faster father fault favorite fear feel feeling feet fell felt fight figure fill final finally find fine finger \
     finish fire first fit five flat floor flow fly focus follow food foot force forget forgot form forward found \
     four free fresh friend front full fully fun funny future gain game garden gave general generally girl given \
     glad glass goal gone grab gray green grew ground group grow guard guy hair half hand handle hang happen happy \
     hard harder hate head health hear heard heart heat heavy held hello help hide high higher highly hill history \
     hit hold hole home honest honestly hope hopefully horse hot hour house huge human hundred hurt ice image \
     imagine important improve include increase indeed information inside instead interest involve island item \
     job join joke jump key kid kill kept king knew knock known lack lady land language large last late later \
     laugh law lay lead learn least leave led left leg length less letter level lie life lift light likely limit \
     line link list listen live load local long longer lose loss lost loud love low lower luck lunch machine main \
     major man manage many map mark market match matter mean meant measure meet member men mention mess message \
     met middle might mile mind mine minute miss mistake mix mode model moment money month moon morning mother \
     mountain mouth move movement movie music must myself name nature near nearly neck neither never next night \
     nine nobody noise none normal normally north nose note nothing notice number object obvious obviously offer \
     office often open opinion option order original otherwise ourselves outside page paid pain paint pair paper \
     parent park party pass past path pause pay people per perfect perhaps period person phone pick picture piece \
     place plan plant play player plenty plus pocket police poor pop position possible possibly post power practice \
     prepare present press price probably problem process produce proper properly protect prove provide pull \
     purpose push put quality question quick quickly quiet quite race rain raise ran range rate rather reach read \
     ready realize reason receive recent recently record red reduce refer regular relate release remain remember \
     remove repeat replace reply report require rest result return rich ride rise river road rock roll room round \
     row rule run rush sad sadly safe sat save saw scale scene school sea search season seat second section seen \
     self sell send sense sent serious seriously serve service set settle seven several shake shape share sharp \
     ship shoot shop short shot show shown shut sick side sign silly similar simple simply since sing single sir \
     sister sit site situation six size skill skin sky sleep slide slightly slow slowly small smaller smart smell \
     smile smooth snow soft sold solid solve someone sometimes son song soon sorry sort sound south space speak \
     special speed spend spent spin spoke spot spread spring square stable stage stand star start state station \
     stay step stick stone stood stop store storm story straight strange street strength strike strong struggle \
     student study stupid style subject success sudden suggest summer sun super supply support suppose surface \
     surprise sweet swim switch system table take taken talk tall target task taste taught teach team ten term \
     terrible test thank themselves thick thin third thousand three threw throw thrown tie time tiny tired today \
     together told tomorrow tonight took top total totally touch tough toward town track trade train travel treat \
     tree trip trouble true trust truth turn twice two type understand unit unless until update upon upper usage \
     useful user usual usually value version via video view visit voice vote walk wall war warm waste watch water \
     wear weather week weight weird went west wheel whatever white whole wide wife wild win wind window winter wish \
     within without woke woman women won wonder wood word wore world worry worse worst worth write written wrong \
     wrote yard year yellow yesterday young yourself zero \
     agent api app branch bug button claude code codex command commit config csm debug default doc docs error \
     feature file folder function handoff install log memory menu merge odd output project prompt readme repo \
     screen screenshot script server session setting setup subagent terminal tool variable verify"
        .split_whitespace()
        .map(stem)
        .collect()
});

/// Distinctive words of `text`, lower case and roughly stemmed, so
/// "running legs" and "the leg runs" share "run" and "leg".
pub fn terms(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter_map(term).collect()
}

/// `word` as [`terms`] would keep it, if it would.
fn term(word: &str) -> Option<String> {
    Some(word.to_lowercase())
        .filter(|w| w.chars().count() >= 3 && !w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| !STOPWORDS.contains(w.as_str()))
        .map(|w| stem(&w))
        .filter(|w| !STOPWORDS.contains(w.as_str()))
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
    /// The model that reported the result, when known.
    pub model: Option<String>,
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
            model: reply.model.clone(),
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

/// Run at the end of a main-agent turn the claim check let through. When
/// the session has taken [`NUDGE_AFTER_REQUESTS`] requests and changed code
/// since it last saved anything for later sessions, or was last sent back
/// for it, returns the reason to send it back once: to save what a new
/// session could not see in the code, or to say there is nothing.
///
/// `sees_saves` is whether the hooks see the session's own saves, which
/// they do when they see every tool call. Without it a save by anyone in
/// the project counts, so a session is never told it saved nothing when it
/// did.
pub fn save_nudge(
    store: &Store,
    host: &str,
    session_id: &str,
    project: &str,
    sees_saves: bool,
    now: i64,
) -> Result<Option<String>> {
    let mut since = store.injected_at(host, session_id, "", SAVE_NUDGE)?.unwrap_or(0);
    if !sees_saves {
        since = since.max(store.last_saved(project)?.unwrap_or(0));
    }
    let events: Vec<Event> = store
        .events(&EventQuery {
            session_id: Some(session_id),
            kinds: &["prompt", "edit", "write", "delete", "save"],
            since_ts: Some(since + 1),
            limit: MAX_CANDIDATES,
            ..Default::default()
        })?
        .into_iter()
        .filter(|e| e.host == host)
        // Newest first: what came after the session's last save.
        .take_while(|e| e.kind != "save")
        .collect();
    let requests = events
        .iter()
        .filter(|e| e.kind == "prompt" && e.agent_id.is_empty() && (e.summary.is_empty() || human(&e.summary)))
        .count();
    let changed: BTreeSet<&str> = events
        .iter()
        .filter(|e| e.kind != "prompt" && e.ok != Some(false))
        .flat_map(|e| &e.files)
        .map(String::as_str)
        .filter(|file| is_code(file))
        .collect();
    if requests < NUDGE_AFTER_REQUESTS || changed.is_empty() {
        return Ok(None);
    }
    let card = match store.card(project)? {
        Some(card) => format!(
            "If the project card (memory #{}) no longer says what is true, save a new one with csm_save kind \
             \"card\".",
            card.id
        ),
        None => format!(
            "This project has no project card yet: save one with csm_save kind \"card\" (what the project is, \
             where things live, how to build, test and run it; at most {MAX_CARD_BYTES} bytes)."
        ),
    };
    let files = if changed.len() == 1 { "1 code file".to_string() } else { format!("{} code files", changed.len()) };
    let reason = format!(
        "CSM save nudge: this session has taken {requests} requests and changed {files} since it last saved \
         anything for later sessions. Before you finish, save what a new session could not see in the code: a \
         lesson, a trap or a fact with csm_save (one idea each), where the work stands with csm_checkpoint, a \
         standing decision the user made with csm_decide. {card} If nothing is worth keeping, say so in one \
         line. Either way, do not repeat your reply."
    );
    if !store.claim_injection(project, host, session_id, "", SAVE_NUDGE, Some(1), &reason, &[], now)? {
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
    let weight = rarity(&wanted, &sets);
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

/// How much each of `wanted` says when shared: the fewer of `sets` have a
/// word, the more.
fn rarity<'a>(wanted: &'a BTreeSet<String>, sets: &[BTreeSet<String>]) -> BTreeMap<&'a str, f64> {
    let n = sets.len() as f64;
    wanted
        .iter()
        .map(|t| {
            let df = sets.iter().filter(|s| s.contains(t)).count() as f64;
            (t.as_str(), ((n + 1.0) / (df + 0.5)).ln())
        })
        .collect()
}

/// A saved memory that looks like a request.
pub struct Related {
    pub memory: Memory,
    /// In words only this memory has and says once: 2.0 is two of them.
    pub score: f64,
    /// The request's words the memory has, weightiest first.
    pub shared: Vec<String>,
}

/// Saved memories in `project` that look like `prompt`, best first. Words
/// count the way a search engine counts them (BM25): one few memories have
/// says more, one the memory keeps coming back to says more, and one that
/// turns up once in a long memory says little. A match shares at least two
/// of the request's words and scores [`MIN_MEMORY_SCORE`].
pub fn memories_like(store: &Store, project: &str, prompt: &str) -> Result<Vec<Related>> {
    if prompt.starts_with(SUGGESTION_PREAMBLE) {
        return Ok(Vec::new());
    }
    let wanted = terms(&spoken(prompt));
    if wanted.len() < MIN_REQUEST_TERMS {
        return Ok(Vec::new());
    }
    let memories = store.list(Filter { project: Some(project), kind: None, limit: MAX_MEMORY_CANDIDATES })?;
    let counts: Vec<BTreeMap<String, f64>> = memories
        .iter()
        .map(|m| {
            let mut counts = BTreeMap::new();
            let text = format!("{} {}", m.content, m.tags.join(" "));
            for word in text.split(|c: char| !c.is_alphanumeric()).filter_map(term) {
                *counts.entry(word).or_insert(0.0) += 1.0;
            }
            counts
        })
        .collect();
    let sets: Vec<BTreeSet<String>> = counts.iter().map(|c| c.keys().cloned().collect()).collect();
    let weight = rarity(&wanted, &sets);
    // What a word only one memory has is worth: scores are in units of it, so
    // a handful of memories and a thousand are held to the same standard.
    let rarest = ((sets.len() as f64 + 1.0) / 1.5).ln();
    let lengths: Vec<f64> = counts.iter().map(|c| c.values().sum()).collect();
    let average = (lengths.iter().sum::<f64>() / lengths.len().max(1) as f64).max(1.0);
    let mut found = Vec::new();
    for ((memory, counts), length) in memories.into_iter().zip(&counts).zip(&lengths) {
        let crowding = BM25_K1 * (1.0 - BM25_B + BM25_B * length / average);
        let opening =
            terms(&format!("{} {}", truncate(&one_line(&memory.content), OPENING_BYTES), memory.tags.join(" ")));
        let mut shared: Vec<(f64, &str)> = weight
            .iter()
            .filter_map(|(word, weight)| {
                // A memory is about the words it repeats or opens with; one
                // it says once in passing is as likely chance.
                let said = *counts.get(*word).filter(|said| **said >= 2.0 || opening.contains(*word))?;
                Some((weight * said * (BM25_K1 + 1.0) / (said + crowding) / rarest, *word))
            })
            .collect();
        let score: f64 = shared.iter().map(|s| s.0).sum();
        // One of them has to be a thing of the project's, and not one most
        // of its memories mention.
        let particular =
            shared.iter().any(|(_, word)| weight[word] >= MIN_PARTICULAR * rarest && !EVERYDAY.contains(*word));
        if shared.len() >= MIN_SHARED && score >= MIN_MEMORY_SCORE && particular {
            shared.sort_by(|a, b| b.0.total_cmp(&a.0));
            found.push(Related { memory, score, shared: shared.into_iter().map(|s| s.1.to_string()).collect() });
        }
    }
    found.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.memory.id.cmp(&a.memory.id)));
    Ok(found)
}

/// The memories [`memories_like`] finds for `prompt`, each with the part of
/// it that has the shared words. [`unseen`] says which a session is given.
pub fn related_memories(
    store: &Store,
    host: &str,
    session_id: &str,
    project: &str,
    prompt: &str,
    now: i64,
) -> Result<Option<String>> {
    let found = memories_like(store, project, prompt)?.into_iter().map(|related| {
        let text = one_line(&related.memory.content);
        let marks: Vec<usize> = text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| term(word).is_some_and(|t| related.shared.contains(&t)))
            .map(|word| word.as_ptr() as usize - text.as_ptr() as usize)
            .collect();
        let shown = passage(&text, &marks);
        (related.memory, shown)
    });
    let shown = unseen(store, host, session_id, "", project, found, now)?;
    Ok(notice("look related to this request", &shown))
}

/// Saved memories in `project` that name one of `paths`, the files an agent
/// is about to change: the most important first, and of those the shortest,
/// which is the most about the file. A subagent is shown them too: it starts
/// without its parent's briefing.
pub fn memories_naming(
    store: &Store,
    host: &str,
    session_id: &str,
    agent_id: &str,
    project: &str,
    paths: &[String],
    now: i64,
) -> Result<Option<String>> {
    let mut named: Vec<(Memory, String)> = Vec::new();
    for path in paths {
        let Some(name) = path.rsplit('/').next().filter(|n| !n.is_empty()) else {
            continue;
        };
        for m in store.memories_containing(project, name, MAX_MEMORY_CANDIDATES)? {
            if named.iter().any(|(n, _)| n.id == m.id) {
                continue;
            }
            let text = one_line(&m.content);
            let marks = mentions(&text, path);
            let elsewhere = format!("{}\n{}", m.source.as_deref().unwrap_or(""), m.tags.join(" "));
            if marks.is_empty() && mentions(&elsewhere, path).is_empty() {
                continue;
            }
            let shown = passage(&text, &marks);
            named.push((m, shown));
        }
    }
    named.sort_by(|(a, _), (b, _)| {
        b.importance.cmp(&a.importance).then(a.content.len().cmp(&b.content.len())).then(b.id.cmp(&a.id))
    });
    let shown = unseen(store, host, session_id, agent_id, project, named.into_iter(), now)?;
    Ok(notice("name a file this edit changes", &shown))
}

/// Where `text` names the file at `path`: the file's name stands alone
/// there, and any folders written in front of it are the ones it is in.
/// `melee.gd` is not named by `proteus_melee.gd`, by `melee.gd.uid`, or by
/// `tools/melee.gd` when the file is `scripts/melee.gd`.
fn mentions(text: &str, path: &str) -> Vec<usize> {
    // Neither changes a length, so a place in this is the same place in `text`.
    let text = text.to_ascii_lowercase().replace('\\', "/");
    let path = path.to_ascii_lowercase();
    let mut folders: Vec<&str> = path.split('/').collect();
    let Some(name) = folders.pop().filter(|n| !n.is_empty()) else {
        return Vec::new();
    };
    let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-');
    let named = |at: usize| {
        let (before, after) = (&text[..at], &text[at + name.len()..]);
        if before.chars().next_back().is_some_and(|c| word(c) || c == '.') {
            return false;
        }
        let mut rest = after.chars();
        match rest.next() {
            Some(c) if word(c) => return false,
            Some('.') if rest.next().is_some_and(word) => return false,
            _ => {}
        }
        let Some(written) = before.strip_suffix('/') else {
            return true;
        };
        let start = written
            .rfind(|c: char| !(word(c) || matches!(c, '.' | '/' | ':')))
            .map_or(0, |i| i + written[i..].chars().next().map_or(1, char::len_utf8));
        written[start..]
            .rsplit('/')
            .take_while(|folder| !matches!(*folder, "" | "." | ".."))
            .zip(folders.iter().rev())
            .all(|(written, real)| written == *real)
    };
    text.match_indices(name).map(|(at, _)| at).filter(|&at| named(at)).collect()
}

/// What to show of a memory, on one line: all of a short one; of a long one
/// how it opens and then the stretch where the most of `marks`, the places
/// in it that matched, fall.
fn passage(text: &str, marks: &[usize]) -> String {
    if text.len() <= MEMORY_BYTES {
        return text.to_string();
    }
    let room = MEMORY_BYTES - LEAD_BYTES;
    let reach = room - CONTEXT_BYTES;
    let within = |from: usize| marks.iter().filter(|&&m| m >= from && m < from + reach).count();
    // The earliest of the stretches with the most marks.
    let best = marks.iter().copied().max_by(|a, b| within(*a).cmp(&within(*b)).then(b.cmp(a)));
    let Some(from) = best.filter(|from| from + reach > MEMORY_BYTES) else {
        return truncate(text, MEMORY_BYTES);
    };
    // A few words before it, from the start of a word.
    let mut start = from - CONTEXT_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let start = text[start..from].find(' ').map_or(from, |space| start + space + 1);
    format!("{} {}", truncate(text, LEAD_BYTES), truncate(&text[start..], room))
}

/// Up to [`MAX_MEMORIES`] of `memories`, in the order given, that the
/// session (or its subagent `agent_id`) has not been given since it last
/// compacted, in a briefing or here. Compacting can drop them from what the
/// agent remembers, so they may be shown again after it.
fn unseen(
    store: &Store,
    host: &str,
    session_id: &str,
    agent_id: &str,
    project: &str,
    memories: impl Iterator<Item = (Memory, String)>,
    now: i64,
) -> Result<Vec<(Memory, String)>> {
    let compacted = store
        .events(&EventQuery { session_id: Some(session_id), kinds: &["compact"], limit: 1, ..Default::default() })?
        .first()
        .map_or(0, |e| e.ts);
    let mut shown = Vec::new();
    for (m, text) in memories {
        if shown.len() == MAX_MEMORIES {
            break;
        }
        let source = format!("memory:{}", m.id);
        if store.was_given(host, session_id, agent_id, &source, compacted)? {
            continue;
        }
        // Claimed again only when the last time was before the compaction.
        let window = Some(now - compacted + 1);
        let trigger = format!("recall:{source}");
        if store.claim_injection(project, host, session_id, agent_id, &trigger, window, &text, &[source], now)? {
            shown.push((m, text));
        }
    }
    Ok(shown)
}

/// `memories`, each with what to show of it, as a notice saying they `why`;
/// nothing without any.
fn notice(why: &str, memories: &[(Memory, String)]) -> Option<String> {
    if memories.is_empty() {
        return None;
    }
    let mut lines = vec![format!(
        "csm: saved memories in this project that {why} (notes from earlier sessions; they can be out of date):"
    )];
    for (m, text) in memories {
        let saved = m.created_at.get(..10).unwrap_or(&m.created_at);
        lines.push(format!("  #{} [{}] {saved}: {text}", m.id, m.kind));
    }
    Some(lines.join("\n"))
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

    fn remember(store: &Store, kind: &str, importance: i64, content: &str, source: Option<&str>) -> i64 {
        let new = crate::store::NewMemory {
            project: P.into(),
            kind: kind.into(),
            content: content.into(),
            tags: Vec::new(),
            importance,
            source: source.map(str::to_string),
            model: None,
        };
        store.save(new).unwrap().memory.id
    }

    #[test]
    fn a_request_brings_up_the_memories_that_share_its_rarer_words() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..20 {
            remember(&store, "note", 3, &format!("Portal refill number {i} was tuned against the hangar timer."), None);
        }
        let knees = remember(
            &store,
            "lesson",
            4,
            "The running legs looked wrong because the knees were driven by the walk curve; drive them from stride.",
            None,
        );
        let ask = "watch the running legs and the knees again";
        let text = related_memories(&store, "claude", "s1", P, ask, NOW).unwrap().expect("recalled");
        assert!(text.starts_with("csm: saved memories in this project that look related to this request"), "{text}");
        assert!(text.contains(&format!("  #{knees} [lesson] ")), "{text}");
        assert!(text.contains("the knees were driven by the walk curve"), "{text}");
        assert!(!text.contains("Portal"), "{text}");
        assert!(related_memories(&store, "claude", "s1", P, ask, NOW + MIN).unwrap().is_none(), "once");
        assert!(related_memories(&store, "claude", "s2", P, ask, NOW + MIN).unwrap().is_some(), "per session");

        // Compacting can lose it, so it may come up again afterwards.
        say(&store, "s1", "compact", "compact", NOW + 2 * MIN);
        assert!(related_memories(&store, "claude", "s1", P, ask, NOW + 3 * MIN).unwrap().is_some(), "after compacting");
        assert!(related_memories(&store, "claude", "s1", P, ask, NOW + 4 * MIN).unwrap().is_none(), "and once again");

        // A session whose briefing showed the memory already has it.
        store
            .claim_injection(P, "claude", "s3", "", "startup", None, "briefing", &[format!("memory:{knees}")], NOW)
            .unwrap();
        assert!(related_memories(&store, "claude", "s3", P, ask, NOW + MIN).unwrap().is_none(), "in the briefing");

        let common = "tune the portal refill number for the hangar timer";
        assert!(related_memories(&store, "claude", "s4", P, common, NOW).unwrap().is_none(), "words every memory has");
        let one = "the knees squeak, oil them";
        assert!(related_memories(&store, "claude", "s5", P, one, NOW).unwrap().is_none(), "one word in common");
        assert!(related_memories(&store, "claude", "s6", P, "running legs", NOW).unwrap().is_none(), "an answer");
        assert!(related_memories(&store, "claude", "s7", "C:/work/other", ask, NOW).unwrap().is_none(), "this project");
    }

    #[test]
    fn at_most_three_memories_come_up_at_once_best_first() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..20 {
            remember(&store, "note", 3, &format!("Portal refill number {i} was tuned against the hangar timer."), None);
        }
        let weak = remember(&store, "note", 3, "Cartwheel recovery uses the gyro.", None);
        let ids: Vec<i64> = (0..4)
            .map(|i| remember(&store, "fact", 3, &format!("Cartwheel recovery gyro torque pass {i} landed."), None))
            .collect();
        let ask = "the cartwheel recovery still overshoots, check the gyro torque";
        let text = related_memories(&store, "claude", "s1", P, ask, NOW).unwrap().expect("recalled");
        assert_eq!(text.lines().count(), 4, "{text}");
        assert!(!text.contains(&format!("#{weak} ")), "the fuller matches first: {text}");
        let rest = related_memories(&store, "claude", "s1", P, ask, NOW + MIN).unwrap().expect("the rest");
        let shown = |text: &str| ids.iter().filter(|id| text.contains(&format!("#{id} "))).count();
        assert_eq!((shown(&text), shown(&rest)), (3, 1), "{text}\n{rest}");
        assert!(rest.contains(&format!("#{weak} ")), "{rest}");
    }

    #[test]
    fn a_file_is_named_by_its_own_name_in_its_own_folders() {
        let path = "C:/work/game/scripts/melee.gd";
        for text in [
            "melee.gd settles too early",
            "See scripts/melee.gd _low_clip.",
            "(scripts\\melee.gd)",
            "`game/scripts/melee.gd`:414",
            "C:\\work\\game\\scripts\\MELEE.GD",
            "fixed in ./scripts/melee.gd.",
        ] {
            assert!(!mentions(text, path).is_empty(), "{text}");
        }
        for text in [
            "proteus_melee.gd settles too early",
            "melee.gd.uid changed",
            "melee.gdx",
            "tools/melee.gd is the checker",
            "other/scripts/melee.gd",
            "the melee system",
            "docs/MELEE.md section 12",
        ] {
            assert!(mentions(text, path).is_empty(), "{text}");
        }
        assert_eq!(mentions("See scripts/melee.gd and melee.gd.", path), [12, 25], "where each is");
    }

    #[test]
    fn a_memory_is_about_what_it_repeats_or_opens_with() {
        let store = Store::open_in_memory().unwrap();
        let filler = "The hangar bench covers nine of ten board laws and the forge path was rebuilt. ".repeat(4);
        for i in 0..4 {
            remember(
                &store,
                "note",
                3,
                &format!("{filler}Portal refill number {i} was tuned against the timer."),
                None,
            );
        }
        remember(&store, "note", 3, &format!("{filler}Someone once saw a gyro near the cartwheel rig."), None);
        let about = remember(
            &store,
            "note",
            3,
            &format!("{filler}The gyro fights the cartwheel: gyro gain 0.4 ends the cartwheel."),
            None,
        );
        let ask = "the cartwheel is back, is the gyro gain wrong";
        let found = memories_like(&store, P, ask).unwrap();
        assert_eq!(
            found.iter().map(|r| r.memory.id).collect::<Vec<_>>(),
            [about],
            "not the one that says it in passing"
        );
        assert_eq!(found[0].shared, ["cartwheel", "gyro"], "\"gain\" comes once, past the opening");

        // A long memory is shown by how it opens and the part that matched.
        let text = related_memories(&store, "claude", "s1", P, ask, NOW).unwrap().expect("recalled");
        let line = text.lines().nth(1).unwrap();
        assert!(line.contains(": The hangar bench covers nine of ten board laws"), "{line}");
        assert!(line.contains("… ") && line.contains("The gyro fights the cartwheel: gyro gain 0.4 ends"), "{line}");
        assert!(line.len() < MEMORY_BYTES + 40, "{line}");

        // Words anyone uses say nothing in particular.
        remember(&store, "note", 3, "Trust the small test: the small test caught it.", None);
        let everyday = "trust me, a small model could not handle the test";
        assert!(memories_like(&store, P, everyday).unwrap().is_empty());
    }

    #[test]
    fn a_passage_is_cut_where_the_matches_are() {
        let long = format!("{}needle here. {}", "word ".repeat(100), "tail ".repeat(100));
        assert_eq!(passage("short", &[0]), "short");
        assert_eq!(passage(&long, &[]), truncate(&long, MEMORY_BYTES), "nothing to point at");
        assert_eq!(passage(&long, &[10]), truncate(&long, MEMORY_BYTES), "it opens with it");
        let shown = passage(&long, &[500, 900, 905]);
        assert!(shown.starts_with("word word") && shown.contains("… tail tail"), "{shown}");
        assert!(!shown.contains("needle"), "the stretch with the most matches: {shown}");
        assert!(shown.len() <= MEMORY_BYTES + 1, "{}", shown.len());
        // A cut never lands inside a character.
        let accents = format!("{} needle", "é".repeat(300));
        assert!(passage(&accents, &[601]).ends_with(" needle"));
    }

    #[test]
    fn an_edit_brings_up_the_memories_that_name_its_file() {
        let store = Store::open_in_memory().unwrap();
        let path = vec!["C:/work/game/scripts/melee.gd".to_string()];
        let note = remember(&store, "note", 3, "melee.gd _settled waits two frames before it clears.", None);
        let lesson =
            remember(&store, "lesson", 5, "The low clip pops unless the blend is eased.", Some("scripts/melee.gd"));
        remember(&store, "note", 5, "tools/godot/melee_check.gd runs the melee gate.", None);
        remember(&store, "fact", 5, "Unrelated: the hangar door timer is 4 s.", None);

        let text = memories_naming(&store, "claude", "s1", "", P, &path, NOW).unwrap().expect("recalled");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].starts_with("csm: saved memories in this project that name a file this edit changes"));
        assert!(lines[1].starts_with(&format!("  #{lesson} [lesson] ")), "the most important first: {text}");
        assert!(lines[2].starts_with(&format!("  #{note} [note] ")), "{text}");
        assert!(memories_naming(&store, "claude", "s1", "", P, &path, NOW + MIN).unwrap().is_none(), "once");
        assert!(
            memories_naming(&store, "claude", "s1", "a1", P, &path, NOW + MIN).unwrap().is_some(),
            "a subagent too"
        );

        // One a request already brought up is not repeated at the edit.
        let ask = "the melee blend pops at the low clip";
        assert!(related_memories(&store, "claude", "s2", P, ask, NOW).unwrap().expect("recalled").contains("low clip"));
        let text = memories_naming(&store, "claude", "s2", "", P, &path, NOW + MIN).unwrap().expect("the other");
        assert!(text.contains(&format!("#{note} ")) && !text.contains(&format!("#{lesson} ")), "{text}");

        let other = vec!["C:/work/game/scripts/hangar.gd".to_string()];
        assert!(memories_naming(&store, "claude", "s3", "", P, &other, NOW).unwrap().is_none(), "no memory names it");
    }

    #[test]
    fn a_session_that_worked_and_saved_nothing_is_sent_back_to_save() {
        let store = Store::open_in_memory().unwrap();
        let ask = |session: &str, i: i64| {
            say(&store, session, "prompt", &format!("make the dash longer, pass {i}"), NOW + i * MIN)
        };
        let nudge = |session: &str, at: i64| save_nudge(&store, "claude", session, P, true, NOW + at * MIN).unwrap();
        for i in 0..3 {
            ask("s1", i);
        }
        say(&store, "s1", "prompt", "<task-notification>done</task-notification>", NOW + 3 * MIN);
        touch(&store, "s1", "edit", "src/dash.rs", NOW + 3 * MIN);
        assert!(nudge("s1", 4).is_none(), "three requests of the user's");
        ask("s1", 5);
        let reason = nudge("s1", 6).expect("sent back");
        assert!(
            reason.starts_with("CSM save nudge: this session has taken 4 requests and changed 1 code file since"),
            "{reason}"
        );
        assert!(reason.contains("This project has no project card yet: save one with csm_save kind \"card\""));
        assert!(nudge("s1", 7).is_none(), "once");

        // It counts again from there, and only work on code counts.
        for i in 8..12 {
            ask("s1", i);
        }
        assert!(nudge("s1", 12).is_none(), "nothing changed since");
        touch(&store, "s1", "write", "notes/plan.md", NOW + 12 * MIN);
        assert!(nudge("s1", 13).is_none(), "prose is not code");
        touch(&store, "s1", "edit", "src/dash.rs", NOW + 13 * MIN);
        touch(&store, "s1", "edit", "src/slide.rs", NOW + 13 * MIN);
        let card = remember(&store, "card", 3, "A game. Run: godot --path .", None);
        let reason = nudge("s1", 14).expect("again");
        assert!(reason.contains("taken 4 requests and changed 2 code files"), "{reason}");
        assert!(reason.contains(&format!("If the project card (memory #{card}) no longer says what is true")));

        // What the session saved itself starts the count again.
        for i in 0..4 {
            ask("s2", i);
        }
        touch(&store, "s2", "edit", "src/dash.rs", NOW + 4 * MIN);
        say(&store, "s2", "save", "csm_checkpoint", NOW + 5 * MIN);
        ask("s2", 6);
        touch(&store, "s2", "edit", "src/dash.rs", NOW + 7 * MIN);
        assert!(nudge("s2", 8).is_none(), "one request since it saved");
    }

    #[test]
    fn hooks_that_cannot_see_a_sessions_saves_count_anyones() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..4 {
            say(&store, "s1", "prompt", &format!("tune the wake, pass {i}"), NOW + i * MIN);
        }
        touch(&store, "s1", "edit", "src/wake.rs", NOW + 4 * MIN);
        say(&store, "other", "checkpoint", "Wake tuned", NOW + 5 * MIN);
        assert!(save_nudge(&store, "claude", "s1", P, false, NOW + 6 * MIN).unwrap().is_none(), "someone saved since");
        assert!(save_nudge(&store, "claude", "s1", P, true, NOW + 6 * MIN).unwrap().is_some(), "not this session");
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
