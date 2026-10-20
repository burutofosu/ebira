//! `ebira ask`: a question about past work, answered from the records with quotes.
//!
//! Claude models choose literal search strings, screen what Ebira finds, and read the records
//! around the best of it; a draft answer feeds a second round of searching and a check, for each
//! decision it reports, of the person's later messages that may have changed it. Every quote in
//! the answer is checked against the records it was given before it is shown. Each string is
//! also searched with a space and with none between Latin letters and Japanese; when the screen
//! judges nothing found relevant, the answer reads the closest records, and the result says first
//! that they do not settle the question.
//!
//! The question and the text of the records read (not images) go to the API the person chose
//! (the Claude API unless another is named, by a flag or by `ebira ask-setup`) under their own
//! key; nothing else leaves the computer. Every call is written to a spending ledger.

mod api;
mod keys;
mod prompts;
mod records;
pub mod setup;
mod text;
pub mod usage;
mod value;

pub use api::Endpoint;
use api::{ApiError, Client, Request, Usage};
pub use records::Filters;
use records::{Corpus, Hit, Label, Line, PersonMessages, SourceRef, Who};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use usage::Ledger;
use value::Value;

/// What to ask and how hard to look.
pub struct Options {
    pub question: String,
    pub filters: Filters,
    /// Search strings given by the person; without them, the plan model chooses them.
    pub terms: Vec<String>,
    /// Search only, and show the records the answer model would read.
    pub no_api: bool,
    pub text_format: bool,
    /// Where to write the records read and the person's messages a sweep judged relevant, as
    /// text, for reading elsewhere.
    pub export: Option<PathBuf>,
    /// How many records the answer model reads.
    pub keep: usize,
    /// The person's messages, and the agents' replies, taken per search string (newest first).
    pub person_per_term: usize,
    pub agent_per_term: usize,
    /// How many candidates are screened per round.
    pub candidates: usize,
    /// After a first answer, search the strings it suggests, and answer again with what they add.
    pub second_round: bool,
    /// After a first answer, look for later messages of the person that change what it says
    /// was decided.
    pub reversal_check: bool,
    /// Screen every message the person wrote in the scope, not only those that hold a search
    /// string; the agents' records are still found by the strings. On unless `--no-sweep`.
    pub sweep: bool,
    /// The most of the person's messages a sweep screens, the newest first.
    pub sweep_limit: usize,
    /// Show the sweep each message with the agent's words around it.
    pub sweep_context: bool,
    /// Screen a long message in overlapping pieces instead of cutting out its middle.
    pub sweep_chunks: bool,
    /// Screen the agent's replies in the turns of the person's relevant messages.
    pub turn_replies: bool,
    /// How the records found are handed to the answer.
    pub delivery: Delivery,
    /// Which of the agents' records that hold a search string are screened.
    pub agent_select: AgentSelect,
    /// Stop before the answer and write what it would read, for measuring.
    pub evidence_only: bool,
    /// The model of that first answer, a draft that only feeds the later searches.
    pub draft_model: Option<String>,
    pub plan_model: String,
    pub screen_model: String,
    pub answer_model: String,
    pub effort: String,
    /// The service the requests go to: the Claude API unless another is named.
    pub endpoint: Endpoint,
}

impl Options {
    /// The default, the most accurate for its cost in a comparison of model setups on questions
    /// with known answers (October 2026): Claude Opus 5.5 chooses the search strings and writes
    /// the answer, Claude Haiku 5.5 screens and drafts, two rounds of searching, and the
    /// reversal check. The sweep shows the screen the agent's words around each message and
    /// a long message in pieces, and the records reach the answer spread out: together they
    /// made 100% of the evaluation's key points, against 95% without (evals/results).
    pub fn thorough(question: String) -> Options {
        Options {
            question,
            filters: Filters::default(),
            terms: Vec::new(),
            no_api: false,
            text_format: false,
            export: None,
            keep: 40,
            person_per_term: 60,
            agent_per_term: 40,
            candidates: 600,
            second_round: true,
            reversal_check: true,
            sweep: true,
            sweep_limit: 4000,
            sweep_context: true,
            sweep_chunks: true,
            turn_replies: false,
            delivery: Delivery::Spread,
            agent_select: AgentSelect::Newest,
            evidence_only: false,
            draft_model: Some("claude-haiku-5-5".to_string()),
            plan_model: "claude-opus-5-5".to_string(),
            screen_model: "claude-haiku-5-5".to_string(),
            answer_model: "claude-opus-5-5".to_string(),
            effort: "medium".to_string(),
            endpoint: Endpoint::default(),
        }
    }

    /// `--quick`: Claude Haiku 5.5 throughout, reading more records, without the reversal check:
    /// about a tenth of the cost, with more mistakes about what changed later.
    pub fn quick(question: String) -> Options {
        Options {
            keep: 60,
            reversal_check: false,
            draft_model: None,
            plan_model: "claude-haiku-5-5".to_string(),
            answer_model: "claude-haiku-5-5".to_string(),
            effort: "high".to_string(),
            ..Options::thorough(question)
        }
    }
}

/// How the records found reach the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The best by score then the order found, and the rest of the person's relevant messages
    /// in one line each, the earliest and the latest when there are too many.
    Current,
    /// The central before the background, one session at a time within a score, and the rest
    /// of the person's relevant messages within a character budget, identical ones once.
    Spread,
}

/// Which of the agents' records that hold a search string go to the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSelect {
    /// The newest of each string.
    Newest,
    /// As many, chosen across the months by how rare the strings they hold are.
    Idf,
    /// All of them, up to a cap.
    All,
}

const MAX_TERMS: usize = 16;
const SECOND_ROUND_TERMS: usize = 8;
/// The kinds of finding whose later change matters, in the order they are checked.
const CHECKED_KINDS: [&str; 4] = ["decision", "principle", "reversal", "cause"];
const MAX_CLAIMS: usize = 6;
const REVERSAL_TERMS: usize = 4;
const REVERSAL_PER_TERM: usize = 40;
const MAX_LATER: usize = 12;
const SCREEN_CHUNK: usize = 40;
/// How many of the records found the answer reads when the screen judged none relevant.
const CLOSEST: usize = 8;
const WORKERS: usize = 4;
const RECENT_SESSIONS: usize = 4;
const RECENT_MESSAGES: usize = 8;
/// How much of a message a sweep shows the screen: the person's messages are short, and a long
/// one is mostly something pasted.
const SWEEP_CHARS: usize = 600;
/// A long message is screened in pieces of this many characters, overlapping by the second.
const CHUNK_CHARS: usize = 500;
const CHUNK_OVERLAP: usize = 100;
/// How much of the agent's words around a message the sweep shows: the end of the reply before
/// it, and the head and tail of the turn's last reply.
const CONTEXT_CHARS: usize = 120;
/// How many of the last replies of a relevant turn are screened, each up to this many characters.
const TURN_REPLIES: usize = 3;
const REPLY_CHARS: usize = 600;
/// The spread digest: a character budget, and the most of one message shown.
const DIGEST_BUDGET_CHARS: usize = 36_000;
const DIGEST_LINE_CHARS: usize = 240;
/// How many of the agents' records holding a string are read to choose from, and the most
/// screened when all are.
const AGENT_POOL_LIMIT: usize = 1500;
const ALL_AGENTS_MAX: usize = 3000;
/// How many of the person's later messages a reversal check reads for one claim in a sweep:
/// the earliest and the latest halves when there are more.
const REVERSAL_SWEEP_MAX: usize = 400;
/// How many of the person's messages the plan model sees, spread over the scope, to learn how
/// the person writes.
const VOCABULARY_SAMPLE: usize = 40;
/// How many of the sessions the answer did not read are listed, newest first.
const UNREAD_LISTED: usize = 12;
/// The person's other messages a sweep judged relevant, beyond those read with their context,
/// go to the answer in one line each: at most this many (the earliest and the latest halves
/// when there are more), each cut at this many characters.
const SWEEP_DIGEST_MAX: usize = 300;
const SWEEP_DIGEST_CHARS: usize = 160;

/// A setting `ebira ask-setup` saved in the Windows user environment, such as
/// `EBIRA_ASK_STRONG_MODEL`; read once per run, with the saved keys.
pub fn saved_setting(name: &str) -> Option<String> {
    keys::saved(name)
}

/// Answers the question and writes the result.
pub fn run(root: &Path, options: &Options, ledger: &Ledger, scratch: PathBuf) -> io::Result<()> {
    let corpus = Corpus::open(root)?;
    let client = if options.no_api {
        None
    } else {
        Some(
            Client::for_endpoint(&options.endpoint, scratch)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?,
        )
    };
    let report = ask(&corpus, options, client.as_ref(), ledger).map_err(io::Error::other)?;
    crate::output::write(&report)
}

/// How many search strings one search took, and of how many records that held it.
#[derive(Clone, Debug)]
struct TermCount {
    term: String,
    who: Who,
    returned: usize,
    total: u64,
    /// The time of the oldest record taken when not all were: nothing before it was read.
    oldest_taken: Option<String>,
}

/// A sweep: every message the person wrote in the scope, screened against the question.
#[derive(Default)]
struct Swept {
    in_scope: usize,
    screened: usize,
    threes: usize,
    twos: usize,
    /// The time of the oldest and the newest message screened, as the logs wrote them.
    oldest: String,
    newest: String,
    stale_sources: u64,
    unscanned_bytes: u64,
    /// Messages the screen gave no score to (a call failed): they count as unscreened.
    unscreened: usize,
    /// Whether the scope held more messages with text than the sweep's limit: only the newest
    /// were screened.
    truncated: bool,
    /// Every message in the scope, newest first, screened or not: a reversal check reads those
    /// after a claim, and the coverage counts the sessions.
    messages: Vec<Hit>,
}

/// What the rounds of searching have gathered.
#[derive(Default)]
struct Pool {
    terms: Vec<String>,
    counts: Vec<TermCount>,
    synced_through: Option<String>,
    /// Every record already screened (or, without the API, taken), so a later round screens
    /// only new ones. A record left past a round's limit is not in it: a later round's strings
    /// can bring it back.
    seen: HashSet<SourceRef>,
    /// Every record a search found.
    found: HashSet<SourceRef>,
    /// The relevant records with their score (the screen's 2 or 3; 0 without the API), in the
    /// order found.
    relevant: Vec<(Hit, u64)>,
    /// The records screened and not judged relevant, with the screen's 1 or 0 (1 for one it
    /// gave no score), in the order found.
    near: Vec<(Hit, u64)>,
    threes: usize,
    twos: usize,
    screened: bool,
    rounds: usize,
    swept: Option<Swept>,
    /// For a long message screened in pieces, the characters of the piece judged most relevant.
    focus: HashMap<SourceRef, (usize, usize)>,
    /// The agent's words around each message the sweep read, when they were read.
    turns: HashMap<SourceRef, records::TurnContext>,
}

impl Pool {
    /// The best `keep` records: the higher score first, then in the order found.
    fn best(&self, keep: usize) -> Vec<Hit> {
        highest(&self.relevant, keep)
    }

    /// When the screen judged none relevant, the `count` closest: those that share words with
    /// the question before those it judged unrelated, then in the order found.
    fn closest(&self, count: usize) -> Vec<Hit> {
        highest(&self.near, count)
    }

    /// Whether the screen judged none of the records it screened relevant.
    fn nothing_relevant(&self) -> bool {
        self.screened && self.threes + self.twos == 0
    }
}

/// The first `count` of the records by score, the higher first, then in their order.
fn highest(scored: &[(Hit, u64)], count: usize) -> Vec<Hit> {
    let mut order: Vec<usize> = (0..scored.len()).collect();
    order.sort_by_key(|&index| (std::cmp::Reverse(scored[index].1), index));
    order
        .into_iter()
        .take(count)
        .map(|index| scored[index].0.clone())
        .collect()
}

/// What happened on the way that the result should say: an inferred period, steps that failed,
/// and what each reversal check looked for and found.
#[derive(Default)]
struct Notes {
    period: Option<(String, String)>,
    issues: Vec<String>,
    checks: Vec<ReversalCheck>,
    recent: Vec<records::Recent>,
    recent_sessions_omitted: usize,
    coverage: Option<Coverage>,
    /// The sweep's digest: how many of the person's relevant messages were shown in one line,
    /// and how many were left out.
    digest: Option<(usize, usize)>,
}

/// Which conversations in the scope the records read come from, and which they leave unread:
/// what the answer did not read, counted, for the person and for the answer.
struct Coverage {
    sessions_in_scope: usize,
    sessions_with_evidence: usize,
    /// Whether a sweep screened every message of the person's in the other sessions.
    swept: bool,
    unread: Vec<UnreadSession>,
    unread_total: usize,
}

/// A session in the scope none of whose records the answer read: when the person wrote in it,
/// how much, and how it opened.
struct UnreadSession {
    session: String,
    project: String,
    first: String,
    last: String,
    messages: usize,
    opening: String,
}

struct ReversalCheck {
    id: String,
    date: String,
    kind: String,
    quote: String,
    terms: Vec<String>,
    /// Whether the candidates were every later message of the person's (a sweep), not the
    /// records the strings found.
    swept: bool,
    /// The later messages there were before the check kept the earliest and the latest.
    later_total: usize,
    candidates: usize,
    changes: usize,
}

struct Spend {
    step: &'static str,
    model: String,
    usage: Usage,
    usd: Option<f64>,
}

/// Calls the API and writes every billed call to the ledger.
struct Caller<'a> {
    client: &'a Client,
    ledger: &'a Ledger,
    question: &'a str,
    spent: Mutex<Vec<Spend>>,
}

impl<'a> Caller<'a> {
    fn new(client: &'a Client, ledger: &'a Ledger, question: &'a str) -> Caller<'a> {
        Caller {
            client,
            ledger,
            question,
            spent: Mutex::new(Vec::new()),
        }
    }

    fn call(&self, step: &'static str, request: &Request) -> Result<Value, ApiError> {
        let mut request = *request;
        let mut retried = false;
        loop {
            let result = self.client.send(&request);
            let billed = match &result {
                Ok(reply) => Some((reply.model.clone(), reply.usage)),
                Err(error) => error
                    .billed()
                    .map(|(model, usage)| (model.to_string(), usage)),
            };
            if let Some((model, usage)) = billed {
                let usd = api::cost(&model, &usage);
                if let Err(error) = self.ledger.record(step, &model, &usage, usd, self.question) {
                    crate::output::diagnostic(format_args!(
                        "ebira ask: cannot write to the ledger {}: {error}",
                        self.ledger.path().display()
                    ));
                }
                self.spent.lock().unwrap().push(Spend {
                    step,
                    model,
                    usage,
                    usd,
                });
            }
            // A truncated JSON reply cannot be safely completed by string concatenation.
            // Repeat the same request once with more room; every billed attempt is recorded.
            if !retried
                && request.max_tokens < 64000
                && matches!(&result, Err(ApiError::Truncated { .. }))
            {
                request.max_tokens = request.max_tokens.saturating_mul(2).min(64000);
                retried = true;
                progress(&format!(
                    "{step} reached its output limit; retrying once with max_tokens={} ({})",
                    request.max_tokens, request.model
                ));
                continue;
            }
            return result.map(|reply| reply.output);
        }
    }
}

/// Who wrote a line of the evidence: the record the search found, or one near it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Author {
    Found(Who),
    Near(Label),
}

impl Author {
    fn shown(self) -> &'static str {
        match self {
            Author::Found(Who::Person) => "person",
            Author::Found(Who::Agent) => "agent",
            Author::Near(label) => label.shown(),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Author::Found(Who::Person) => "person",
            Author::Found(Who::Agent) => "agent",
            Author::Near(label) => label.id(),
        }
    }

    fn is_person(self) -> bool {
        matches!(
            self,
            Author::Found(Who::Person) | Author::Near(Label::Person)
        )
    }
}

struct Evidence {
    id: String,
    hit: Hit,
    block: String,
    /// The lines the answer model saw, to find whose words a quote is and where they are.
    lines: Vec<ShownLine>,
    /// Shown in one line, without its context: one of the sweep's digest.
    digest: bool,
}

/// How many of the records given to the answer were read with their context: the digest's
/// one-line records are not counted as read.
fn read_count(evidence: &[Evidence]) -> usize {
    evidence.iter().filter(|item| !item.digest).count()
}

struct ShownLine {
    author: Author,
    timestamp: String,
    /// The record's own text: quotes are checked against this, never against what the model
    /// was shown around it (the header, the authors' labels, the marks of what was left out).
    text: String,
    source_ref: SourceRef,
}

fn ask(
    corpus: &Corpus,
    options: &Options,
    client: Option<&Client>,
    ledger: &Ledger,
) -> Result<String, String> {
    let caller = client.map(|client| Caller::new(client, ledger, &options.question));
    let mut notes = Notes::default();

    // 1. What to search for.
    let mut filters = options.filters.clone();
    // The person's messages in the scope, read once for the plan model's sample of how the
    // person writes, and kept for the sweep while the scope stays the same.
    let mut prefetched: Option<PersonMessages> = None;
    let terms = if options.terms.is_empty() {
        let caller = caller
            .as_ref()
            .ok_or("without the API, name the search strings with --term")?;
        progress(&format!("choosing search strings ({})", options.plan_model));
        let (sample, messages) = vocabulary(corpus, &filters);
        prefetched = messages;
        let user = format!(
            "Today: {} ({})\nQuestion: {}{}",
            corpus.today(),
            corpus.zone(),
            options.question,
            sample
        );
        let schema = prompts::plan_schema();
        let plan = caller
            .call(
                "plan",
                &Request {
                    model: &options.plan_model,
                    effort: "low",
                    max_tokens: 4000,
                    system: prompts::PLAN_SYSTEM,
                    user: &user,
                    schema: &schema,
                },
            )
            .map_err(|error| error.to_string())?;
        let (from, to) = (plan.str_at("from"), plan.str_at("to"));
        if filters.from.is_none() && filters.to.is_none() && (is_date(from) || is_date(to)) {
            filters.from = is_date(from).then(|| from.to_string());
            filters.to = is_date(to).then(|| to.to_string());
            notes.period = Some((from.to_string(), to.to_string()));
            prefetched = None;
        }
        clean_terms(
            plan.array_at("terms")
                .iter()
                .map(|term| term.str_at("text").to_string()),
        )
    } else {
        clean_terms(options.terms.iter().cloned())
    };
    if terms.is_empty() {
        return Err("no search strings".to_string());
    }

    // 2-3. Search and screen; with a sweep, every message of the person's in the scope first.
    let mut pool = Pool::default();
    if options.sweep {
        let caller = caller
            .as_ref()
            .ok_or("--sweep screens the person's messages with the API")?;
        sweep(
            corpus,
            caller,
            options,
            &filters,
            prefetched.take(),
            &mut pool,
            &mut notes,
        )?;
        if options.turn_replies {
            turn_replies(corpus, caller, options, &mut pool, &mut notes);
        }
    }
    gather(
        corpus,
        caller.as_ref(),
        options,
        &filters,
        &terms,
        &mut pool,
        &mut notes,
    )?;
    // When no record holds any of the strings, choose others once: shorter stems, other
    // spellings, the person's own words. A sweep does not need them for the person's side,
    // but the agents' records are found by strings alone.
    if pool.found.is_empty() && options.terms.is_empty() {
        if let Some(caller) = caller.as_ref() {
            let tried = pool.terms.clone();
            let more = replan(caller, corpus, options, &tried, &mut notes);
            if !more.is_empty() {
                gather(
                    corpus,
                    Some(caller),
                    options,
                    &filters,
                    &more,
                    &mut pool,
                    &mut notes,
                )?;
            }
        }
    }

    // 4. Read the records around the best ones, or around the closest when the screen judged
    // none relevant.
    let mut evidence = read_pool(corpus, &filters, &pool, options, Vec::new(), &mut notes);

    // 5. Answer.
    let Some(caller) = caller else {
        return Ok(records_report(
            corpus,
            options,
            &evidence,
            &Summary::of(&pool, read_count(&evidence), &notes, &filters),
        ));
    };
    if evidence.is_empty() && options.evidence_only {
        let spent = caller.spent.lock().unwrap();
        return Ok(evidence_report(
            corpus,
            options,
            &evidence,
            &pool,
            &Summary::of(&pool, 0, &notes, &filters),
            &spent,
        ));
    }
    if evidence.is_empty() {
        notes.coverage = coverage(corpus, &filters, &pool, &evidence, prefetched.as_ref());
        export_if_asked(corpus, options, &filters, &pool, &evidence, &mut notes);
        let spent = caller.spent.lock().unwrap();
        return Ok(answer_report(
            corpus,
            options,
            None,
            &evidence,
            &Summary::of(&pool, 0, &notes, &filters),
            &spent,
        ));
    }
    let draft_model = options
        .draft_model
        .as_deref()
        .filter(|_| options.second_round || options.reversal_check)
        .unwrap_or(&options.answer_model);
    let mut answer = answer_with(
        &caller,
        options,
        corpus,
        &evidence,
        &pool,
        notes.digest,
        draft_model,
    )?;
    let mut records_changed = false;

    // 6. A second round: search the strings the answer suggests.
    if options.second_round {
        let more: Vec<String> = clean_terms(strings(&answer, "more_terms").into_iter())
            .into_iter()
            .filter(|term| !holds_term(&pool.terms, term))
            .take(SECOND_ROUND_TERMS)
            .collect();
        let (relevant, near) = (pool.relevant.len(), pool.near.len());
        if !more.is_empty() {
            gather(
                corpus,
                Some(&caller),
                options,
                &filters,
                &more,
                &mut pool,
                &mut notes,
            )?;
        }
        // While none is relevant, the closest are chosen again from what the round added.
        records_changed =
            pool.relevant.len() > relevant || (pool.nothing_relevant() && pool.near.len() > near);
    }

    // 7. For what the answer says was decided, look after its date for messages that change it.
    let mut later = Vec::new();
    if options.reversal_check {
        let claims = claims_to_check(corpus, &answer, &evidence);
        later = hunt_reversals(
            corpus, &caller, options, &filters, &claims, &pool, &mut notes,
        );
        records_changed |= !later.is_empty();
    }

    // 8. The final answer, from the answer model, over the records as they now stand.
    if options.evidence_only {
        if records_changed {
            evidence = read_pool(corpus, &filters, &pool, options, later, &mut notes);
        }
        let spent = caller.spent.lock().unwrap();
        return Ok(evidence_report(
            corpus,
            options,
            &evidence,
            &pool,
            &Summary::of(&pool, read_count(&evidence), &notes, &filters),
            &spent,
        ));
    }
    if records_changed {
        evidence = read_pool(corpus, &filters, &pool, options, later, &mut notes);
        answer = answer_with(
            &caller,
            options,
            corpus,
            &evidence,
            &pool,
            notes.digest,
            &options.answer_model,
        )?;
    } else if draft_model != options.answer_model {
        // The first answer was only a draft from another model.
        answer = answer_with(
            &caller,
            options,
            corpus,
            &evidence,
            &pool,
            notes.digest,
            &options.answer_model,
        )?;
    }
    notes.coverage = coverage(corpus, &filters, &pool, &evidence, prefetched.as_ref());
    export_if_asked(corpus, options, &filters, &pool, &evidence, &mut notes);
    let spent = caller.spent.lock().unwrap();
    Ok(answer_report(
        corpus,
        options,
        Some(&answer),
        &evidence,
        &Summary::of(&pool, read_count(&evidence), &notes, &filters),
        &spent,
    ))
}

/// The records the answer reads, each with the records around it: the best `keep` of the
/// relevant ones with the recent messages of their sessions, or, when the screen judged none
/// relevant, the closest (their sessions are not known to bear on the question); and `later`,
/// the messages a reversal check found, each with what it may change.
fn read_pool(
    corpus: &Corpus,
    filters: &Filters,
    pool: &Pool,
    options: &Options,
    later: Vec<Hit>,
    notes: &mut Notes,
) -> Vec<Evidence> {
    let keep = options.keep;
    let closest = pool.nothing_relevant();
    let mut hits = if closest {
        pool.closest(CLOSEST.min(keep))
    } else if options.delivery == Delivery::Spread {
        let order = spread_order(&pool.relevant);
        order
            .into_iter()
            .take(keep)
            .map(|index| pool.relevant[index].0.clone())
            .collect()
    } else {
        pool.best(keep)
    };
    if closest {
        progress(&format!(
            "none of the records screened was judged relevant; reading the {} closest",
            hits.len()
        ));
    }
    for found in later {
        match hits
            .iter_mut()
            .find(|hit| hit.source_ref == found.source_ref)
        {
            Some(hit) => hit.note = found.note,
            None => hits.push(found),
        }
    }
    let hits = if closest {
        hits
    } else {
        with_recent(corpus, filters, hits, notes)
    };
    let shown: HashSet<SourceRef> = hits.iter().map(|hit| hit.source_ref.clone()).collect();
    let mut evidence = read_evidence(corpus, hits, &pool.focus);
    if !closest {
        let (digest, left_out, chars) = match options.delivery {
            Delivery::Current => {
                let (digest, left_out) = sweep_digest(corpus, pool, &shown);
                let digest = digest.into_iter().map(|hit| (hit, 1)).collect();
                (digest, left_out, SWEEP_DIGEST_CHARS)
            }
            Delivery::Spread => {
                let (digest, left_out) = sweep_digest_spread(corpus, pool, &shown);
                (digest, left_out, DIGEST_LINE_CHARS)
            }
        };
        notes.digest = (!digest.is_empty() || left_out > 0).then_some((digest.len(), left_out));
        let base = evidence.len();
        evidence.extend(
            digest
                .into_iter()
                .enumerate()
                .map(|(index, (hit, copies))| {
                    let focus = pool.focus.get(&hit.source_ref).copied();
                    compose_digest(
                        corpus,
                        format!("E{}", base + index + 1),
                        hit,
                        chars,
                        focus,
                        copies,
                    )
                }),
        );
    }
    evidence
}

/// The order to read scored records in: the higher score first, and within a score one
/// session at a time (the sessions in the order their first record comes), so that one
/// session does not fill the reading. Indices into `scored`.
fn spread_order(scored: &[(Hit, u64)]) -> Vec<usize> {
    let mut scores: Vec<u64> = scored.iter().map(|(_, score)| *score).collect();
    scores.sort_unstable();
    scores.dedup();
    let mut order = Vec::with_capacity(scored.len());
    for score in scores.into_iter().rev() {
        let mut sessions: Vec<(&str, VecDeque<usize>)> = Vec::new();
        for (index, (hit, value)) in scored.iter().enumerate() {
            if *value != score {
                continue;
            }
            match sessions
                .iter_mut()
                .find(|(session, _)| *session == hit.session.as_str())
            {
                Some((_, queue)) => queue.push_back(index),
                None => sessions.push((hit.session.as_str(), VecDeque::from([index]))),
            }
        }
        loop {
            let mut took = false;
            for (_, queue) in sessions.iter_mut() {
                if let Some(index) = queue.pop_front() {
                    order.push(index);
                    took = true;
                }
            }
            if !took {
                break;
            }
        }
    }
    order
}

/// The spread digest: the person's relevant messages the answer does not read with context,
/// identical ones once with their count, the central before the background and one session at
/// a time, as many as `DIGEST_BUDGET_CHARS` holds at up to `DIGEST_LINE_CHARS` each; oldest
/// first. Also how many were left out.
fn sweep_digest_spread(
    corpus: &Corpus,
    pool: &Pool,
    shown: &HashSet<SourceRef>,
) -> (Vec<(Hit, usize)>, usize) {
    if pool.swept.is_none() {
        return (Vec::new(), 0);
    }
    let mut unique: Vec<(Hit, u64)> = Vec::new();
    let mut copies: Vec<usize> = Vec::new();
    let mut place: HashMap<String, usize> = HashMap::new();
    for (hit, score) in pool.relevant.iter().filter(|(hit, _)| {
        hit.who == Who::Person && hit.terms.is_empty() && !shown.contains(&hit.source_ref)
    }) {
        let key = text::normalize(&hit.text);
        match place.get(&key) {
            Some(&at) => {
                copies[at] += 1;
                unique[at].1 = unique[at].1.max(*score);
            }
            None => {
                place.insert(key, unique.len());
                unique.push((hit.clone(), *score));
                copies.push(1);
            }
        }
    }
    let mut used = 0;
    let mut chosen: Vec<usize> = Vec::new();
    for index in spread_order(&unique) {
        let cost = unique[index].0.text.chars().count().min(DIGEST_LINE_CHARS) + 60;
        if used + cost > DIGEST_BUDGET_CHARS {
            continue;
        }
        used += cost;
        chosen.push(index);
    }
    let left_out = unique.len() - chosen.len();
    let mut digest: Vec<(Hit, usize)> = chosen
        .into_iter()
        .map(|index| (unique[index].0.clone(), copies[index]))
        .collect();
    digest.sort_by(|left, right| time_order(corpus, &left.0, &right.0));
    (digest, left_out)
}

/// The person's messages the sweep judged relevant that the answer does not read with their
/// context: oldest first, up to `SWEEP_DIGEST_MAX` (the earliest and the latest halves when
/// there are more), and how many were left out.
fn sweep_digest(corpus: &Corpus, pool: &Pool, shown: &HashSet<SourceRef>) -> (Vec<Hit>, usize) {
    if pool.swept.is_none() {
        return (Vec::new(), 0);
    }
    let mut rest: Vec<Hit> = pool
        .relevant
        .iter()
        .filter(|(hit, _)| {
            hit.who == Who::Person && hit.terms.is_empty() && !shown.contains(&hit.source_ref)
        })
        .map(|(hit, _)| hit.clone())
        .collect();
    rest.sort_by(|left, right| time_order(corpus, left, right));
    let left_out = rest.len().saturating_sub(SWEEP_DIGEST_MAX);
    if rest.len() > SWEEP_DIGEST_MAX {
        let latest = rest.split_off(rest.len() - SWEEP_DIGEST_MAX / 2);
        rest.truncate(SWEEP_DIGEST_MAX - latest.len());
        rest.extend(latest);
    }
    (rest, left_out)
}

/// The block for one of the digest's messages: one line, without its context, at most `chars`
/// characters of it (of the piece found, for a long message screened in pieces), and how many
/// times the same words were sent.
fn compose_digest(
    corpus: &Corpus,
    id: String,
    hit: Hit,
    chars: usize,
    focus: Option<(usize, usize)>,
    copies: usize,
) -> Evidence {
    let shown = match focus {
        Some((start, end)) => {
            let piece: String = hit.text.chars().skip(start).take(end - start).collect();
            format!("… {}", text::clip_head(&text::one_line(&piece), chars))
        }
        None => text::clip_head(&text::one_line(&hit.text), chars),
    };
    let block = format!(
        "{id} | {} | person | {} | {} | digest: shown in one line, without its context, cut at \
         {} characters{}\n▶ person {}: {}\n",
        corpus.local(&hit.timestamp),
        if hit.project.is_empty() {
            "?"
        } else {
            &hit.project
        },
        hit.app,
        chars,
        if copies > 1 {
            format!(" | sent {copies} times in the same words")
        } else {
            String::new()
        },
        corpus.local(&hit.timestamp),
        shown
    );
    let lines = vec![ShownLine {
        author: Author::Found(Who::Person),
        timestamp: hit.timestamp.clone(),
        text: hit.text.clone(),
        source_ref: hit.source_ref.clone(),
    }];
    Evidence {
        id,
        hit,
        block,
        lines,
        digest: true,
    }
}

/// Add a bounded tail from the newest related sessions even when ranking or literal search
/// missed a later decision. Search remains global; no existing search hits are excluded.
fn with_recent(
    corpus: &Corpus,
    filters: &Filters,
    mut hits: Vec<Hit>,
    notes: &mut Notes,
) -> Vec<Hit> {
    let mut sessions: HashMap<String, Option<i128>> = HashMap::new();
    for hit in &hits {
        if !hit.session.is_empty() {
            let instant = corpus.instant(&hit.timestamp);
            let latest = sessions.entry(hit.session.clone()).or_default();
            *latest = (*latest).max(instant);
        }
    }
    let mut sessions = sessions.into_iter().collect::<Vec<_>>();
    sessions.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    notes.recent_sessions_omitted = sessions.len().saturating_sub(RECENT_SESSIONS);
    notes.recent.clear();
    for (session, _) in sessions.into_iter().take(RECENT_SESSIONS) {
        match corpus.recent_person(&session, filters, RECENT_MESSAGES) {
            Ok(recent) => {
                for latest in &recent.hits {
                    if let Some(hit) = hits
                        .iter_mut()
                        .find(|hit| hit.source_ref == latest.source_ref)
                    {
                        if !hit.note.is_empty() {
                            hit.note.push(' ');
                        }
                        hit.note.push_str(&latest.note);
                    } else {
                        hits.push(latest.clone());
                    }
                }
                notes.recent.push(recent);
            }
            Err(error) => notes.issues.push(format!(
                "Could not read recent human messages in session {session}: {error}"
            )),
        }
    }
    hits
}

/// Something the first answer says was decided, set as a rule, reversed, or found out, dated.
struct Claim {
    id: String,
    kind: String,
    quote: String,
    /// yyyy-mm-dd at the corpus offset.
    date: String,
    /// The time of the record it comes from, as the log wrote it.
    timestamp: String,
    source: SourceRef,
}

/// Whether one of the person's messages is one to check against the claim: another record,
/// from the date the search starts, and written no earlier than the claim. A message earlier
/// the same day is not a later change, however its time is written.
fn written_after(corpus: &Corpus, claim: &Claim, from: &str, hit: &Hit) -> bool {
    hit.source_ref != claim.source
        && corpus
            .local_date(&hit.timestamp)
            .is_some_and(|date| date.as_str() >= from)
        && corpus.instant(&hit.timestamp) >= corpus.instant(&claim.timestamp)
}

fn claims_to_check(corpus: &Corpus, answer: &Value, evidence: &[Evidence]) -> Vec<Claim> {
    let list = findings(corpus, answer, evidence);
    let mut claims: Vec<Claim> = Vec::new();
    for kind in CHECKED_KINDS {
        for finding in list.iter().filter(|finding| finding.kind == kind) {
            let Some((_, timestamp, source)) = finding.origin() else {
                continue;
            };
            let Some(date) = corpus.local_date(timestamp) else {
                continue;
            };
            if claims.iter().any(|claim| claim.source == *source) {
                continue;
            }
            claims.push(Claim {
                id: format!("D{}", claims.len() + 1),
                kind: kind.to_string(),
                quote: finding.quote.clone(),
                date,
                timestamp: timestamp.to_string(),
                source: source.clone(),
            });
            if claims.len() == MAX_CLAIMS {
                return claims;
            }
        }
    }
    claims
}

/// The person's messages, written at or after each claim, that the screen judges to change it
/// (2 or 3 points). Ranking by closeness to the question would drop them: a later change rarely
/// repeats the question's words. ("Use whatever library gets it done" undoes a rule against new
/// dependencies without the word dependency.)
fn hunt_reversals(
    corpus: &Corpus,
    caller: &Caller,
    options: &Options,
    filters: &Filters,
    claims: &[Claim],
    pool: &Pool,
    notes: &mut Notes,
) -> Vec<Hit> {
    if claims.is_empty() {
        return Vec::new();
    }
    let model = options.screen_model.as_str();
    progress(&format!(
        "checking {} decisions for later changes ({})",
        claims.len(),
        model
    ));
    // After a sweep, every later message of the person's is read: no strings are needed.
    let swept = if options.sweep {
        pool.swept.as_ref()
    } else {
        None
    };

    // 1. What a later message about each claim would contain: chosen by the plan model, like
    // every other search string (a weaker model rarely thinks of the alternative a change comes
    // as: another framework, another database).
    let chosen: Vec<Vec<String>> = if swept.is_some() {
        vec![Vec::new(); claims.len()]
    } else {
        let list: Vec<String> = claims
            .iter()
            .map(|claim| {
                format!(
                    "{} | {} | {} | 「{}」",
                    claim.id,
                    claim.date,
                    claim.kind,
                    text::one_line(&claim.quote)
                )
            })
            .collect();
        let user = format!(
            "Question: {}\n\nItems:\n{}",
            options.question,
            list.join("\n")
        );
        let schema = prompts::reversal_terms_schema();
        let request = Request {
            model: &options.plan_model,
            effort: "low",
            max_tokens: 4000,
            system: prompts::REVERSAL_TERMS_SYSTEM,
            user: &user,
            schema: &schema,
        };
        let reply = match caller.call("reversal-terms", &request) {
            Ok(reply) => reply,
            Err(error) => {
                notes
                    .issues
                    .push(format!("reversal check: no search strings ({error})"));
                return Vec::new();
            }
        };
        claims
            .iter()
            .map(|claim| {
                let strings: Vec<String> = reply
                    .array_at("items")
                    .iter()
                    .find(|item| item.str_at("id") == claim.id)
                    .map(|item| {
                        clean_terms(
                            item.array_at("terms")
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string),
                        )
                    })
                    .unwrap_or_default()
                    .into_iter()
                    .take(REVERSAL_TERMS)
                    .collect();
                with_spacing_variants(&strings)
            })
            .collect()
    };

    // 2. The person's messages from each claim's date on: one pass for every string of every
    // claim, from the earliest date. A claim takes the first messages after it, where a change
    // usually follows, and the newest, where the current state is. After a sweep, every message
    // of the person's from the claim on is a candidate.
    let mut all_terms: Vec<String> = Vec::new();
    for term in chosen.iter().flatten() {
        if !holds_term(&all_terms, term) {
            all_terms.push(term.clone());
        }
    }
    let start = |claim: &Claim| match &filters.from {
        Some(from) if *from > claim.date => from.clone(),
        _ => claim.date.clone(),
    };
    let found = if all_terms.is_empty() {
        Vec::new()
    } else {
        let after = Filters {
            from: claims.iter().map(start).min(),
            ..filters.clone()
        };
        match corpus.person(&all_terms, &after, usize::MAX, false) {
            Ok(found) => found,
            Err(error) => {
                notes
                    .issues
                    .push(format!("reversal check: the search failed ({error})"));
                return Vec::new();
            }
        }
    };
    let mut candidates: Vec<(&Claim, Vec<Hit>)> = Vec::new();
    let mut totals: HashMap<String, usize> = HashMap::new();
    for (claim, strings) in claims.iter().zip(&chosen) {
        let from = start(claim);
        let mut list: Vec<Hit> = Vec::new();
        for term in strings {
            let Some(index) = all_terms
                .iter()
                .position(|known| known.eq_ignore_ascii_case(term))
            else {
                continue;
            };
            let dated: Vec<&Hit> = found[index]
                .hits
                .iter()
                .filter(|hit| written_after(corpus, claim, &from, hit))
                .collect();
            let half = REVERSAL_PER_TERM / 2;
            let first = dated.iter().take(half);
            let newest = dated.iter().rev().take(half);
            for hit in first.chain(newest) {
                match list
                    .iter_mut()
                    .find(|known| known.source_ref == hit.source_ref)
                {
                    Some(known) if !known.terms.contains(term) => known.terms.push(term.clone()),
                    Some(_) => {}
                    None => {
                        let mut hit = (*hit).clone();
                        hit.terms = vec![term.clone()];
                        list.push(hit);
                    }
                }
            }
        }
        if let Some(swept) = swept {
            list = swept
                .messages
                .iter()
                .filter(|hit| written_after(corpus, claim, &from, hit))
                .cloned()
                .collect();
        }
        totals.insert(claim.id.clone(), list.len());
        // Keep the earliest and the latest when there are more than the check takes.
        let most = if swept.is_some() {
            REVERSAL_SWEEP_MAX
        } else {
            SCREEN_CHUNK
        };
        list.sort_by(|left, right| time_order(corpus, left, right));
        if list.len() > most {
            let latest = list.split_off(list.len() - most / 2);
            list.truncate(most - latest.len());
            list.extend(latest);
        }
        candidates.push((claim, list));
    }
    let screened: HashMap<String, usize> = candidates
        .iter()
        .map(|(claim, list)| (claim.id.clone(), list.len()))
        .collect();
    candidates.retain(|(_, list)| !list.is_empty());

    // 3. Does each later message change the claim? A claim's candidates go to the screen in
    // chunks, in the order written.
    let schema = prompts::screen_schema();
    let judged = parallel(candidates, |(claim, list)| {
        let mut results: Vec<(usize, usize, Result<Value, ApiError>)> = Vec::new();
        for (index, chunk) in list.chunks(SCREEN_CHUNK).enumerate() {
            let base = index * SCREEN_CHUNK;
            let lines: Vec<String> = chunk
                .iter()
                .enumerate()
                .map(|(index, hit)| {
                    format!(
                        "C{} | {} | {} | {}",
                        base + index + 1,
                        corpus.local(&hit.timestamp),
                        if hit.project.is_empty() {
                            "?"
                        } else {
                            &hit.project
                        },
                        if swept.is_some() {
                            text::one_line(&text::clip_middle(&hit.text, SWEEP_CHARS))
                        } else {
                            text::one_line(&text::window(&hit.text, &hit.terms, 360))
                        }
                    )
                })
                .collect();
            let user = format!(
                "Item {} ({}, {}):\n「{}」\n\nThe person's messages from then on:\n{}",
                claim.id,
                corpus.local(&claim.timestamp),
                claim.kind,
                text::one_line(&claim.quote),
                lines.join("\n")
            );
            let request = Request {
                model,
                effort: "low",
                max_tokens: 6000,
                system: if swept.is_some() {
                    prompts::REVERSAL_SWEEP_SYSTEM
                } else {
                    prompts::REVERSAL_SCREEN_SYSTEM
                },
                user: &user,
                schema: &schema,
            };
            results.push((base, chunk.len(), caller.call("reversal-screen", &request)));
        }
        (claim, list, results)
    });
    // The messages that may change each claim, the likeliest first, claim by claim.
    let mut per_claim: Vec<Vec<(Hit, u64)>> = Vec::new();
    let mut changes: HashMap<String, usize> = HashMap::new();
    for (claim, list, results) in judged {
        let mut found: Vec<(Hit, u64)> = Vec::new();
        for (base, length, result) in results {
            let output = match result {
                Ok(output) => output,
                Err(error) => {
                    notes.issues.push(format!(
                        "reversal check: the messages after {} (C{}-C{}) were not judged ({error})",
                        claim.id,
                        base + 1,
                        base + length
                    ));
                    continue;
                }
            };
            for entry in output.array_at("scores") {
                let score = entry.get("score").and_then(Value::as_u64).unwrap_or(0);
                let index = entry
                    .str_at("id")
                    .trim()
                    .trim_start_matches(['C', 'c'])
                    .parse::<usize>()
                    .ok();
                let Some(hit) = index
                    .and_then(|number| number.checked_sub(1))
                    .filter(|index| (base..base + length).contains(index))
                    .and_then(|index| list.get(index))
                else {
                    continue;
                };
                if score < 2 {
                    continue;
                }
                *changes.entry(claim.id.clone()).or_default() += 1;
                let mut hit = hit.clone();
                hit.note = format!(
                    "reversal check: may {} 「{}」 of {}",
                    if score == 3 { "reverse" } else { "change" },
                    text::clip_head(&text::one_line(&claim.quote), 40),
                    claim.date
                );
                found.push((hit, score));
            }
        }
        found.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        per_claim.push(found);
    }
    let later = take_in_turn(per_claim, MAX_LATER);
    for (claim, strings) in claims.iter().zip(chosen) {
        notes.checks.push(ReversalCheck {
            id: claim.id.clone(),
            date: claim.date.clone(),
            kind: claim.kind.clone(),
            quote: claim.quote.clone(),
            terms: strings,
            swept: swept.is_some(),
            later_total: totals.get(&claim.id).copied().unwrap_or(0),
            candidates: screened.get(&claim.id).copied().unwrap_or(0),
            changes: changes.get(&claim.id).copied().unwrap_or(0),
        });
    }
    later
}

/// At most `cap` records, each list's first, then each list's second, and so on, every record
/// once: a claim with many likely changes does not crowd out the one change of another.
fn take_in_turn(lists: Vec<Vec<(Hit, u64)>>, cap: usize) -> Vec<Hit> {
    let mut taken: Vec<Hit> = Vec::new();
    let deepest = lists.iter().map(Vec::len).max().unwrap_or(0);
    for round in 0..deepest {
        for list in &lists {
            if taken.len() == cap {
                return taken;
            }
            let Some((hit, _)) = list.get(round) else {
                continue;
            };
            if !taken.iter().any(|known| known.source_ref == hit.source_ref) {
                taken.push(hit.clone());
            }
        }
    }
    taken
}

/// One round: search `terms` with their spacing variants (those no earlier round searched),
/// drop records already considered, rank, and screen (with the API) into the pool.
fn gather(
    corpus: &Corpus,
    caller: Option<&Caller>,
    options: &Options,
    filters: &Filters,
    terms: &[String],
    pool: &mut Pool,
    notes: &mut Notes,
) -> Result<(), String> {
    let terms: Vec<String> = with_spacing_variants(terms)
        .into_iter()
        .filter(|term| !holds_term(&pool.terms, term))
        .collect();
    if terms.is_empty() {
        return Ok(());
    }
    pool.rounds += 1;
    progress(&format!("searching for {} strings", terms.len()));
    let searched = search(corpus, &terms, filters, options)?;
    pool.terms.extend(terms.iter().cloned());
    pool.counts.extend(searched.counts);
    if pool.synced_through.is_none() {
        pool.synced_through = searched.synced_through;
    }
    let limit = if options.agent_select == AgentSelect::All {
        usize::MAX
    } else {
        options.candidates
    };
    let candidates = take_new(pool, corpus, searched.hits, limit);
    match caller {
        Some(caller) if !candidates.is_empty() => {
            progress(&format!(
                "screening {} candidates ({})",
                candidates.len(),
                options.screen_model
            ));
            let scores = screen(
                caller,
                options,
                corpus,
                &candidates,
                notes,
                Screening::Found,
                None,
            );
            pool.screened = true;
            for (hit, score) in candidates.into_iter().zip(scores) {
                match score {
                    Some(3) => pool.threes += 1,
                    Some(2) => pool.twos += 1,
                    _ => {
                        pool.near.push((hit, score.map_or(1, |score| score.min(1))));
                        continue;
                    }
                }
                pool.relevant.push((hit, score.unwrap_or(0)));
            }
        }
        _ => pool
            .relevant
            .extend(candidates.into_iter().map(|hit| (hit, 0))),
    }
    Ok(())
}

/// Screens every message the person wrote in the scope (the newest `sweep_limit` of them)
/// against the question, in the order written, and puts the relevant ones in the pool: the
/// person's side of the search no longer depends on the strings chosen. The agents' records
/// are still found by the strings.
fn sweep(
    corpus: &Corpus,
    caller: &Caller,
    options: &Options,
    filters: &Filters,
    prefetched: Option<PersonMessages>,
    pool: &mut Pool,
    notes: &mut Notes,
) -> Result<(), String> {
    let found = match prefetched {
        Some(found) => found,
        None => corpus
            .person_all(filters, usize::MAX)
            .map_err(|error| format!("the sweep could not read the person's messages: {error}"))?,
    };
    // The newest `sweep_limit` messages with text, oldest first: each chunk the screen reads
    // is a stretch of the conversation. A message of images alone has nothing to screen.
    let with_text = found
        .hits
        .iter()
        .filter(|hit| !hit.text.trim().is_empty())
        .count();
    let mut ordered: Vec<Hit> = found
        .hits
        .iter()
        .filter(|hit| !hit.text.trim().is_empty())
        .take(options.sweep_limit)
        .cloned()
        .collect();
    ordered.reverse();
    let mut swept = Swept {
        in_scope: found.total,
        screened: ordered.len(),
        unscreened: 0,
        truncated: with_text > ordered.len(),
        threes: 0,
        twos: 0,
        oldest: ordered
            .first()
            .map(|hit| hit.timestamp.clone())
            .unwrap_or_default(),
        newest: ordered
            .last()
            .map(|hit| hit.timestamp.clone())
            .unwrap_or_default(),
        stale_sources: found.stale_sources,
        unscanned_bytes: found.unscanned_bytes,
        messages: found.hits,
    };
    if ordered.is_empty() {
        pool.swept = Some(swept);
        return Ok(());
    }
    progress(&format!(
        "sweeping {} of the person's messages ({})",
        ordered.len(),
        options.screen_model
    ));
    // The agent's words around each message, read once: shown to the screen with the message,
    // and kept for the turn's replies.
    let contexts: Option<HashMap<SourceRef, String>> =
        if options.sweep_context || options.turn_replies {
            match corpus.turn_contexts(&ordered, TURN_REPLIES) {
                Ok(turns) => {
                    let shown = options.sweep_context.then(|| {
                        turns
                            .iter()
                            .map(|(key, turn)| (key.clone(), turn_context_text(turn)))
                            .collect()
                    });
                    pool.turns = turns;
                    shown
                }
                Err(error) => {
                    notes.issues.push(format!(
                        "the agent's words around the person's messages could not be read \
                         ({error})"
                    ));
                    None
                }
            }
        } else {
            None
        };
    let (pieces, owners, ranges) = pieces_of(&ordered, options.sweep_chunks);
    let screening = if contexts.is_some() {
        Screening::SweepContext
    } else {
        Screening::Sweep
    };
    let piece_scores = screen(
        caller,
        options,
        corpus,
        &pieces,
        notes,
        screening,
        contexts.as_ref(),
    );
    // A message's score is its best piece's, and that piece is where the reader looks.
    let mut scores: Vec<Option<u64>> = vec![None; ordered.len()];
    for ((owner, range), score) in owners.iter().zip(&ranges).zip(piece_scores) {
        let Some(score) = score else {
            continue;
        };
        if scores[*owner].is_none_or(|best| score > best) {
            scores[*owner] = Some(score);
            if let (Some(range), true) = (range, score >= 2) {
                pool.focus
                    .insert(ordered[*owner].source_ref.clone(), *range);
            }
        }
    }
    pool.screened = true;
    // Into the pool newest first, as the search's records go.
    for (hit, score) in ordered.into_iter().zip(scores).rev() {
        pool.seen.insert(hit.source_ref.clone());
        match score {
            None => {
                swept.unscreened += 1;
                pool.near.push((hit, 1));
            }
            Some(3) => {
                swept.threes += 1;
                pool.threes += 1;
                pool.relevant.push((hit, 3));
            }
            Some(2) => {
                swept.twos += 1;
                pool.twos += 1;
                pool.relevant.push((hit, 2));
            }
            _ => pool.near.push((hit, score.map_or(1, |score| score.min(1)))),
        }
    }
    swept.screened -= swept.unscreened;
    pool.swept = Some(swept);
    Ok(())
}

/// Pieces of messages to screen, the message each is part of, and the characters it covers.
type Pieces = (Vec<Hit>, Vec<usize>, Vec<Option<(usize, usize)>>);

/// The pieces a sweep screens: each message whole, or, with `chunks`, a long one in overlapping
/// pieces that each fit the screen whole. With each piece, the message it is part of and, for a
/// piece, the characters it covers.
fn pieces_of(messages: &[Hit], chunks: bool) -> Pieces {
    let mut pieces = Vec::new();
    let mut owners = Vec::new();
    let mut ranges = Vec::new();
    for (index, hit) in messages.iter().enumerate() {
        let count = hit.text.chars().count();
        if !chunks || count <= SWEEP_CHARS {
            pieces.push(hit.clone());
            owners.push(index);
            ranges.push(None);
            continue;
        }
        let chars: Vec<char> = hit.text.chars().collect();
        let mut start = 0;
        loop {
            let end = (start + CHUNK_CHARS).min(count);
            let mut piece = hit.clone();
            piece.text = chars[start..end].iter().collect();
            pieces.push(piece);
            owners.push(index);
            ranges.push(Some((start, end)));
            if end == count {
                break;
            }
            start = end - CHUNK_OVERLAP;
        }
    }
    (pieces, owners, ranges)
}

/// What the sweep shows after a message: the end of the agent's reply before it, and the head
/// and tail of the agent's last reply in the turn it opened.
fn turn_context_text(turn: &records::TurnContext) -> String {
    let mut out = String::new();
    let before = text::one_line(&turn.before);
    if !before.trim().is_empty() {
        let chars: Vec<char> = before.chars().collect();
        let tail: String = chars[chars.len().saturating_sub(CONTEXT_CHARS)..]
            .iter()
            .collect();
        out.push_str(&format!(" [agent before] …{tail}"));
    }
    if let Some(last) = turn.replies.last() {
        let after = text::one_line(&last.text);
        out.push_str(&format!(
            " [agent after] {}",
            text::clip_middle(&after, CONTEXT_CHARS * 2)
        ));
    }
    out
}

/// The agent's replies in the turns of the person's messages the sweep judged relevant: no
/// search string has to hold them. Screened like the records a search finds.
fn turn_replies(
    corpus: &Corpus,
    caller: &Caller,
    options: &Options,
    pool: &mut Pool,
    notes: &mut Notes,
) {
    let mut candidates: Vec<Hit> = Vec::new();
    let mut taken: HashSet<SourceRef> = HashSet::new();
    for (hit, _) in pool
        .relevant
        .iter()
        .filter(|(hit, _)| hit.who == Who::Person)
    {
        let Some(turn) = pool.turns.get(&hit.source_ref) else {
            continue;
        };
        for reply in &turn.replies {
            if !pool.seen.contains(&reply.source_ref) && taken.insert(reply.source_ref.clone()) {
                candidates.push(reply.clone());
            }
        }
    }
    if candidates.is_empty() {
        return;
    }
    progress(&format!(
        "screening {} of the agent's replies in the relevant turns ({})",
        candidates.len(),
        options.screen_model
    ));
    let scores = screen(
        caller,
        options,
        corpus,
        &candidates,
        notes,
        Screening::Reply,
        None,
    );
    let screened = candidates.len();
    let mut relevant = 0;
    for (hit, score) in candidates.into_iter().zip(scores) {
        pool.seen.insert(hit.source_ref.clone());
        match score {
            Some(3) => {
                relevant += 1;
                pool.threes += 1;
                pool.relevant.push((hit, 3));
            }
            Some(2) => {
                relevant += 1;
                pool.twos += 1;
                pool.relevant.push((hit, 2));
            }
            _ => pool.near.push((hit, score.map_or(1, |score| score.min(1)))),
        }
    }
    notes.issues.push(format!(
        "Screened {screened} of the agent's replies in the turns of the person's relevant \
         messages; {relevant} judged relevant"
    ));
}

/// A sample of the person's messages in the scope, spread over its span, for the plan model to
/// see how the person writes: which words, which spellings. The messages read are returned for
/// the sweep to use.
fn vocabulary(corpus: &Corpus, filters: &Filters) -> (String, Option<PersonMessages>) {
    let Ok(found) = corpus.person_all(filters, usize::MAX) else {
        return (String::new(), None);
    };
    let messages: Vec<&Hit> = found
        .hits
        .iter()
        .filter(|hit| !hit.text.trim().is_empty())
        .collect();
    if messages.is_empty() {
        return (String::new(), Some(found));
    }
    let picked = spread(&messages, VOCABULARY_SAMPLE);
    let mut out = format!(
        "\n\nHow the person writes: {} of their {} messages in the scope, newest first (time | \
         project | message):\n",
        picked.len(),
        messages.len()
    );
    for hit in picked {
        out.push_str(&format!(
            "{} | {} | {}\n",
            corpus.local(&hit.timestamp),
            if hit.project.is_empty() {
                "?"
            } else {
                &hit.project
            },
            text::clip_head(&text::one_line(&hit.text), 140)
        ));
    }
    (out, Some(found))
}

/// `count` of the items, spread evenly over them, the first and the last included.
fn spread<T: Copy>(items: &[T], count: usize) -> Vec<T> {
    if items.len() <= count {
        return items.to_vec();
    }
    (0..count)
        .map(|index| items[index * (items.len() - 1) / (count - 1).max(1)])
        .collect()
}

/// Asks the plan model for other strings when the first ones found no record at all.
fn replan(
    caller: &Caller,
    corpus: &Corpus,
    options: &Options,
    tried: &[String],
    notes: &mut Notes,
) -> Vec<String> {
    progress(&format!(
        "no record holds the strings; choosing others ({})",
        options.plan_model
    ));
    let user = format!(
        "Today: {} ({})\nQuestion: {}\n\nA first choice of strings found no record at all: {}. \
         Choose other strings: shorter stems, other spellings (kana, kanji, katakana, full-width \
         letters), the words the person would type, and the words an agent would use when \
         reporting on it.",
        corpus.today(),
        corpus.zone(),
        options.question,
        tried.join(", ")
    );
    let schema = prompts::plan_schema();
    let request = Request {
        model: &options.plan_model,
        effort: "low",
        max_tokens: 4000,
        system: prompts::PLAN_SYSTEM,
        user: &user,
        schema: &schema,
    };
    match caller.call("plan", &request) {
        Ok(plan) => {
            let more: Vec<String> = clean_terms(
                plan.array_at("terms")
                    .iter()
                    .map(|term| term.str_at("text").to_string()),
            )
            .into_iter()
            .filter(|term| !holds_term(tried, term))
            .collect();
            notes.issues.push(format!(
                "No record held the first {} strings; {} others were chosen and searched",
                tried.len(),
                more.len()
            ));
            more
        }
        Err(error) => {
            notes.issues.push(format!(
                "no record held the strings, and choosing others failed ({error})"
            ));
            Vec::new()
        }
    }
}

/// The sessions in the scope with a message from the person, against the sessions the records
/// read come from. Reads the person's messages once more unless a sweep already did.
fn coverage(
    corpus: &Corpus,
    filters: &Filters,
    pool: &Pool,
    evidence: &[Evidence],
    prefetched: Option<&PersonMessages>,
) -> Option<Coverage> {
    let owned;
    let messages: &[Hit] = match (&pool.swept, prefetched) {
        (Some(swept), _) => &swept.messages,
        (None, Some(found)) => &found.hits,
        (None, None) => {
            owned = corpus.person_all(filters, usize::MAX).ok()?;
            &owned.hits
        }
    };
    let read: HashSet<&str> = evidence
        .iter()
        .map(|item| item.hit.session.as_str())
        .collect();
    // Newest first, as the messages come: the first message seen of a session is its newest.
    let mut sessions: Vec<UnreadSession> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for hit in messages {
        if hit.session.is_empty() {
            continue;
        }
        match index.get(hit.session.as_str()) {
            Some(&at) => {
                let entry = &mut sessions[at];
                entry.messages += 1;
                entry.first = hit.timestamp.clone();
                entry.opening = hit.text.clone();
            }
            None => {
                index.insert(hit.session.as_str(), sessions.len());
                sessions.push(UnreadSession {
                    session: hit.session.clone(),
                    project: hit.project.clone(),
                    first: hit.timestamp.clone(),
                    last: hit.timestamp.clone(),
                    messages: 1,
                    opening: hit.text.clone(),
                });
            }
        }
    }
    let sessions_in_scope = sessions.len();
    let sessions_with_evidence = sessions
        .iter()
        .filter(|session| read.contains(session.session.as_str()))
        .count();
    let mut unread: Vec<UnreadSession> = sessions
        .into_iter()
        .filter(|session| !read.contains(session.session.as_str()))
        .collect();
    let unread_total = unread.len();
    unread.truncate(UNREAD_LISTED);
    for session in &mut unread {
        session.opening = text::clip_head(&text::one_line(&session.opening), 90);
    }
    Some(Coverage {
        sessions_in_scope,
        sessions_with_evidence,
        swept: pool.swept.is_some(),
        unread,
        unread_total,
    })
}

/// Writes, as text, the person's messages a sweep judged relevant (whole, oldest first) and
/// the records the answer read: an extract to read elsewhere.
/// Writes the export when one was asked for, and says so, or why not, in the notes.
fn export_if_asked(
    corpus: &Corpus,
    options: &Options,
    filters: &Filters,
    pool: &Pool,
    evidence: &[Evidence],
    notes: &mut Notes,
) {
    let Some(path) = &options.export else {
        return;
    };
    match write_export(path, corpus, options, filters, pool, evidence) {
        Ok(()) => notes.issues.push(format!(
            "Exported the records read and the person's relevant messages to {}",
            path.display()
        )),
        Err(error) => notes.issues.push(format!(
            "could not write the export to {}: {error}",
            path.display()
        )),
    }
}

fn write_export(
    path: &Path,
    corpus: &Corpus,
    options: &Options,
    filters: &Filters,
    pool: &Pool,
    evidence: &[Evidence],
) -> io::Result<()> {
    let mut out = format!(
        "# ebira ask: what was read\nQuestion: {}\n",
        options.question
    );
    if let Some(cwd) = &filters.cwd {
        out.push_str(&format!("Working directory contains: {cwd}\n"));
    }
    for (word, date) in [("From", &filters.from), ("To", &filters.to)] {
        if let Some(date) = date {
            out.push_str(&format!("{word}: {date}\n"));
        }
    }
    out.push_str(&format!("Times: {}\n\n", corpus.zone()));
    if let Some(swept) = &pool.swept {
        let mut relevant: Vec<&(Hit, u64)> = pool
            .relevant
            .iter()
            .filter(|(hit, _)| hit.who == Who::Person && hit.terms.is_empty())
            .collect();
        relevant.sort_by(|left, right| time_order(corpus, &left.0, &right.0));
        out.push_str(&format!(
            "## The person's messages judged to bear on the question: {} of the {} screened ({} in \
             the scope)\n\n",
            relevant.len(),
            swept.screened,
            swept.in_scope
        ));
        for (hit, score) in relevant {
            out.push_str(&format!(
                "[{} | {} points | {} | session {} | {}]\n{}\n\n",
                corpus.local(&hit.timestamp),
                score,
                if hit.project.is_empty() {
                    "?"
                } else {
                    &hit.project
                },
                hit.session,
                source_ref_text(&hit.source_ref),
                hit.text.trim()
            ));
        }
    }
    let digests = evidence.len() - read_count(evidence);
    out.push_str(&format!(
        "## The records the answer read: {}{}\n\n",
        read_count(evidence),
        if digests > 0 {
            format!(" (and {digests} more of the person's messages, in one line each)")
        } else {
            String::new()
        }
    ));
    for item in evidence {
        out.push_str(&format!(
            "{}  {}\n\n",
            item.block.trim_end(),
            source_ref_text(&item.hit.source_ref)
        ));
    }
    std::fs::write(path, out)
}

/// What a screen is given: the records a search found, shown around the strings that found
/// them, or every message of the person's in a sweep, shown whole.
#[derive(Clone, Copy)]
enum Screening {
    Found,
    Sweep,
    /// A sweep that shows each message with the agent's words around it.
    SweepContext,
    /// The agent's replies in the turns of relevant messages.
    Reply,
}

impl Screening {
    fn step(self) -> &'static str {
        match self {
            Screening::Found => "screen",
            Screening::Sweep | Screening::SweepContext => "sweep",
            Screening::Reply => "turn-screen",
        }
    }

    fn system(self) -> &'static str {
        match self {
            Screening::Found | Screening::Reply => prompts::SCREEN_SYSTEM,
            Screening::Sweep => prompts::SWEEP_SYSTEM,
            Screening::SweepContext => prompts::SWEEP_CONTEXT_SYSTEM,
        }
    }

    fn text(self, hit: &Hit) -> String {
        match self {
            Screening::Found => text::one_line(&text::window(&hit.text, &hit.terms, 360)),
            Screening::Sweep | Screening::SweepContext => {
                text::one_line(&text::clip_middle(&hit.text, SWEEP_CHARS))
            }
            Screening::Reply => text::one_line(&text::clip_middle(&hit.text, REPLY_CHARS)),
        }
    }
}

/// This round's records not screened yet, best first, up to `limit`, which are marked as
/// screened. One past the limit is not: a later round's strings can bring it back.
fn take_new(pool: &mut Pool, corpus: &Corpus, hits: Vec<Hit>, limit: usize) -> Vec<Hit> {
    // Every record found counts as found, also one a sweep or an earlier round screened.
    pool.found
        .extend(hits.iter().map(|hit| hit.source_ref.clone()));
    let mut this_round = HashSet::new();
    let mut candidates: Vec<Hit> = hits
        .into_iter()
        .filter(|hit| {
            !pool.seen.contains(&hit.source_ref) && this_round.insert(hit.source_ref.clone())
        })
        .collect();
    rank(corpus, &mut candidates);
    candidates.truncate(limit);
    pool.seen
        .extend(candidates.iter().map(|hit| hit.source_ref.clone()));
    candidates
}

/// The records, oldest first, each with the nearest readable records around it; a long message
/// screened in pieces shows the piece found.
fn read_evidence(
    corpus: &Corpus,
    mut hits: Vec<Hit>,
    focus: &HashMap<SourceRef, (usize, usize)>,
) -> Vec<Evidence> {
    hits.sort_by(|left, right| time_order(corpus, left, right));
    if !hits.is_empty() {
        progress(&format!("reading the records around {} found", hits.len()));
    }
    let contexts = parallel(hits.clone(), |hit| {
        // Logs interleave many records without text (queue operations, titles, modes, empty
        // thinking): read a wide window, then keep the nearest readable lines.
        let (window_before, window_after, keep_before, keep_after) = if hit.who == Who::Person {
            (10, 15, 2, 3)
        } else {
            (6, 6, 1, 2)
        };
        corpus
            .around(&hit.source_ref, window_before, window_after)
            .map(|lines| nearest(lines, keep_before, keep_after))
            .map_err(|error| error.to_string())
    });
    hits.into_iter()
        .zip(contexts)
        .enumerate()
        .map(|(index, (hit, lines))| {
            let piece = focus.get(&hit.source_ref).copied();
            compose_focused(corpus, format!("E{}", index + 1), hit, lines, piece)
        })
        .collect()
}

fn time_order(corpus: &Corpus, left: &Hit, right: &Hit) -> std::cmp::Ordering {
    earliest_first(corpus, &left.timestamp, &right.timestamp)
}

/// Earlier first, by the instant each time stands for: logs write the same moment with
/// different offsets (`2026-10-16T09:00:00+09:00` comes before `2026-10-16T01:00:00Z`). Times
/// that do not parse go last, in the order of their text.
fn earliest_first(corpus: &Corpus, left: &str, right: &str) -> std::cmp::Ordering {
    match (corpus.instant(left), corpus.instant(right)) {
        (Some(left_at), Some(right_at)) => left_at.cmp(&right_at),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

/// Newer first, by instant; times that do not parse still go last.
fn newest_first(corpus: &Corpus, left: &str, right: &str) -> std::cmp::Ordering {
    match (corpus.instant(left), corpus.instant(right)) {
        (Some(left_at), Some(right_at)) => right_at.cmp(&left_at),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => right.cmp(left),
    }
}

fn answer_with(
    caller: &Caller,
    options: &Options,
    corpus: &Corpus,
    evidence: &[Evidence],
    pool: &Pool,
    digest: Option<(usize, usize)>,
    model: &str,
) -> Result<Value, String> {
    progress(&format!(
        "answering from {} records ({model})",
        evidence.len()
    ));
    let blocks: Vec<&str> = evidence.iter().map(|item| item.block.as_str()).collect();
    // Records the screen judged unrelated invite an answer about something else.
    let closest = if pool.nothing_relevant() {
        format!(
            "No record the search found ({} in all) was judged relevant to the question. The \
             records below are the closest, and they may be about something else. If they do \
             not settle the question, say so plainly at the start of the answer, then what in \
             them comes closest, if anything does; never answer from a record about something \
             else.\n\n",
            pool.found.len()
        )
    } else {
        String::new()
    };
    // After a sweep, the person's side is complete as far as the screen saw: a matter none of
    // the records raises is one the person did not raise in the messages screened.
    let swept = match &pool.swept {
        Some(swept) if swept.screened > 0 => {
            let which = if swept.truncated {
                format!(
                    "The newest {} of the {} messages the person wrote in the scope of this \
                     question ({} to {}) were screened by a model for bearing on it; the older \
                     ones were not",
                    swept.screened,
                    swept.in_scope,
                    corpus.local(&swept.oldest),
                    corpus.local(&swept.newest)
                )
            } else {
                format!(
                    "Every message the person wrote in the scope of this question ({} messages, \
                     {} to {}) was screened by a model for bearing on it",
                    swept.screened,
                    corpus.local(&swept.oldest),
                    corpus.local(&swept.newest)
                )
            };
            let left_out = match digest {
                Some((_, left_out)) if left_out > 0 => {
                    format!(" ({left_out} of them were left out for room)")
                }
                _ => String::new(),
            };
            let unscreened = if swept.unscreened > 0 {
                format!(
                    " {} messages got no score and were not screened.",
                    swept.unscreened
                )
            } else {
                String::new()
            };
            format!(
                "{which}, and every one judged to bear on it is among the records below, with \
                 its context or in one line{left_out}.{unscreened} So a matter none of these \
                 records raises is one the person did not raise in the messages screened, as \
                 far as the screen saw: say so under gaps instead of guessing, and do not claim \
                 the person never said something outside that scope. The agents' records were \
                 found by search strings and are not complete.\n\n"
            )
        }
        _ => String::new(),
    };
    let digest_note = if evidence.iter().any(|item| item.digest) {
        "Records marked digest are more of the person's messages the sweep judged to bear on the \
         question, shown in one line each without their context and cut short: quote from them \
         only the words shown, and count them when the question is about what the person said \
         more than once.\n\n"
    } else {
        ""
    };
    let user = format!(
        "Question: {}\nToday: {}\nTimes: every time below is local time, {}\n\n{closest}{swept}{digest_note}Records ({} \
         of them, by E number; ▶ marks the record found, the lines around it are its \
         context):\n\n{}",
        options.question,
        corpus.today(),
        corpus.zone(),
        evidence.len(),
        blocks.join("\n")
    );
    let schema = prompts::answer_schema();
    // Room for long thinking: Claude Haiku 5.5 at effort high over 60 records ran past 16,000
    // tokens.
    let request = Request {
        model,
        effort: &options.effort,
        max_tokens: 32000,
        system: prompts::ANSWER_SYSTEM,
        user: &user,
        schema: &schema,
    };
    caller
        .call("answer", &request)
        .map_err(|error| error.to_string())
}

fn progress(message: &str) {
    crate::output::diagnostic(format_args!("ebira ask: {message}…"));
}

fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

/// Whether `terms` holds `term`, ASCII case aside, as the search folds it.
fn holds_term(terms: &[String], term: &str) -> bool {
    terms.iter().any(|known| known.eq_ignore_ascii_case(term))
}

/// Each search string followed by its spacing variants (`csv 取り込み` for `csv取り込み`), every
/// string once: the variants go into the same pass over the corpus as the strings.
fn with_spacing_variants(terms: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for term in terms {
        for string in std::iter::once(term.clone()).chain(text::spacing_variants(term)) {
            if !holds_term(&out, &string) {
                out.push(string);
            }
        }
    }
    out
}

/// Trimmed, without repeats, at most `MAX_TERMS`.
fn clean_terms(terms: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for term in terms {
        let term = term.trim().to_string();
        if !term.is_empty() && !out.contains(&term) {
            out.push(term);
        }
    }
    out.truncate(MAX_TERMS);
    out
}

struct Searched {
    hits: Vec<Hit>,
    counts: Vec<TermCount>,
    synced_through: Option<String>,
}

/// Every term in one pass over the person's messages and one over the agents' replies, side by
/// side; a record found by several terms is listed once, with all of them.
fn search(
    corpus: &Corpus,
    terms: &[String],
    filters: &Filters,
    options: &Options,
) -> Result<Searched, String> {
    let agent_limit = match options.agent_select {
        AgentSelect::Newest => options.agent_per_term,
        AgentSelect::Idf | AgentSelect::All => AGENT_POOL_LIMIT.max(options.agent_per_term),
    };
    let (person, agents) = thread::scope(|scope| {
        let agents = (options.agent_per_term > 0)
            .then(|| scope.spawn(|| corpus.agents(terms, filters, agent_limit)));
        let person = corpus.person(terms, filters, options.person_per_term, true);
        let agents = agents.map(|handle| {
            handle.join().unwrap_or_else(|_| {
                Err(io::Error::other(
                    "the search of the agents' replies stopped",
                ))
            })
        });
        (person, agents)
    });
    let person = person.map_err(|error| error.to_string())?;
    let (agents, synced_through) = match agents {
        Some(result) => {
            let (lists, synced_through) = result.map_err(|error| error.to_string())?;
            (Some(lists), synced_through)
        }
        None => (None, None),
    };
    let selected: Option<HashSet<SourceRef>> = match (&agents, options.agent_select) {
        (Some(lists), AgentSelect::Idf | AgentSelect::All) => {
            Some(select_agents(corpus, lists, terms, options))
        }
        _ => None,
    };
    let mut place: HashMap<SourceRef, usize> = HashMap::new();
    let mut searched = Searched {
        hits: Vec::new(),
        counts: Vec::new(),
        synced_through,
    };
    for (index, term) in terms.iter().enumerate() {
        let mut lists = vec![(Who::Person, &person[index])];
        if let Some(agents) = &agents {
            lists.push((Who::Agent, &agents[index]));
        }
        for (who, found) in lists {
            let taken: Vec<&Hit> = found
                .hits
                .iter()
                .filter(|hit| {
                    who == Who::Person
                        || selected
                            .as_ref()
                            .is_none_or(|set| set.contains(&hit.source_ref))
                })
                .collect();
            searched.counts.push(TermCount {
                term: term.clone(),
                who,
                returned: taken.len(),
                total: found.total,
                oldest_taken: ((taken.len() as u64) < found.total)
                    .then(|| taken.last().map(|hit| hit.timestamp.clone()))
                    .flatten(),
            });
            for hit in taken {
                match place.get(&hit.source_ref) {
                    Some(&at) => {
                        if !searched.hits[at].terms.contains(term) {
                            searched.hits[at].terms.push(term.clone());
                        }
                    }
                    None => {
                        place.insert(hit.source_ref.clone(), searched.hits.len());
                        searched.hits.push(hit.clone());
                    }
                }
            }
        }
    }
    Ok(searched)
}

/// The agents' records to screen, from those that hold the strings (the newest
/// `AGENT_POOL_LIMIT` of each): with `Idf`, as many as the newest of each string would give,
/// chosen across the months, half in equal shares and half in proportion to the records each
/// month holds, and within a month by the sum of the rarity, ln(1 + (D - df + 0.5)/(df + 0.5)),
/// of the strings each holds, a string and its spacing variants counted once; with `All`, all
/// of them, up to `ALL_AGENTS_MAX` chosen the same way.
fn select_agents(
    corpus: &Corpus,
    lists: &[records::Found],
    terms: &[String],
    options: &Options,
) -> HashSet<SourceRef> {
    let mut records: HashMap<SourceRef, (String, HashSet<String>)> = HashMap::new();
    let mut newest: HashSet<SourceRef> = HashSet::new();
    for (term, found) in terms.iter().zip(lists) {
        let group = text::normalize(term);
        for (rank, hit) in found.hits.iter().enumerate() {
            let entry = records
                .entry(hit.source_ref.clone())
                .or_insert_with(|| (hit.timestamp.clone(), HashSet::new()));
            entry.1.insert(group.clone());
            if rank < options.agent_per_term {
                newest.insert(hit.source_ref.clone());
            }
        }
    }
    let total = records.len();
    let quota = match options.agent_select {
        AgentSelect::All if total <= ALL_AGENTS_MAX => return records.into_keys().collect(),
        AgentSelect::All => ALL_AGENTS_MAX,
        _ => newest.len(),
    };
    let mut df: HashMap<String, usize> = HashMap::new();
    for (_, groups) in records.values() {
        for group in groups {
            *df.entry(group.clone()).or_default() += 1;
        }
    }
    let documents = total as f64;
    let rarity = |group: &String| {
        let count = df.get(group).copied().unwrap_or(0) as f64;
        (1.0 + (documents - count + 0.5) / (count + 0.5)).ln()
    };
    let mut strata: BTreeMap<String, Vec<(f64, Option<i128>, SourceRef)>> = BTreeMap::new();
    for (key, (timestamp, groups)) in &records {
        let month = corpus
            .local_date(timestamp)
            .map(|date| date.chars().take(7).collect())
            .unwrap_or_else(|| "undated".to_string());
        let score: f64 = groups.iter().map(rarity).sum();
        strata
            .entry(month)
            .or_default()
            .push((score, corpus.instant(timestamp), key.clone()));
    }
    let months = strata.len().max(1) as f64;
    let mut shares: Vec<(String, usize)> = strata
        .iter()
        .map(|(month, list)| {
            let share = quota as f64 * (0.5 / months + 0.5 * list.len() as f64 / total as f64);
            (month.clone(), (share.round() as usize).min(list.len()))
        })
        .collect();
    let mut left = quota.saturating_sub(shares.iter().map(|(_, share)| share).sum());
    while left > 0 {
        let mut gave = false;
        let mut by_room: Vec<usize> = (0..shares.len()).collect();
        by_room.sort_by_key(|&index| {
            std::cmp::Reverse(strata[&shares[index].0].len() - shares[index].1)
        });
        for index in by_room {
            if left == 0 {
                break;
            }
            if shares[index].1 < strata[&shares[index].0].len() {
                shares[index].1 += 1;
                left -= 1;
                gave = true;
            }
        }
        if !gave {
            break;
        }
    }
    let mut selected = HashSet::new();
    for (month, share) in shares {
        let mut list = strata.remove(&month).unwrap_or_default();
        list.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| right.1.cmp(&left.1))
        });
        selected.extend(list.into_iter().take(share).map(|(_, _, key)| key));
    }
    selected
}

/// More search strings matched first, then the person's own words, then the newer.
fn rank(corpus: &Corpus, hits: &mut [Hit]) {
    hits.sort_by(|a, b| {
        b.terms
            .len()
            .cmp(&a.terms.len())
            .then_with(|| (b.who == Who::Person).cmp(&(a.who == Who::Person)))
            .then_with(|| newest_first(corpus, &a.timestamp, &b.timestamp))
    });
}

/// A score (0-3) for each candidate, none where none came back.
fn screen(
    caller: &Caller,
    options: &Options,
    corpus: &Corpus,
    candidates: &[Hit],
    notes: &mut Notes,
    screening: Screening,
    contexts: Option<&HashMap<SourceRef, String>>,
) -> Vec<Option<u64>> {
    let schema = prompts::screen_schema();
    let chunks: Vec<(usize, &[Hit])> = candidates
        .chunks(SCREEN_CHUNK)
        .enumerate()
        .map(|(index, chunk)| (index * SCREEN_CHUNK, chunk))
        .collect();
    let results = parallel(chunks, |(base, chunk)| {
        let lines: Vec<String> = chunk
            .iter()
            .enumerate()
            .map(|(index, hit)| {
                format!(
                    "C{} | {} | {} | {} | {}{}",
                    base + index + 1,
                    corpus.local(&hit.timestamp),
                    if hit.who == Who::Person {
                        "person"
                    } else {
                        "agent"
                    },
                    if hit.project.is_empty() {
                        "?"
                    } else {
                        &hit.project
                    },
                    screening.text(hit),
                    contexts
                        .and_then(|map| map.get(&hit.source_ref))
                        .map(String::as_str)
                        .unwrap_or("")
                )
            })
            .collect();
        let user = format!(
            "Question: {}\n\nCandidates:\n{}",
            options.question,
            lines.join("\n")
        );
        let result = caller.call(
            screening.step(),
            &Request {
                model: &options.screen_model,
                effort: "low",
                max_tokens: 6000,
                system: screening.system(),
                user: &user,
                schema: &schema,
            },
        );
        (base, chunk.len(), result)
    });

    let mut scores: Vec<Option<u64>> = vec![None; candidates.len()];
    for (base, length, result) in results {
        match result {
            Ok(output) => {
                for entry in output.array_at("scores") {
                    let index = entry
                        .str_at("id")
                        .trim()
                        .trim_start_matches(['C', 'c'])
                        .parse::<usize>()
                        .ok();
                    if let Some(index) = index.and_then(|number| number.checked_sub(1)) {
                        if (base..base + length).contains(&index) {
                            scores[index] = entry.get("score").and_then(Value::as_u64);
                        }
                    }
                }
            }
            Err(error) => match screening {
                // Keep the best-ranked third of the chunk rather than lose it, and say so.
                Screening::Found => {
                    notes.issues.push(format!(
                        "candidates C{}-C{} were not screened ({error}); the third with the most \
                         search strings was kept",
                        base + 1,
                        base + length
                    ));
                    for (offset, score) in scores[base..base + length].iter_mut().enumerate() {
                        *score = Some(if offset < length.div_ceil(3) { 2 } else { 1 });
                    }
                }
                // A sweep's chunk is a stretch of the conversation, not ranked: nothing in it
                // can be kept on its position. Nor can a turn's replies.
                Screening::Sweep | Screening::SweepContext | Screening::Reply => {
                    notes.issues.push(format!(
                        "the person's messages C{}-C{} were not swept ({error}); they count as \
                     unscreened",
                        base + 1,
                        base + length
                    ))
                }
            },
        }
    }
    let unscored = scores.iter().filter(|score| score.is_none()).count();
    if unscored > 0 {
        notes.issues.push(format!(
            "{unscored} candidates got no score and were left out"
        ));
    }
    scores
}

/// Runs `work` on every item with a few threads; the results keep the items' order.
fn parallel<T: Send, R: Send>(items: Vec<T>, work: impl Fn(T) -> R + Sync) -> Vec<R> {
    let count = items.len();
    let queue = Mutex::new(items.into_iter().enumerate());
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..count).map(|_| None).collect());
    thread::scope(|scope| {
        for _ in 0..WORKERS.min(count) {
            scope.spawn(|| loop {
                let next = queue.lock().unwrap().next();
                let Some((index, item)) = next else {
                    break;
                };
                let result = work(item);
                results.lock().unwrap()[index] = Some(result);
            });
        }
    });
    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|result| result.expect("every item ran"))
        .collect()
}

/// The hit with up to `before` readable lines before it and up to `after` after it, stopping
/// after the agent's first reply.
fn nearest(lines: Vec<Line>, before: usize, after: usize) -> Vec<Line> {
    let Some(hit) = lines.iter().position(|line| line.is_hit) else {
        return lines;
    };
    let start = hit.saturating_sub(before);
    let mut end = hit + 1;
    while end < lines.len() && end - hit <= after {
        end += 1;
        if lines[end - 1].label == Label::Agent {
            break;
        }
    }
    lines[start..end].to_vec()
}

/// An agent's record as read, with the text around the match added when the reading left that
/// part out (a long tool input, say).
fn agent_text(read: &str, hit: &Hit) -> String {
    let folded = read.to_ascii_lowercase();
    let shown = hit
        .terms
        .iter()
        .any(|term| folded.contains(&term.to_ascii_lowercase()));
    if shown || hit.text.trim().is_empty() {
        return read.to_string();
    }
    format!(
        "{read}\n[where the search found it] … {} …",
        hit.text.trim()
    )
}

/// A long message shown around the piece the screen judged relevant: its head, the piece, and its
/// tail, instead of the head and tail alone.
fn focused(full: &str, start: usize, end: usize) -> String {
    let chars: Vec<char> = full.chars().collect();
    let head: String = chars[..300.min(chars.len())].iter().collect();
    let piece: String = chars[start.min(chars.len())..end.min(chars.len())]
        .iter()
        .collect();
    let tail: String = chars[chars.len().saturating_sub(300)..].iter().collect();
    format!("{head} … [the part found] {piece} … {tail}")
}

/// The block the answer model reads for one record, shown whole.
#[cfg(test)]
fn compose(corpus: &Corpus, id: String, hit: Hit, lines: Result<Vec<Line>, String>) -> Evidence {
    compose_focused(corpus, id, hit, lines, None)
}

/// The block the answer model reads for one record: a header, then the record (▶) among its
/// neighbours; a long message of the person's screened in pieces, around the piece found.
fn compose_focused(
    corpus: &Corpus,
    id: String,
    hit: Hit,
    lines: Result<Vec<Line>, String>,
    piece: Option<(usize, usize)>,
) -> Evidence {
    let found = Author::Found(hit.who);
    let mut block = format!(
        "{id} | {} | {} | {} | {}{}{}\n",
        corpus.local(&hit.timestamp),
        found.shown(),
        if hit.project.is_empty() {
            "?"
        } else {
            &hit.project
        },
        hit.app,
        if hit.images > 0 {
            format!(" | {} images attached", hit.images)
        } else {
            String::new()
        },
        if hit.note.is_empty() {
            String::new()
        } else {
            format!(" | {}", hit.note)
        }
    );
    let mut shown = Vec::new();
    let add_hit = |block: &mut String, shown: &mut Vec<ShownLine>, full: &str, timestamp: &str| {
        let body = match (hit.who, piece) {
            (Who::Person, Some((start, end))) if full.chars().count() > 1800 => {
                focused(full, start, end)
            }
            (Who::Person, _) => text::clip_middle(full, 1800),
            (Who::Agent, _) => text::clip_middle(full, 1500),
        };
        block.push_str(&format!(
            "▶ {} {}: {}\n",
            found.shown(),
            corpus.local(timestamp),
            body
        ));
        shown.push(ShownLine {
            author: found,
            timestamp: timestamp.to_string(),
            text: full.to_string(),
            source_ref: hit.source_ref.clone(),
        });
    };
    match lines {
        Ok(lines) => {
            let mut hit_seen = false;
            for line in &lines {
                if line.is_hit {
                    hit_seen = true;
                    // The person's words as Ebira separated them; an agent's whole reply.
                    let full = if hit.who == Who::Person {
                        hit.text.clone()
                    } else {
                        agent_text(&line.text, &hit)
                    };
                    add_hit(&mut block, &mut shown, &full, &line.timestamp);
                } else {
                    let limit = match line.label {
                        Label::Agent => 600,
                        Label::Person | Label::UserTurn => 500,
                        Label::Summary => 400,
                        Label::Result => 200,
                        Label::Tool => 160,
                        Label::System | Label::Record => 120,
                    };
                    let body = text::clip_head(&text::one_line(&line.text), limit);
                    block.push_str(&format!(
                        "  {} {}: {}\n",
                        line.label.shown(),
                        corpus.local(&line.timestamp),
                        body
                    ));
                    shown.push(ShownLine {
                        author: Author::Near(line.label),
                        timestamp: line.timestamp.clone(),
                        text: line.text.clone(),
                        source_ref: SourceRef {
                            source_id: hit.source_ref.source_id.clone(),
                            byte_start: line.byte_start,
                            byte_len: line.byte_len,
                        },
                    });
                }
            }
            if !hit_seen {
                add_hit(&mut block, &mut shown, &hit.text, &hit.timestamp);
            }
        }
        Err(error) => {
            add_hit(&mut block, &mut shown, &hit.text, &hit.timestamp);
            block.push_str(&format!(
                "  (the records around it could not be read: {error})\n"
            ));
        }
    }
    Evidence {
        id,
        hit,
        block,
        lines: shown,
        digest: false,
    }
}

struct Summary<'a> {
    terms: &'a [String],
    counts: &'a [TermCount],
    found: usize,
    screened: Option<(usize, usize)>,
    kept: usize,
    /// The records read are the closest: the screen judged none relevant.
    closest: bool,
    rounds: usize,
    synced_through: Option<&'a str>,
    filters: &'a Filters,
    notes: &'a Notes,
    swept: Option<&'a Swept>,
}

impl<'a> Summary<'a> {
    fn of(pool: &'a Pool, kept: usize, notes: &'a Notes, filters: &'a Filters) -> Summary<'a> {
        Summary {
            terms: &pool.terms,
            counts: &pool.counts,
            found: pool.found.len(),
            screened: pool.screened.then_some((pool.threes, pool.twos)),
            kept,
            closest: pool.nothing_relevant(),
            rounds: pool.rounds,
            synced_through: pool.synced_through.as_deref(),
            filters,
            notes,
            swept: pool.swept.as_ref(),
        }
    }

    /// What a sweep adds to the statement that the records do not settle the question: that
    /// none of the person's own messages in the scope was judged to bear on it either.
    fn swept_sentence(&self) -> String {
        match self.swept {
            Some(swept) if swept.screened > 0 && swept.truncated => format!(
                " Of the person's {} messages in the scope, the newest {} were screened and none \
                 was judged to bear on the question.",
                swept.in_scope, swept.screened
            ),
            Some(swept) if swept.screened > 0 => format!(
                " Of the person's own messages in the scope, all {} were screened and none was \
                 judged to bear on the question.",
                swept.screened
            ),
            Some(_) => " The person wrote no message in the scope.".to_string(),
            None => String::new(),
        }
    }

    /// What the result says first when the records found do not settle the question: that,
    /// which strings were searched and where, and how many records they found. None when the
    /// screen judged records relevant.
    fn not_settled(&self) -> Option<String> {
        let mut scope = String::new();
        if let Some(cwd) = &self.filters.cwd {
            scope.push_str(&format!(
                " in conversations whose working directory contains {}",
                crate::json::string(cwd)
            ));
        }
        for (word, date) in [("from", &self.filters.from), ("to", &self.filters.to)] {
            if let Some(date) = date {
                scope.push_str(&format!(" {word} {date}"));
            }
        }
        let (any, by) = match self.terms.len() {
            1 => (
                "the string searched".to_string(),
                "the string searched".to_string(),
            ),
            count => (
                format!("any of the {count} strings searched"),
                format!("the {count} strings searched"),
            ),
        };
        let searched = self.terms.join(", ");
        let swept = self.swept_sentence();
        let swept_relevant = self
            .swept
            .is_some_and(|swept| swept.threes + swept.twos > 0);
        if self.found == 0 && !swept_relevant {
            let reads = if self.kept > 0 {
                format!(
                    " The answer reads the {} closest of the person's messages, with the records \
                     around them.",
                    self.kept
                )
            } else {
                String::new()
            };
            return Some(format!(
                "The records do not settle the question: no record holds {any}{scope}. Searched: \
                 {searched}.{swept}{reads}"
            ));
        }
        self.closest.then(|| match self.found {
            1 => format!(
                "The record found does not settle the question: the one record found by \
                 {by}{scope} was not judged relevant to it.{swept} The answer reads it, with the \
                 records around it. Searched: {searched}."
            ),
            found => format!(
                "The records found do not settle the question: none of the {found} records found \
                 by {by}{scope} was judged relevant to it.{swept} The answer reads {}, with the \
                 records around them. Searched: {searched}.",
                if self.kept < found {
                    format!("the {} closest", self.kept)
                } else {
                    "them".to_string()
                }
            ),
        })
    }

    fn lines(&self, corpus: &Corpus) -> Vec<String> {
        let mut lines = vec![format!(
            "Search strings{}: {}",
            if self.rounds > 1 {
                format!(" ({} rounds)", self.rounds)
            } else {
                String::new()
            },
            self.terms.join(", ")
        )];
        if let Some((from, to)) = &self.notes.period {
            lines.push(format!(
                "Period inferred from the question: {from} to {to} (set it with --from and --to)"
            ));
        }
        let partial: Vec<String> = self
            .counts
            .iter()
            .filter(|count| (count.returned as u64) < count.total)
            .map(|count| {
                format!(
                    "{} ({} {}/{}{})",
                    count.term,
                    if count.who == Who::Person {
                        "person"
                    } else {
                        "agent"
                    },
                    count.returned,
                    count.total,
                    count
                        .oldest_taken
                        .as_deref()
                        .map(|time| format!(", none before {}", corpus.local(time)))
                        .unwrap_or_default()
                )
            })
            .collect();
        if !partial.is_empty() {
            lines.push(format!(
                "Held by many records, of which only the newest were taken: {}",
                partial.join(", ")
            ));
        }
        lines.push(match self.screened {
            Some(_) if self.closest => format!(
                "Candidates {} -> relevant 0 -> the closest read {}",
                self.found, self.kept
            ),
            Some((threes, twos)) => format!(
                "Candidates {} -> relevant {} (3 points {}, 2 points {}) -> records read {}",
                self.found,
                threes + twos,
                threes,
                twos,
                self.kept
            ),
            None => format!(
                "Candidates {} -> shown {} (by the number of search strings each holds)",
                self.found, self.kept
            ),
        });
        if let Some(swept) = self.swept {
            lines.push(format!(
                "Swept the person's messages: {} in scope, {} screened{}, relevant {} (3 points {}, \
                 2 points {}){}",
                swept.in_scope,
                swept.screened,
                if swept.screened > 0 {
                    format!(
                        " ({} to {})",
                        corpus.local(&swept.oldest),
                        corpus.local(&swept.newest)
                    )
                } else {
                    String::new()
                },
                swept.threes + swept.twos,
                swept.threes,
                swept.twos,
                if swept.stale_sources > 0 || swept.unscanned_bytes > 0 {
                    format!(
                        "; stale sources {}, unscanned bytes {}",
                        swept.stale_sources, swept.unscanned_bytes
                    )
                } else {
                    String::new()
                }
            ));
        }
        if let Some((shown, left_out)) = self.notes.digest {
            lines.push(format!(
                "Sweep digest: {} more of the person's relevant messages were given to the answer \
                 in one line each{}",
                shown,
                if left_out > 0 {
                    format!(" ({left_out} left out)")
                } else {
                    String::new()
                }
            ));
        }
        if let Some(coverage) = &self.notes.coverage {
            lines.push(format!(
                "Sessions in the scope with a message from the person: {}; the records read come \
                 from {}; {} {}",
                coverage.sessions_in_scope,
                coverage.sessions_with_evidence,
                coverage.unread_total,
                if coverage.swept {
                    "others had every message of the person's screened and none judged relevant"
                } else {
                    "others were not read"
                }
            ));
            if !coverage.swept {
                for session in &coverage.unread {
                    lines.push(format!(
                        "  not read: {} | {} | {} to {} | {} messages | {}",
                        session.session,
                        if session.project.is_empty() {
                            "?"
                        } else {
                            &session.project
                        },
                        corpus.local(&session.first),
                        corpus.local(&session.last),
                        session.messages,
                        session.opening
                    ));
                }
                if coverage.unread_total > coverage.unread.len() {
                    lines.push(format!(
                        "  … and {} more sessions not read",
                        coverage.unread_total - coverage.unread.len()
                    ));
                }
            }
        }
        lines.push(format!(
            "The corpus has read the logs through {} (run `ebira sync` first to include the latest)",
            self.synced_through
                .map(|time| format!("{} {}", corpus.local(time), corpus.zone()))
                .unwrap_or_else(|| "an unknown time".to_string())
        ));
        for check in &self.notes.checks {
            lines.push(format!(
                "Reversal check {} ({}, {} 「{}」): {} -> {} candidates -> {} may change it",
                check.id,
                check.date,
                check.kind,
                text::clip_head(&text::one_line(&check.quote), 30),
                if !check.swept {
                    format!("strings {}", check.terms.join(", "))
                } else if check.candidates < check.later_total {
                    format!(
                        "the earliest and the latest {} of the person's {} later messages",
                        check.candidates, check.later_total
                    )
                } else {
                    format!("every later message of the person's ({})", check.candidates)
                },
                check.candidates,
                check.changes
            ));
        }
        for recent in &self.notes.recent {
            lines.push(format!("Recent human messages in {}: last {} of {} within the search's date and directory bounds; newest {}; stale sources {}, unscanned bytes {}",
                recent.session, recent.hits.len(), recent.total,
                recent.hits.first().map(|hit| hit.timestamp.as_str()).unwrap_or("unknown"),
                recent.stale_sources, recent.unscanned_bytes));
        }
        if self.notes.recent_sessions_omitted > 0 {
            lines.push(format!("Recent-message check omitted {} other related sessions; this is not an exhaustive decision history", self.notes.recent_sessions_omitted));
        }
        lines.extend(self.notes.issues.iter().cloned());
        lines
    }

    fn json(&self) -> String {
        let terms = self.terms.iter().map(|term| {
            let mut object = crate::json::Object::new();
            object.text("text", term);
            for who in [Who::Person, Who::Agent] {
                let key = if who == Who::Person {
                    "person"
                } else {
                    "agent"
                };
                match self
                    .counts
                    .iter()
                    .find(|count| &count.term == term && count.who == who)
                {
                    Some(count) => {
                        let mut taken = crate::json::Object::new();
                        taken
                            .number("returned", count.returned as u64)
                            .number("total", count.total);
                        // Only when not all were taken: the oldest that was.
                        if let Some(oldest) = &count.oldest_taken {
                            taken.name("oldest_taken", oldest);
                        }
                        object.raw(key, &taken.finish())
                    }
                    None => object.raw(key, "null"),
                };
            }
            object.finish()
        });
        let checks = self.notes.checks.iter().map(|check| {
            crate::json::Object::new()
                .name("id", &check.id)
                .name("date", &check.date)
                .name("kind", &check.kind)
                .text("quote", &check.quote)
                .raw(
                    "terms",
                    &crate::json::strings(check.terms.iter().map(String::as_str)),
                )
                .boolean("swept", check.swept)
                .number("later_messages", check.later_total as u64)
                .number("candidates", check.candidates as u64)
                .number("may_change", check.changes as u64)
                .finish()
        });
        crate::json::Object::new()
            .raw("terms", &crate::json::array(terms))
            .number("rounds", self.rounds as u64)
            .raw(
                "period_inferred",
                &crate::json::or_null(self.notes.period.as_ref().map(|(from, to)| {
                    crate::json::Object::new()
                        .name("from", from)
                        .name("to", to)
                        .finish()
                })),
            )
            .number("candidates", self.found as u64)
            .optional_number(
                "relevant",
                self.screened.map(|(threes, twos)| (threes + twos) as u64),
            )
            .number("read", self.kept as u64)
            .optional("synced_through", self.synced_through)
            .raw("reversal_checks", &crate::json::array(checks))
            .raw(
                "recent_messages",
                &crate::json::array(self.notes.recent.iter().map(|recent| {
                    crate::json::Object::new()
                        .name("session", &recent.session)
                        .number("total", recent.total as u64)
                        .number("read", recent.hits.len() as u64)
                        .optional(
                            "newest",
                            recent.hits.first().map(|hit| hit.timestamp.as_str()),
                        )
                        .optional(
                            "oldest_read",
                            recent.hits.last().map(|hit| hit.timestamp.as_str()),
                        )
                        .boolean("truncated", recent.total > recent.hits.len())
                        .number("stale_sources", recent.stale_sources)
                        .number("unscanned_source_bytes", recent.unscanned_bytes)
                        .finish()
                })),
            )
            .number(
                "recent_sessions_omitted",
                self.notes.recent_sessions_omitted as u64,
            )
            .raw(
                "sweep_digest",
                &crate::json::or_null(self.notes.digest.map(|(shown, left_out)| {
                    crate::json::Object::new()
                        .number("shown", shown as u64)
                        .number("left_out", left_out as u64)
                        .finish()
                })),
            )
            .raw(
                "coverage",
                &crate::json::or_null(self.notes.coverage.as_ref().map(|coverage| {
                    let unread = coverage.unread.iter().map(|session| {
                        crate::json::Object::new()
                            .name("session", &session.session)
                            .text("project", &session.project)
                            .name("first", &session.first)
                            .name("last", &session.last)
                            .number("messages", session.messages as u64)
                            .text("opening", &session.opening)
                            .finish()
                    });
                    crate::json::Object::new()
                        .number("sessions_in_scope", coverage.sessions_in_scope as u64)
                        .number(
                            "sessions_with_evidence",
                            coverage.sessions_with_evidence as u64,
                        )
                        .number("sessions_unread", coverage.unread_total as u64)
                        .boolean("unread_swept", coverage.swept)
                        .raw("unread", &crate::json::array(unread))
                        .finish()
                })),
            )
            .raw(
                "sweep",
                &crate::json::or_null(self.swept.map(|swept| {
                    crate::json::Object::new()
                        .number("in_scope", swept.in_scope as u64)
                        .number("screened", swept.screened as u64)
                        .number("relevant", (swept.threes + swept.twos) as u64)
                        .number("threes", swept.threes as u64)
                        .number("twos", swept.twos as u64)
                        .optional(
                            "oldest",
                            (swept.screened > 0).then_some(swept.oldest.as_str()),
                        )
                        .optional(
                            "newest",
                            (swept.screened > 0).then_some(swept.newest.as_str()),
                        )
                        .number("stale_sources", swept.stale_sources)
                        .number("unscanned_source_bytes", swept.unscanned_bytes)
                        .finish()
                })),
            )
            .raw(
                "issues",
                &crate::json::strings(self.notes.issues.iter().map(String::as_str)),
            )
            .finish()
    }
}

fn cost_line(spent: &[Spend]) -> String {
    if spent.is_empty() {
        return "Cost: $0 (the API was not called)".to_string();
    }
    let mut steps: Vec<(&str, f64)> = Vec::new();
    let mut unknown = false;
    for spend in spent {
        match spend.usd {
            Some(usd) => match steps.iter_mut().find(|(step, _)| *step == spend.step) {
                Some((_, total)) => *total += usd,
                None => steps.push((spend.step, usd)),
            },
            None => unknown = true,
        }
    }
    let total: f64 = steps.iter().map(|(_, usd)| usd).sum();
    let parts: Vec<String> = steps
        .iter()
        .map(|(step, usd)| format!("{step} ${usd:.3}"))
        .collect();
    let tokens: u64 = spent
        .iter()
        .map(|spend| spend.usage.input + spend.usage.output)
        .sum();
    format!(
        "Cost: ${total:.3} ({}), {tokens} tokens{}",
        parts.join(", "),
        if unknown {
            " (calls of models without a known price are not counted)"
        } else {
            ""
        }
    )
}

/// What the run cost by step, counting the calls whose price is known; `unpriced_calls` says
/// how many were left out, as `ask-usage` does, so a total is never read as complete when it
/// is not.
fn cost_json(spent: &[Spend]) -> String {
    let mut steps: Vec<(&str, f64)> = Vec::new();
    let mut unpriced: u64 = 0;
    for spend in spent {
        let usd = spend.usd.unwrap_or_else(|| {
            unpriced += 1;
            0.0
        });
        match steps.iter_mut().find(|(step, _)| *step == spend.step) {
            Some((_, total)) => *total += usd,
            None => steps.push((spend.step, usd)),
        }
    }
    let total: f64 = steps.iter().map(|(_, usd)| usd).sum();
    let mut models: Vec<&str> = Vec::new();
    for spend in spent {
        if !models.contains(&spend.model.as_str()) {
            models.push(&spend.model);
        }
    }
    let mut object = crate::json::Object::new();
    object.raw("total", &format!("{total:.4}"));
    for (step, usd) in &steps {
        object.raw(&step.replace('-', "_"), &format!("{usd:.4}"));
    }
    object
        .raw("models", &crate::json::strings(models))
        .number(
            "tokens",
            spent
                .iter()
                .map(|spend| spend.usage.input + spend.usage.output)
                .sum(),
        )
        .number("unpriced_calls", unpriced)
        .finish()
}

fn source_ref_text(source: &SourceRef) -> String {
    format!(
        "{} @{}+{}",
        source.source_id, source.byte_start, source.byte_len
    )
}

fn source_ref_json(hit: &Hit, source: &SourceRef) -> String {
    let line = (source == &hit.source_ref).then_some(hit.line);
    crate::json::source_ref(
        &source.source_id,
        &hit.source_path,
        line,
        source.byte_start,
        source.byte_len,
    )
}

/// For measuring: what the answer would read, how each record is shown, and what the steps
/// before it screened and judged, without calling the answer model.
fn evidence_report(
    corpus: &Corpus,
    options: &Options,
    evidence: &[Evidence],
    pool: &Pool,
    summary: &Summary,
    spent: &[Spend],
) -> String {
    let mut ids: HashSet<String> = HashSet::new();
    for item in evidence {
        ids.insert(item.hit.source_ref.source_id.clone());
    }
    for key in pool.seen.iter().chain(pool.found.iter()) {
        ids.insert(key.source_id.clone());
    }
    for (hit, _) in pool.relevant.iter().chain(pool.near.iter()) {
        ids.insert(hit.source_ref.source_id.clone());
    }
    if let Some(swept) = &pool.swept {
        for hit in &swept.messages {
            ids.insert(hit.source_ref.source_id.clone());
        }
    }
    let paths = corpus.source_paths(&ids).unwrap_or_default();
    let reference = |key: &SourceRef| {
        crate::json::Object::new()
            .name("source_id", &key.source_id)
            .number("byte_start", key.byte_start)
            .finish()
    };
    let scored = |list: &[(Hit, u64)]| {
        crate::json::array(list.iter().map(|(hit, score)| {
            crate::json::Object::new()
                .name("source_id", &hit.source_ref.source_id)
                .number("byte_start", hit.source_ref.byte_start)
                .number("score", *score)
                .name("author", Author::Found(hit.who).id())
                .finish()
        }))
    };
    let items = evidence.iter().map(|item| {
        let lines = item.lines.iter().map(|line| {
            crate::json::Object::new()
                .name("source_id", &line.source_ref.source_id)
                .number("byte_start", line.source_ref.byte_start)
                .name("author", line.author.id())
                .finish()
        });
        crate::json::Object::new()
            .name("id", &item.id)
            .name("source_id", &item.hit.source_ref.source_id)
            .number("byte_start", item.hit.source_ref.byte_start)
            .name("author", Author::Found(item.hit.who).id())
            .boolean("digest", item.digest)
            .raw("lines", &crate::json::array(lines))
            .text("block", &item.block)
            .finish()
    });
    let swept = pool
        .swept
        .as_ref()
        .map(|swept| {
            crate::json::array(swept.messages.iter().map(|hit| reference(&hit.source_ref)))
        })
        .unwrap_or_else(|| "[]".to_string());
    let sources = {
        let mut object = crate::json::Object::new();
        for (id, path) in &paths {
            object.text(id, path);
        }
        object.finish()
    };
    let mut out = crate::json::Object::new()
        .name("disposition", "evidence")
        .name("mode", "ask")
        .text("question", &options.question)
        .raw("evidence", &crate::json::array(items))
        .raw(
            "pool",
            &crate::json::Object::new()
                .raw("relevant", &scored(&pool.relevant))
                .raw("near", &scored(&pool.near))
                .raw("seen", &crate::json::array(pool.seen.iter().map(reference)))
                .raw(
                    "found",
                    &crate::json::array(pool.found.iter().map(reference)),
                )
                .raw("swept", &swept)
                .finish(),
        )
        .raw("sources", &sources)
        .name("local_time_zone", &corpus.zone())
        .raw("search", &summary.json())
        .raw("cost_usd", &cost_json(spent))
        .finish();
    out.push('\n');
    out
}

/// Without the API: the records the answer model would have read.
fn records_report(
    corpus: &Corpus,
    options: &Options,
    evidence: &[Evidence],
    summary: &Summary,
) -> String {
    if options.text_format {
        let mut out = format!("Question: {}\n\n", options.question);
        out.push_str(
            "Without the API: the records the answer model would read. Nothing was spent.\n\n",
        );
        for item in evidence {
            out.push_str(&format!(
                "{}  {}\n\n",
                item.block.trim_end(),
                source_ref_text(&item.hit.source_ref)
            ));
        }
        if evidence.is_empty() {
            out.push_str("No record holds the search strings.\n\n");
        }
        out.push_str("--\n");
        for line in summary.lines(corpus) {
            out.push_str(&line);
            out.push('\n');
        }
        return out;
    }
    let records = evidence.iter().map(|item| {
        crate::json::Object::new()
            .name("id", &item.id)
            .name("timestamp", &item.hit.timestamp)
            .name("local_time", &corpus.local(&item.hit.timestamp))
            .name("author", Author::Found(item.hit.who).id())
            .name("session", &item.hit.session)
            .raw(
                "source_ref",
                &source_ref_json(&item.hit, &item.hit.source_ref),
            )
            .text("text", &item.block)
            .finish()
    });
    let mut out = crate::json::Object::new()
        .name(
            "disposition",
            if evidence.is_empty() {
                "no_records"
            } else {
                "records"
            },
        )
        .name("mode", "ask")
        .text("question", &options.question)
        .raw("records", &crate::json::array(records))
        .name("local_time_zone", &corpus.zone())
        .raw("search", &summary.json())
        .finish();
    out.push('\n');
    out
}

struct Finding<'a> {
    item: Option<&'a Evidence>,
    /// The line the quote was found in: whose words they are, when, and where in the log.
    line: Option<&'a ShownLine>,
    evidence: String,
    kind: String,
    quote: String,
    verified: bool,
    note: String,
}

impl Finding<'_> {
    /// Who wrote the quoted words, when, and where: the quoted line if found, else the record.
    fn origin(&self) -> Option<(Author, &str, &SourceRef)> {
        match (self.line, self.item) {
            (Some(line), _) => Some((line.author, line.timestamp.as_str(), &line.source_ref)),
            (None, Some(item)) => Some((
                Author::Found(item.hit.who),
                item.hit.timestamp.as_str(),
                &item.hit.source_ref,
            )),
            (None, None) => None,
        }
    }
}

/// The answer's findings, oldest first. A quote is verified only when one record's own text
/// holds it.
fn findings<'a>(corpus: &Corpus, answer: &Value, evidence: &'a [Evidence]) -> Vec<Finding<'a>> {
    let mut list: Vec<Finding> = answer
        .array_at("findings")
        .iter()
        .map(|finding| {
            let id = finding.str_at("evidence").trim().to_string();
            let item = evidence.iter().find(|item| item.id == id);
            let quote = finding.str_at("quote").trim().to_string();
            // Words an agent quotes from the person are the person's: their line is the origin.
            let line = item.and_then(|item| {
                let quoted = |line: &&ShownLine| text::quote_found(&quote, &line.text);
                item.lines
                    .iter()
                    .filter(|line| line.author.is_person())
                    .find(quoted)
                    .or_else(|| item.lines.iter().find(quoted))
            });
            Finding {
                verified: line.is_some(),
                item,
                line,
                evidence: id,
                kind: finding.str_at("kind").to_string(),
                quote,
                note: finding.str_at("note").to_string(),
            }
        })
        .collect();
    list.sort_by(|a, b| {
        let time = |finding: &Finding| -> String {
            finding
                .origin()
                .map(|(_, time, _)| time.to_string())
                .unwrap_or_default()
        };
        earliest_first(corpus, &time(a), &time(b))
    });
    list
}

fn strings(value: &Value, key: &str) -> Vec<String> {
    value
        .array_at(key)
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .filter(|item| !item.trim().is_empty())
        .collect()
}

fn answer_report(
    corpus: &Corpus,
    options: &Options,
    answer: Option<&Value>,
    evidence: &[Evidence],
    summary: &Summary,
    spent: &[Spend],
) -> String {
    let empty = Value::Object(Vec::new());
    let answer_value = answer.unwrap_or(&empty);
    let list = findings(corpus, answer_value, evidence);
    let more_terms = strings(answer_value, "more_terms");
    if !options.text_format {
        let findings_json = list.iter().map(|finding| {
            let mut object = crate::json::Object::new();
            object
                .name("evidence", &finding.evidence)
                .name("kind", &finding.kind)
                .text("quote", &finding.quote)
                .boolean("quote_verified", finding.verified)
                .text("relevance", &finding.note);
            match (finding.origin(), finding.item) {
                (Some((author, when, source)), Some(item)) => {
                    object
                        .name("author", author.id())
                        .name("timestamp", when)
                        .name("local_time", &corpus.local(when))
                        .name("project", &item.hit.project)
                        .name("app", &item.hit.app)
                        .name("session", &item.hit.session)
                        .raw("source_ref", &source_ref_json(&item.hit, source));
                }
                _ => {
                    object
                        .raw("author", "null")
                        .raw("timestamp", "null")
                        .raw("local_time", "null")
                        .raw("project", "null")
                        .raw("app", "null")
                        .raw("session", "null")
                        .raw("source_ref", "null");
                }
            }
            object.finish()
        });
        let list_of =
            |key: &str| crate::json::strings(strings(answer_value, key).iter().map(String::as_str));
        let mut out = crate::json::Object::new()
            .name(
                "disposition",
                match answer {
                    None => "no_records",
                    Some(_) if summary.closest => "nothing_relevant",
                    Some(_) => "answered",
                },
            )
            .name("mode", "ask")
            .name("api", &options.endpoint.name)
            .text("question", &options.question)
            .raw(
                "not_settled",
                &crate::json::or_null(summary.not_settled().map(|lead| crate::json::string(&lead))),
            )
            .raw(
                "answer",
                &crate::json::or_null(
                    answer.map(|value| crate::json::string(value.str_at("answer"))),
                ),
            )
            .raw("findings", &crate::json::array(findings_json))
            .raw("timeline", &list_of("timeline"))
            .raw("gaps", &list_of("gaps"))
            .raw("more_terms", &list_of("more_terms"))
            .name("local_time_zone", &corpus.zone())
            .raw("search", &summary.json())
            .raw("cost_usd", &cost_json(spent))
            .finish();
        out.push('\n');
        return out;
    }

    let mut out = format!("Question: {}\n\n", options.question);
    if let Some(lead) = summary.not_settled() {
        out.push_str(&lead);
        out.push_str("\n\n");
    }
    match answer {
        None => out.push_str("Try other words, or name them with --term.\n\n"),
        Some(answer) => {
            out.push_str(&format!("Answer\n{}\n\n", answer.str_at("answer").trim()));
            if !list.is_empty() {
                out.push_str(&format!(
                    "Evidence, oldest first, times in {}\n",
                    corpus.zone()
                ));
                for finding in &list {
                    match (finding.origin(), finding.item) {
                        (Some((author, when, _)), Some(item)) => out.push_str(&format!(
                            "- {} | {} | {} | {}\n",
                            finding.kind,
                            corpus.local(when),
                            author.id(),
                            if item.hit.project.is_empty() {
                                "?"
                            } else {
                                &item.hit.project
                            }
                        )),
                        _ => out.push_str(&format!(
                            "- {} | ({} is not among the records given)\n",
                            finding.kind, finding.evidence
                        )),
                    }
                    out.push_str(&format!(
                        "  「{}」{}\n",
                        finding.quote,
                        if finding.verified {
                            ""
                        } else {
                            " (not found in the records)"
                        }
                    ));
                    if !finding.note.trim().is_empty() {
                        out.push_str(&format!("  -> {}\n", finding.note.trim()));
                    }
                    if let Some((_, _, source)) = finding.origin() {
                        out.push_str(&format!("  {}\n", source_ref_text(source)));
                    }
                }
                out.push('\n');
            }
            for (title, key) in [
                ("How it changed", "timeline"),
                ("Not settled by the records", "gaps"),
            ] {
                let items = strings(answer, key);
                if !items.is_empty() {
                    out.push_str(title);
                    out.push('\n');
                    for item in items {
                        out.push_str(&format!("- {}\n", item.trim()));
                    }
                    out.push('\n');
                }
            }
            if !more_terms.is_empty() {
                let terms: Vec<String> = more_terms
                    .iter()
                    .map(|term| format!("--term {}", crate::json::string(term)))
                    .collect();
                out.push_str(&format!(
                    "To search further\n  ebira ask {} {}\n\n",
                    crate::json::string(&options.question),
                    terms.join(" ")
                ));
            }
        }
    }
    out.push_str("--\n");
    for line in summary.lines(corpus) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&cost_line(spent));
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(text: &str, who: Who, timestamp: &str, terms: &[&str]) -> Hit {
        Hit {
            source_ref: SourceRef {
                source_id: format!("claude-{timestamp}"),
                byte_start: 10,
                byte_len: 5,
            },
            source_path: "/logs/s.jsonl".to_string(),
            line: 1,
            session: "s".to_string(),
            timestamp: timestamp.to_string(),
            who,
            app: "claude".to_string(),
            project: "work/game".to_string(),
            text: text.to_string(),
            images: 0,
            terms: terms.iter().map(|term| term.to_string()).collect(),
            note: String::new(),
        }
    }

    fn line(label: Label, timestamp: &str, text: &str, is_hit: bool, byte_start: u64) -> Line {
        Line {
            label,
            timestamp: timestamp.to_string(),
            text: text.to_string(),
            is_hit,
            byte_start,
            byte_len: 3,
        }
    }

    fn corpus() -> Corpus {
        Corpus::for_tests(540)
    }

    #[test]
    fn spreads_a_sample_over_the_whole() {
        let items: Vec<usize> = (0..100).collect();
        let picked = spread(&items, 5);
        assert_eq!(picked, vec![0, 24, 49, 74, 99]);
        assert_eq!(spread(&items[..3], 5), vec![0, 1, 2]);
        assert_eq!(spread(&items, 1), vec![0]);
    }

    #[test]
    fn cleans_terms() {
        let terms = clean_terms(
            ["  色が ", "MapLegend", "色が", "", "凡例"]
                .into_iter()
                .map(String::from),
        );
        assert_eq!(terms, vec!["色が", "MapLegend", "凡例"]);
    }

    #[test]
    fn searches_each_string_with_its_spacing_variants_once() {
        let terms: Vec<String> = ["csv取り込み", "CSV 取り込み", "日付", "Rust 製のCLI"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            with_spacing_variants(&terms),
            vec![
                "csv取り込み",
                "csv 取り込み",
                // CSV 取り込み and its variant CSV取り込み are those two, ASCII case aside.
                "日付",
                "Rust 製のCLI",
                "Rust 製の CLI",
                "Rust製のCLI"
            ]
        );
        assert_eq!(
            with_spacing_variants(&terms),
            with_spacing_variants(&terms),
            "the same every time"
        );
        assert!(holds_term(&terms, "CSV取り込み"));
        assert!(!holds_term(&terms, "csv"));
    }

    #[test]
    fn reads_the_closest_when_the_screen_judged_none_relevant() {
        let scored = |text: &str, score: u64| {
            let mut found = hit(text, Who::Agent, "2026-09-18T01:41:00Z", &["公開"]);
            found.source_ref.source_id = text.to_string();
            (found, score)
        };
        let pool = Pool {
            near: vec![
                scored("unrelated", 0),
                scored("shares words", 1),
                scored("unrelated too", 0),
                scored("shares words too", 1),
            ],
            screened: true,
            ..Pool::default()
        };
        assert!(pool.nothing_relevant());
        assert!(pool.best(8).is_empty());
        let closest: Vec<String> = pool.closest(3).into_iter().map(|hit| hit.text).collect();
        assert_eq!(
            closest,
            vec!["shares words", "shares words too", "unrelated"],
            "a record that shares words with the question comes first"
        );
        let without_the_api = Pool {
            screened: false,
            ..Pool::default()
        };
        assert!(
            !without_the_api.nothing_relevant(),
            "nothing was judged without the API"
        );
    }

    #[test]
    fn says_what_it_searched_when_no_record_holds_the_strings() {
        let corpus = corpus();
        let pool = Pool {
            terms: vec!["ダークモード".to_string(), "dark mode".to_string()],
            rounds: 1,
            ..Pool::default()
        };
        let notes = Notes::default();
        let filters = Filters {
            cwd: Some("kurage-app".to_string()),
            ..Filters::default()
        };
        let summary = Summary::of(&pool, 0, &notes, &filters);
        let mut options = Options::thorough("ダークモードはどうするって決めた？".to_string());
        options.text_format = true;
        let text = answer_report(&corpus, &options, None, &[], &summary, &[]);
        assert!(
            text.contains(
                "The records do not settle the question: no record holds any of the 2 strings \
                 searched in conversations whose working directory contains \"kurage-app\". \
                 Searched: ダークモード, dark mode.\n\nTry other words"
            ),
            "{text}"
        );
        options.text_format = false;
        let json = answer_report(&corpus, &options, None, &[], &summary, &[]);
        assert!(
            json.starts_with(r#"{"disposition":"no_records","mode":"ask","api":"anthropic","question":"ダークモードはどうするって決めた？","not_settled":"The records do not settle the question: no record holds any of the 2 strings searched in conversations whose working directory contains \"kurage-app\". Searched: ダークモード, dark mode.","answer":null,"#),
            "{json}"
        );
    }

    #[test]
    fn says_the_records_found_do_not_settle_it_before_the_answer_from_the_closest() {
        let corpus = corpus();
        let words = "公開するときは README と LICENSE を整えましょう。";
        let found = hit(words, Who::Agent, "2026-09-18T01:41:00Z", &["公開"]);
        let lines = vec![
            line(
                Label::Person,
                "2026-09-18T01:40:00Z",
                "そのうち crates.io に出したいな",
                false,
                5,
            ),
            line(Label::Agent, "2026-09-18T01:41:00Z", words, true, 10),
        ];
        let evidence = vec![compose(&corpus, "E1".to_string(), found.clone(), Ok(lines))];
        let pool = Pool {
            terms: vec!["公開".to_string(), "publish".to_string()],
            rounds: 1,
            found: (0..4)
                .map(|byte_start| SourceRef {
                    source_id: "claude-a".to_string(),
                    byte_start,
                    byte_len: 1,
                })
                .collect(),
            near: vec![(found, 1)],
            screened: true,
            ..Pool::default()
        };
        let notes = Notes::default();
        let filters = Filters::default();
        let summary = Summary::of(&pool, 1, &notes, &filters);
        let answer = value::parse(
            r#"{"answer":"公開した記録はありません。","findings":[
                  {"evidence":"E1","kind":"other","quote":"そのうち crates.io に出したいな","note":""}],
                "timeline":[],"gaps":[],"more_terms":[]}"#,
        )
        .unwrap();
        let mut options = Options::thorough("いつ公開した？".to_string());
        options.text_format = true;
        let text = answer_report(&corpus, &options, Some(&answer), &evidence, &summary, &[]);
        assert!(
            text.starts_with(
                "Question: いつ公開した？\n\nThe records found do not settle the question: none of \
                 the 4 records found by the 2 strings searched was judged relevant to it. The \
                 answer reads the 1 closest, with the records around them. Searched: 公開, \
                 publish.\n\nAnswer\n公開した記録はありません。"
            ),
            "{text}"
        );
        assert!(
            text.contains("Candidates 4 -> relevant 0 -> the closest read 1"),
            "{text}"
        );
        assert!(
            !text.contains("(not found in the records)"),
            "the person's words beside the record found are checked like any: {text}"
        );
        options.text_format = false;
        let json = answer_report(&corpus, &options, Some(&answer), &evidence, &summary, &[]);
        assert!(
            json.starts_with(r#"{"disposition":"nothing_relevant","mode":"ask""#),
            "{json}"
        );
        assert!(
            json.contains(r#""quote_verified":true,"relevance":"","author":"person""#),
            "{json}"
        );
    }

    #[test]
    fn ranks_by_terms_then_person_then_newer() {
        let mut hits = vec![
            hit("a", Who::Agent, "2026-10-14T00:00:00Z", &["色が"]),
            hit("b", Who::Person, "2026-08-20T00:00:00Z", &["色が"]),
            hit("c", Who::Agent, "2026-09-01T00:00:00Z", &["色が", "凡例"]),
            hit("d", Who::Person, "2026-10-01T00:00:00Z", &["色が"]),
        ];
        rank(&corpus(), &mut hits);
        let order: Vec<&str> = hits.iter().map(|hit| hit.text.as_str()).collect();
        assert_eq!(order, vec!["c", "d", "b", "a"]);
    }

    #[test]
    fn a_record_past_a_rounds_limit_can_come_back_in_a_later_round() {
        let found = vec![
            hit("a", Who::Person, "2026-10-01T00:00:00Z", &["色が", "凡例"]),
            hit("b", Who::Person, "2026-10-02T00:00:00Z", &["色が"]),
            hit("c", Who::Person, "2026-10-03T00:00:00Z", &["色が"]),
        ];
        let mut pool = Pool::default();
        let first = take_new(&mut pool, &corpus(), found.clone(), 2);
        let texts = |hits: &[Hit]| hits.iter().map(|hit| hit.text.clone()).collect::<Vec<_>>();
        assert_eq!(texts(&first), vec!["a", "c"]);
        let second = take_new(&mut pool, &corpus(), found, 2);
        assert_eq!(
            texts(&second),
            vec!["b"],
            "the record past the first round's limit was not screened, so it comes back"
        );
        assert_eq!(pool.found.len(), 3, "a record found twice is counted once");
    }

    #[test]
    fn composes_a_block_with_the_hit_marked() {
        let lines = vec![
            line(
                Label::Agent,
                "2026-10-13T18:58:00Z",
                "地図を開きました",
                false,
                100,
            ),
            line(
                Label::Person,
                "2026-10-13T19:01:10Z",
                "凡例の色が薄くなってる\n[7 images]",
                true,
                10,
            ),
        ];
        let evidence = compose(
            &corpus(),
            "E1".to_string(),
            hit(
                "凡例の色が薄くなってる",
                Who::Person,
                "2026-10-13T19:01:10Z",
                &["薄く"],
            ),
            Ok(lines),
        );
        assert!(
            evidence.block.starts_with("E1 | 2026-10-14 04:01 | "),
            "{}",
            evidence.block
        );
        assert!(evidence
            .block
            .contains("  agent 2026-10-14 03:58: 地図を開きました"));
        assert!(evidence.block.contains("▶ person "));
        assert_eq!(evidence.lines.len(), 2);
        assert_eq!(evidence.lines[1].author, Author::Found(Who::Person));
    }

    #[test]
    fn attributes_a_quote_to_the_line_it_came_from() {
        // An agent's record was found; the quote is the person's reply next to it.
        let lines = vec![
            line(
                Label::Agent,
                "2026-10-13T19:48:32Z",
                "色の担当の結論です。",
                true,
                500,
            ),
            line(
                Label::Person,
                "2026-10-13T19:49:10Z",
                "拡大すると凡例の色だけ元に戻ってた",
                false,
                900,
            ),
        ];
        let found = hit(
            "色の担当の結論です。",
            Who::Agent,
            "2026-10-13T19:48:32Z",
            &["色"],
        );
        let evidence = vec![compose(&corpus(), "E1".to_string(), found, Ok(lines))];
        let answer = value::parse(
            r#"{"findings":[{"evidence":"E1","kind":"problem","quote":"凡例の色だけ元に戻ってた","note":""},
                            {"evidence":"E1","kind":"other","quote":"どこにもない言葉","note":""}]}"#,
        )
        .unwrap();
        let list = findings(&corpus(), &answer, &evidence);
        let first = list
            .iter()
            .find(|finding| finding.kind == "problem")
            .unwrap();
        let (who, when, source) = first.origin().unwrap();
        assert_eq!(who, Author::Near(Label::Person));
        assert_eq!(who.id(), "person");
        assert_eq!(when, "2026-10-13T19:49:10Z");
        assert_eq!(source.byte_start, 900);
        assert!(first.verified);
        let missing = list.iter().find(|finding| finding.kind == "other").unwrap();
        assert!(!missing.verified);
        assert_eq!(missing.origin().unwrap().0, Author::Found(Who::Agent));
    }

    #[test]
    fn credits_quoted_words_to_the_person_who_said_them() {
        // The agent's note quotes the person's words; the person's own message is also there.
        let words = "古い色を残せとは言っていない";
        let lines = vec![
            line(
                Label::Agent,
                "2026-10-04T07:22:16Z",
                &format!("本人「{words}」と。"),
                true,
                500,
            ),
            line(Label::Person, "2026-10-04T07:21:47Z", words, false, 400),
        ];
        let found = hit(
            "本人「…」と。",
            Who::Agent,
            "2026-10-04T07:22:16Z",
            &["古い色"],
        );
        let evidence = vec![compose(&corpus(), "E1".to_string(), found, Ok(lines))];
        let answer = value::parse(&format!(
            r#"{{"findings":[{{"evidence":"E1","kind":"reversal","quote":"{words}","note":""}}]}}"#
        ))
        .unwrap();
        let list = findings(&corpus(), &answer, &evidence);
        let (who, when, source) = list[0].origin().unwrap();
        assert_eq!(who, Author::Near(Label::Person));
        assert_eq!(when, "2026-10-04T07:21:47Z");
        assert_eq!(source.byte_start, 400);
    }

    #[test]
    fn takes_each_claims_likeliest_change_before_anyones_second() {
        let change = |text: &str, score: u64| {
            let mut found = hit(text, Who::Person, "2026-10-05T00:00:00Z", &["a"]);
            found.source_ref.source_id = text.to_string();
            (found, score)
        };
        let lists = vec![
            vec![change("a1", 3), change("a2", 3), change("a3", 3)],
            vec![],
            vec![change("c1", 2), change("a2", 2)],
            vec![change("d1", 2)],
        ];
        let taken: Vec<String> = take_in_turn(lists, 4)
            .into_iter()
            .map(|hit| hit.text)
            .collect();
        assert_eq!(taken, vec!["a1", "c1", "d1", "a2"]);
    }

    #[test]
    fn keeps_the_nearest_lines_and_stops_after_the_reply() {
        let line = |label: Label, text: &str, is_hit: bool| line(label, "", text, is_hit, 0);
        let lines = vec![
            line(Label::UserTurn, "a", false),
            line(Label::Result, "b", false),
            line(Label::Agent, "c", false),
            line(Label::Person, "hit", true),
            line(Label::Tool, "d", false),
            line(Label::Agent, "e", false),
            line(Label::Tool, "f", false),
        ];
        let kept: Vec<String> = nearest(lines.clone(), 2, 3)
            .into_iter()
            .map(|line| line.text)
            .collect();
        assert_eq!(kept, vec!["b", "c", "hit", "d", "e"]);
        let kept: Vec<String> = nearest(lines, 1, 1)
            .into_iter()
            .map(|line| line.text)
            .collect();
        assert_eq!(kept, vec!["c", "hit", "d"]);
    }

    #[test]
    fn picks_dated_decisions_for_the_reversal_check() {
        let words = "この凡例はいいね、古い地図は読むだけにして構いません";
        let lines = vec![line(Label::Person, "2026-09-27T04:20:00Z", words, true, 10)];
        let evidence = vec![compose(
            &corpus(),
            "E1".to_string(),
            hit(words, Who::Person, "2026-09-27T04:20:00Z", &["凡例"]),
            Ok(lines),
        )];
        let answer = value::parse(
            r#"{"findings":[{"evidence":"E1","kind":"praise","quote":"この凡例はいいね","note":""},
                            {"evidence":"E1","kind":"decision","quote":"古い地図は読むだけにして構いません","note":""},
                            {"evidence":"E9","kind":"decision","quote":"どこにもない","note":""}]}"#,
        )
        .unwrap();
        let claims = claims_to_check(&corpus(), &answer, &evidence);
        assert_eq!(
            claims.len(),
            1,
            "praise is not checked, and a finding without a record cannot be dated"
        );
        assert_eq!(claims[0].kind, "decision");
        assert_eq!(claims[0].date, "2026-09-27");
        assert_eq!(claims[0].source.byte_start, 10);
    }

    #[test]
    fn looks_for_changes_after_the_claim_not_earlier_the_same_day() {
        // 15:00 at the corpus offset, UTC+09:00.
        let claim = Claim {
            id: "D1".to_string(),
            kind: "decision".to_string(),
            quote: "React に変える".to_string(),
            date: "2026-10-18".to_string(),
            timestamp: "2026-10-18T06:00:00Z".to_string(),
            source: SourceRef {
                source_id: "claude-claim".to_string(),
                byte_start: 1,
                byte_len: 1,
            },
        };
        let morning = hit("Vue を使う", Who::Person, "2026-10-18T00:00:00Z", &["Vue"]);
        let evening = hit("Vue に戻す", Who::Person, "2026-10-18T07:00:00Z", &["Vue"]);
        let offset = hit(
            "Vue のまま",
            Who::Person,
            "2026-10-18T15:30:00+09:00",
            &["Vue"],
        );
        assert!(!written_after(&corpus(), &claim, "2026-10-18", &morning));
        assert!(written_after(&corpus(), &claim, "2026-10-18", &evening));
        assert!(written_after(&corpus(), &claim, "2026-10-18", &offset));
        assert!(
            !written_after(&corpus(), &claim, "2026-10-19", &evening),
            "the date the search starts from still bounds it"
        );
    }

    #[test]
    fn picks_the_best_records_across_rounds() {
        let pool = Pool {
            relevant: vec![
                (
                    hit(
                        "first round, 2 points",
                        Who::Person,
                        "2026-10-01T00:00:00Z",
                        &["a"],
                    ),
                    2,
                ),
                (
                    hit(
                        "first round, 3 points",
                        Who::Agent,
                        "2026-10-02T00:00:00Z",
                        &["a"],
                    ),
                    3,
                ),
                (
                    hit(
                        "second round, 3 points",
                        Who::Agent,
                        "2026-10-14T00:00:00Z",
                        &["b"],
                    ),
                    3,
                ),
                (
                    hit(
                        "second round, 2 points",
                        Who::Person,
                        "2026-10-14T01:00:00Z",
                        &["b"],
                    ),
                    2,
                ),
            ],
            ..Pool::default()
        };
        let best: Vec<String> = pool.best(3).into_iter().map(|hit| hit.text).collect();
        assert_eq!(
            best,
            vec![
                "first round, 3 points",
                "second round, 3 points",
                "first round, 2 points"
            ]
        );
    }

    #[test]
    fn keeps_order_in_parallel() {
        let doubled = parallel((0..50).collect::<Vec<u32>>(), |n| n * 2);
        assert_eq!(doubled, (0..50).map(|n| n * 2).collect::<Vec<u32>>());
        assert!(parallel(Vec::<u32>::new(), |n| n).is_empty());
    }

    #[test]
    fn reads_dates() {
        assert!(is_date("2026-10-14"));
        assert!(!is_date(""));
        assert!(!is_date("2026/10/14"));
    }

    #[test]
    fn verifies_quotes_against_the_records_not_what_the_model_was_shown() {
        // The model sees a header, the authors' labels, and marks of what was left out; none
        // of that is anyone's words.
        let reply = format!("{}色を元に戻しました", "あ".repeat(700));
        let lines = vec![
            line(Label::Agent, "2026-10-13T18:58:00Z", &reply, false, 100),
            line(
                Label::Person,
                "2026-10-13T19:01:10Z",
                "古い色のままにして",
                true,
                10,
            ),
        ];
        let evidence = vec![compose(
            &corpus(),
            "E1".to_string(),
            hit(
                "古い色のままにして",
                Who::Person,
                "2026-10-13T19:01:10Z",
                &["色"],
            ),
            Ok(lines),
        )];
        assert!(
            evidence[0].block.contains("more characters]"),
            "{}",
            evidence[0].block
        );
        let answer = value::parse(
            r#"{"findings":[{"evidence":"E1","kind":"other","quote":"more characters]","note":""},
                            {"evidence":"E1","kind":"other","quote":"E1 | 2026-10-14 04:01","note":""},
                            {"evidence":"E1","kind":"other","quote":"agent 2026-10-14 03:58","note":""},
                            {"evidence":"E1","kind":"decision","quote":"古い色のままにして","note":""},
                            {"evidence":"E1","kind":"attempt","quote":"色を元に戻しました","note":""}]}"#,
        )
        .unwrap();
        let list = findings(&corpus(), &answer, &evidence);
        let verified = |quote: &str| {
            list.iter()
                .find(|finding| finding.quote == quote)
                .expect("the finding")
                .verified
        };
        assert!(!verified("more characters]"), "a mark of what was cut");
        assert!(!verified("E1 | 2026-10-14 04:01"), "the header");
        assert!(!verified("agent 2026-10-14 03:58"), "a label and a time");
        assert!(verified("古い色のままにして"));
        assert!(
            verified("色を元に戻しました"),
            "words of the record past the cut are still its words"
        );
    }

    #[test]
    fn orders_by_the_instant_not_the_text_of_the_time() {
        // 09:00 at +09:00 is 00:00 UTC, an hour before 01:00Z, though its text sorts after it.
        let mut hits = vec![
            hit("early", Who::Person, "2026-10-16T09:00:00+09:00", &["a"]),
            hit("undated", Who::Person, "", &["a"]),
            hit("late", Who::Person, "2026-10-16T01:00:00Z", &["a"]),
        ];
        rank(&corpus(), &mut hits);
        let order: Vec<&str> = hits.iter().map(|hit| hit.text.as_str()).collect();
        assert_eq!(order, vec!["late", "early", "undated"]);

        let evidence: Vec<Evidence> = [
            ("E1", "late", "2026-10-16T01:00:00Z"),
            ("E2", "early", "2026-10-16T09:00:00+09:00"),
        ]
        .into_iter()
        .map(|(id, words, at)| {
            compose(
                &corpus(),
                id.to_string(),
                hit(words, Who::Person, at, &["a"]),
                Ok(vec![line(Label::Person, at, words, true, 10)]),
            )
        })
        .collect();
        let answer = value::parse(
            r#"{"findings":[{"evidence":"E1","kind":"decision","quote":"late","note":""},
                            {"evidence":"E2","kind":"decision","quote":"early","note":""}]}"#,
        )
        .unwrap();
        let order: Vec<String> = findings(&corpus(), &answer, &evidence)
            .into_iter()
            .map(|finding| finding.quote)
            .collect();
        assert_eq!(order, vec!["early", "late"]);
    }

    #[test]
    fn says_how_many_calls_have_no_known_price() {
        let usage = Usage {
            input: 1000,
            output: 100,
            ..Usage::default()
        };
        let spent = vec![
            Spend {
                step: "answer",
                model: "claude-opus-5-5".to_string(),
                usage,
                usd: Some(0.006),
            },
            Spend {
                step: "screen",
                model: "mystery-model".to_string(),
                usage,
                usd: None,
            },
        ];
        let json = cost_json(&spent);
        assert!(json.contains(r#""total":0.0060"#), "{json}");
        assert!(json.contains(r#""unpriced_calls":1"#), "{json}");
        assert!(cost_line(&spent).contains("not counted"));
    }

    #[test]
    fn reports_an_answer_as_text_and_as_json() {
        let corpus = corpus();
        let words = "地図の colors を古いままでと言った記憶がない";
        let lines = vec![line(Label::Person, "2026-10-04T07:21:00Z", words, true, 10)];
        let evidence = vec![compose(
            &corpus,
            "E1".to_string(),
            hit(words, Who::Person, "2026-10-04T07:21:00Z", &["colors"]),
            Ok(lines),
        )];
        let answer = value::parse(
            r#"{"answer":"10/4 に条件を外した。","findings":[
                  {"evidence":"E1","kind":"reversal","quote":"古いままでと言った記憶がない","note":"9/27 の条件を外す"},
                  {"evidence":"E1","kind":"other","quote":"ない言葉","note":""}],
                "timeline":["9/27 古いまま","10/4 外した"],"gaps":[],"more_terms":["colors"]}"#,
        )
        .unwrap();
        let pool = Pool {
            terms: vec!["colors".to_string()],
            counts: vec![TermCount {
                term: "colors".to_string(),
                who: Who::Person,
                returned: 60,
                total: 120,
                oldest_taken: Some("2026-09-01T00:00:00Z".to_string()),
            }],
            rounds: 2,
            found: (0..30)
                .map(|byte_start| SourceRef {
                    source_id: "claude-a".to_string(),
                    byte_start,
                    byte_len: 1,
                })
                .collect(),
            screened: true,
            threes: 3,
            twos: 4,
            synced_through: Some("2026-10-15T08:21:32+09:00".to_string()),
            ..Pool::default()
        };
        let notes = Notes::default();
        let filters = Filters::default();
        let summary = Summary::of(&pool, 1, &notes, &filters);
        let spent = vec![Spend {
            step: "answer",
            model: "claude-opus-5-5".to_string(),
            usage: Usage {
                input: 1000,
                output: 100,
                ..Usage::default()
            },
            usd: Some(0.006),
        }];
        let mut options = Options::thorough("地図の色の条件は？".to_string());
        options.text_format = true;
        let text = answer_report(
            &corpus,
            &options,
            Some(&answer),
            &evidence,
            &summary,
            &spent,
        );
        assert!(
            text.starts_with("Question: 地図の色の条件は？\n\nAnswer\n10/4 に条件を外した。"),
            "{text}"
        );
        assert!(
            text.contains("- reversal | 2026-10-04 16:21 | person | work/game"),
            "{text}"
        );
        assert!(
            text.contains("「ない言葉」 (not found in the records)"),
            "{text}"
        );
        assert!(
            text.contains("How it changed\n- 9/27 古いまま\n- 10/4 外した"),
            "{text}"
        );
        assert!(text.contains("--term \"colors\""), "{text}");
        assert!(
            text.contains(
                "Held by many records, of which only the newest were taken: colors (person 60/120, \
                 none before 2026-09-01 09:00)"
            ),
            "{text}"
        );
        assert!(
            text.contains("Candidates 30 -> relevant 7 (3 points 3, 2 points 4) -> records read 1"),
            "{text}"
        );
        assert!(
            text.contains("Cost: $0.006 (answer $0.006), 1100 tokens"),
            "{text}"
        );

        options.text_format = false;
        let json = answer_report(
            &corpus,
            &options,
            Some(&answer),
            &evidence,
            &summary,
            &spent,
        );
        assert!(json.starts_with(r#"{"disposition":"answered","mode":"ask","api":"anthropic","question":"地図の色の条件は？","not_settled":null,"answer":"10/4 に条件を外した。""#), "{json}");
        assert!(json.contains(r#""kind":"reversal","quote":"古いままでと言った記憶がない","quote_verified":true,"relevance":"9/27 の条件を外す","author":"person""#), "{json}");
        assert!(json.contains(r#""quote_verified":false"#), "{json}");
        assert!(json.contains(r#""cost_usd":{"total":0.0060,"answer":0.0060,"models":["claude-opus-5-5"],"tokens":1100,"unpriced_calls":0}"#), "{json}");
    }

    #[test]
    fn the_quick_setup_uses_one_model_and_no_reversal_check() {
        let quick = Options::quick("q".to_string());
        assert_eq!(quick.plan_model, "claude-haiku-5-5");
        assert_eq!(quick.answer_model, "claude-haiku-5-5");
        assert!(!quick.reversal_check && quick.second_round);
        let thorough = Options::thorough("q".to_string());
        assert_eq!(thorough.draft_model.as_deref(), Some("claude-haiku-5-5"));
        assert!(thorough.reversal_check && thorough.second_round);
    }

    #[test]
    fn the_spread_digest_gives_the_central_first_identical_messages_once_and_keeps_its_budget() {
        let message = |text: String, day: u32, minute: u32, score: u64| {
            let mut found = hit(
                &text,
                Who::Person,
                &format!("2026-09-{day:02}T00:{minute:02}:00Z"),
                &[],
            );
            found.session = format!("s{}", minute % 7);
            (found, score)
        };
        let mut pool = Pool {
            swept: Some(Swept::default()),
            ..Pool::default()
        };
        // Ten central messages and 150 background ones, each as long as a digest line shows,
        // and the same short reply sent twice.
        for minute in 0..10 {
            pool.relevant.push(message(
                format!("{minute}{}", "要".repeat(300)),
                20,
                minute,
                3,
            ));
        }
        for minute in 0..50 {
            for day in 1..4 {
                pool.relevant.push(message(
                    format!("{day}{minute}{}", "背".repeat(300)),
                    day,
                    minute,
                    2,
                ));
            }
        }
        let (digest, left_out) = sweep_digest_spread(&corpus(), &pool, &HashSet::new());
        // Each line costs its 240 characters shown and 60 of heading: 120 fit 36,000.
        assert_eq!(digest.len() + left_out, 160);
        assert!(
            digest.len() >= 120 && digest.len() <= 121,
            "{}",
            digest.len()
        );
        assert_eq!(
            digest
                .iter()
                .filter(|(found, _)| found.text.contains('要'))
                .count(),
            10,
            "every central message"
        );
        let times: Vec<&str> = digest
            .iter()
            .map(|(found, _)| found.timestamp.as_str())
            .collect();
        assert!(
            times.windows(2).all(|pair| pair[0] <= pair[1]),
            "oldest first"
        );
        // The same words sent twice are shown once, with how many times they were sent.
        let mut small = Pool {
            swept: Some(Swept::default()),
            ..Pool::default()
        };
        small.relevant = vec![
            message("OK".to_string(), 25, 1, 2),
            message("色は3色まで".to_string(), 25, 3, 3),
            message("OK".to_string(), 26, 2, 2),
        ];
        let (digest, left_out) = sweep_digest_spread(&corpus(), &small, &HashSet::new());
        assert_eq!((digest.len(), left_out), (2, 0));
        let (reply, copies) = digest
            .iter()
            .find(|(found, _)| found.text == "OK")
            .cloned()
            .expect("the short reply, once");
        assert_eq!(copies, 2);
        let block = compose_digest(
            &corpus(),
            "E9".to_string(),
            reply,
            DIGEST_LINE_CHARS,
            None,
            copies,
        );
        assert!(
            block.block.contains("sent 2 times in the same words")
                && block.block.contains("cut at 240 characters"),
            "{}",
            block.block
        );
    }

    #[test]
    fn a_long_message_is_screened_in_overlapping_pieces_that_each_fit_whole() {
        let long: String = "あ".repeat(1200);
        let messages = vec![
            hit("短い", Who::Person, "2026-09-01T00:00:00Z", &[]),
            hit(&long, Who::Person, "2026-09-02T00:00:00Z", &[]),
        ];
        let (pieces, owners, ranges) = pieces_of(&messages, true);
        assert_eq!(owners, vec![0, 1, 1, 1]);
        assert_eq!(
            ranges,
            vec![None, Some((0, 500)), Some((400, 900)), Some((800, 1200))]
        );
        assert!(pieces[1..]
            .iter()
            .all(|piece| piece.text.chars().count() <= SWEEP_CHARS));
        let (whole, owners, _) = pieces_of(&messages, false);
        assert_eq!((whole.len(), owners), (2, vec![0, 1]));
    }

    #[test]
    fn a_long_message_is_read_around_the_piece_found() {
        let text = format!("{}KEY{}", "a".repeat(1000), "b".repeat(1000));
        let shown = focused(&text, 900, 1100);
        assert!(shown.starts_with(&"a".repeat(300)) && shown.ends_with(&"b".repeat(300)));
        assert!(shown.contains("[the part found]") && shown.contains("KEY"));
    }

    #[test]
    fn within_a_score_the_reading_takes_one_session_at_a_time() {
        let in_session = |session: &str, score: u64, at: &str| {
            let mut found = hit("x", Who::Person, at, &[]);
            found.session = session.to_string();
            (found, score)
        };
        let scored = vec![
            in_session("a", 3, "2026-09-05T00:00:00Z"),
            in_session("a", 3, "2026-09-04T00:00:00Z"),
            in_session("a", 3, "2026-09-03T00:00:00Z"),
            in_session("b", 2, "2026-09-02T00:00:00Z"),
            in_session("c", 3, "2026-09-01T00:00:00Z"),
        ];
        assert_eq!(spread_order(&scored), vec![0, 4, 1, 2, 3]);
    }

    #[test]
    fn the_sweep_shows_the_end_of_the_reply_before_and_the_turns_last_reply() {
        let turn = records::TurnContext {
            before: format!("{}Leaflet をおすすめします。", "前置き".repeat(60)),
            replies: vec![
                hit("見てみます。", Who::Agent, "2026-09-01T00:01:00Z", &[]),
                hit(
                    "Leaflet で実装しました。",
                    Who::Agent,
                    "2026-09-01T00:02:00Z",
                    &[],
                ),
            ],
        };
        let shown = turn_context_text(&turn);
        assert!(shown.starts_with(" [agent before] …"), "{shown}");
        assert!(shown.contains("Leaflet をおすすめします。 [agent after] Leaflet で実装しました。"));
        assert!(!shown.contains("見てみます。"));
        assert_eq!(
            turn_context_text(&records::TurnContext::default()),
            String::new()
        );
    }

    #[test]
    fn rarity_and_months_choose_the_agents_records_to_screen() {
        let agent = |at: &str| hit("x", Who::Agent, at, &[]);
        let (a1, a2, a3, a4) = (
            agent("2026-10-04T00:00:00Z"),
            agent("2026-10-03T00:00:00Z"),
            agent("2026-10-02T00:00:00Z"),
            agent("2026-10-01T00:00:00Z"),
        );
        let august = agent("2026-08-04T00:00:00Z");
        let lists = vec![
            records::Found {
                hits: vec![a1.clone(), a2.clone(), a3.clone(), august.clone()],
                total: 4,
            },
            records::Found {
                hits: vec![a1.clone(), a4.clone(), august.clone()],
                total: 3,
            },
        ];
        let terms = vec!["MQTT".to_string(), "keepalive".to_string()];
        let mut options = Options::thorough("q".to_string());
        options.agent_per_term = 2;
        options.agent_select = AgentSelect::Idf;
        // As many as the newest two of each string give (three), one of them from August, and
        // in October the two holding the rarer strings.
        let chosen = select_agents(&corpus(), &lists, &terms, &options);
        let expected: HashSet<SourceRef> = [&a1, &a4, &august]
            .iter()
            .map(|hit| hit.source_ref.clone())
            .collect();
        assert_eq!(chosen, expected);
        options.agent_select = AgentSelect::All;
        assert_eq!(select_agents(&corpus(), &lists, &terms, &options).len(), 5);
    }
}
