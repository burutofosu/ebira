//! `ebira ask`: a question about past work, answered from the records with quotes.
//!
//! Claude models choose literal search strings, screen what Ebira finds, and read the records
//! around the best of it; a draft answer feeds a second round of searching and a check, for each
//! decision it reports, of the person's later messages that may have changed it. Every quote in
//! the answer is checked against the records it was given before it is shown.
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
use records::{Corpus, Hit, Label, Line, SourceRef, Who};
use std::collections::{HashMap, HashSet};
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
    /// reversal check.
    pub fn thorough(question: String) -> Options {
        Options {
            question,
            filters: Filters::default(),
            terms: Vec::new(),
            no_api: false,
            text_format: false,
            keep: 40,
            person_per_term: 60,
            agent_per_term: 40,
            candidates: 600,
            second_round: true,
            reversal_check: true,
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

const MAX_TERMS: usize = 16;
const SECOND_ROUND_TERMS: usize = 8;
/// The kinds of finding whose later change matters, in the order they are checked.
const CHECKED_KINDS: [&str; 4] = ["decision", "principle", "reversal", "cause"];
const MAX_CLAIMS: usize = 6;
const REVERSAL_TERMS: usize = 4;
const REVERSAL_PER_TERM: usize = 40;
const MAX_LATER: usize = 12;
const SCREEN_CHUNK: usize = 40;
const WORKERS: usize = 4;
const RECENT_SESSIONS: usize = 4;
const RECENT_MESSAGES: usize = 8;

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
    threes: usize,
    twos: usize,
    screened: bool,
    rounds: usize,
}

impl Pool {
    /// The best `keep` records: the higher score first, then in the order found.
    fn best(&self, keep: usize) -> Vec<Hit> {
        let mut order: Vec<usize> = (0..self.relevant.len()).collect();
        order.sort_by_key(|&index| (std::cmp::Reverse(self.relevant[index].1), index));
        order
            .into_iter()
            .take(keep)
            .map(|index| self.relevant[index].0.clone())
            .collect()
    }
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
}

struct ReversalCheck {
    id: String,
    date: String,
    kind: String,
    quote: String,
    terms: Vec<String>,
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
    let terms = if options.terms.is_empty() {
        let caller = caller
            .as_ref()
            .ok_or("without the API, name the search strings with --term")?;
        progress(&format!("choosing search strings ({})", options.plan_model));
        let user = format!(
            "Today: {} ({})\nQuestion: {}",
            corpus.today(),
            corpus.zone(),
            options.question
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

    // 2-3. Search and screen.
    let mut pool = Pool::default();
    gather(
        corpus,
        caller.as_ref(),
        options,
        &filters,
        &terms,
        &mut pool,
        &mut notes,
    )?;

    // 4. Read the records around the best ones.
    let mut evidence = read_evidence(
        corpus,
        with_recent(corpus, &filters, pool.best(options.keep), &mut notes),
    );

    // 5. Answer.
    let Some(caller) = caller else {
        return Ok(records_report(
            corpus,
            options,
            &evidence,
            &Summary::of(&pool, evidence.len(), &notes),
        ));
    };
    if evidence.is_empty() {
        let spent = caller.spent.lock().unwrap();
        return Ok(answer_report(
            corpus,
            options,
            None,
            &evidence,
            &Summary::of(&pool, 0, &notes),
            &spent,
        ));
    }
    let draft_model = options
        .draft_model
        .as_deref()
        .filter(|_| options.second_round || options.reversal_check)
        .unwrap_or(&options.answer_model);
    let mut answer = answer_with(&caller, options, corpus, &evidence, draft_model)?;
    let mut records_changed = false;

    // 6. A second round: search the strings the answer suggests.
    if options.second_round {
        let more: Vec<String> = clean_terms(strings(&answer, "more_terms").into_iter())
            .into_iter()
            .filter(|term| !pool.terms.contains(term))
            .take(SECOND_ROUND_TERMS)
            .collect();
        let before = pool.relevant.len();
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
        records_changed = pool.relevant.len() > before;
    }

    // 7. For what the answer says was decided, look after its date for messages that change it.
    let mut later = Vec::new();
    if options.reversal_check {
        let claims = claims_to_check(corpus, &answer, &evidence);
        later = hunt_reversals(corpus, &caller, options, &filters, &claims, &mut notes);
        records_changed |= !later.is_empty();
    }

    // 8. The final answer, from the answer model, over the records as they now stand.
    if records_changed {
        let mut hits = pool.best(options.keep);
        for found in later {
            match hits
                .iter_mut()
                .find(|hit| hit.source_ref == found.source_ref)
            {
                Some(hit) => hit.note = found.note,
                None => hits.push(found),
            }
        }
        evidence = read_evidence(corpus, with_recent(corpus, &filters, hits, &mut notes));
        answer = answer_with(&caller, options, corpus, &evidence, &options.answer_model)?;
    } else if draft_model != options.answer_model {
        // The first answer was only a draft from another model.
        answer = answer_with(&caller, options, corpus, &evidence, &options.answer_model)?;
    }
    let spent = caller.spent.lock().unwrap();
    Ok(answer_report(
        corpus,
        options,
        Some(&answer),
        &evidence,
        &Summary::of(&pool, evidence.len(), &notes),
        &spent,
    ))
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

    // 1. What a later message about each claim would contain: chosen by the plan model, like
    // every other search string (a weaker model rarely thinks of the alternative a change comes
    // as: another framework, another database).
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
    let chosen: Vec<Vec<String>> = claims
        .iter()
        .map(|claim| {
            reply
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
                .collect()
        })
        .collect();

    // 2. The person's messages from each claim's date on: one pass for every string of every
    // claim, from the earliest date. A claim takes the first messages after it, where a change
    // usually follows, and the newest, where the current state is.
    let mut all_terms: Vec<String> = Vec::new();
    for term in chosen.iter().flatten() {
        if !all_terms.contains(term) {
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
    for (claim, strings) in claims.iter().zip(&chosen) {
        let from = start(claim);
        let mut list: Vec<Hit> = Vec::new();
        for term in strings {
            let Some(index) = all_terms.iter().position(|known| known == term) else {
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
        // Keep the earliest and the latest when there are more than one screen takes.
        list.sort_by(|left, right| time_order(corpus, left, right));
        if list.len() > SCREEN_CHUNK {
            let latest = list.split_off(list.len() - SCREEN_CHUNK / 2);
            list.truncate(SCREEN_CHUNK - latest.len());
            list.extend(latest);
        }
        candidates.push((claim, list));
    }
    let screened: HashMap<String, usize> = candidates
        .iter()
        .map(|(claim, list)| (claim.id.clone(), list.len()))
        .collect();
    candidates.retain(|(_, list)| !list.is_empty());

    // 3. Does each later message change the claim?
    let schema = prompts::screen_schema();
    let judged = parallel(candidates, |(claim, list)| {
        let lines: Vec<String> = list
            .iter()
            .enumerate()
            .map(|(index, hit)| {
                format!(
                    "C{} | {} | {} | {}",
                    index + 1,
                    corpus.local(&hit.timestamp),
                    if hit.project.is_empty() {
                        "?"
                    } else {
                        &hit.project
                    },
                    text::one_line(&text::window(&hit.text, &hit.terms, 360))
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
            system: prompts::REVERSAL_SCREEN_SYSTEM,
            user: &user,
            schema: &schema,
        };
        let result = caller.call("reversal-screen", &request);
        (claim, list, result)
    });
    // The messages that may change each claim, the likeliest first, claim by claim.
    let mut per_claim: Vec<Vec<(Hit, u64)>> = Vec::new();
    let mut changes: HashMap<String, usize> = HashMap::new();
    for (claim, list, result) in judged {
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                notes.issues.push(format!(
                    "reversal check: the messages after {} were not judged ({error})",
                    claim.id
                ));
                continue;
            }
        };
        let mut found: Vec<(Hit, u64)> = Vec::new();
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

/// One round: search `terms`, drop records already considered, rank, and screen (with the
/// API) into the pool.
fn gather(
    corpus: &Corpus,
    caller: Option<&Caller>,
    options: &Options,
    filters: &Filters,
    terms: &[String],
    pool: &mut Pool,
    notes: &mut Notes,
) -> Result<(), String> {
    pool.rounds += 1;
    progress(&format!("searching for {} strings", terms.len()));
    let searched = search(corpus, terms, filters, options)?;
    pool.terms.extend(terms.iter().cloned());
    pool.counts.extend(searched.counts);
    if pool.synced_through.is_none() {
        pool.synced_through = searched.synced_through;
    }
    let candidates = take_new(pool, corpus, searched.hits, options.candidates);
    match caller {
        Some(caller) if !candidates.is_empty() => {
            progress(&format!(
                "screening {} candidates ({})",
                candidates.len(),
                options.screen_model
            ));
            let scores = screen(caller, options, corpus, &candidates, notes);
            pool.screened = true;
            for (hit, score) in candidates.into_iter().zip(scores) {
                match score {
                    Some(3) => pool.threes += 1,
                    Some(2) => pool.twos += 1,
                    _ => continue,
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

/// This round's records not screened yet, best first, up to `limit`, which are marked as
/// screened. One past the limit is not: a later round's strings can bring it back.
fn take_new(pool: &mut Pool, corpus: &Corpus, hits: Vec<Hit>, limit: usize) -> Vec<Hit> {
    let mut this_round = HashSet::new();
    let mut candidates: Vec<Hit> = hits
        .into_iter()
        .filter(|hit| {
            !pool.seen.contains(&hit.source_ref) && this_round.insert(hit.source_ref.clone())
        })
        .collect();
    rank(corpus, &mut candidates);
    pool.found
        .extend(candidates.iter().map(|hit| hit.source_ref.clone()));
    candidates.truncate(limit);
    pool.seen
        .extend(candidates.iter().map(|hit| hit.source_ref.clone()));
    candidates
}

/// The records, oldest first, each with the nearest readable records around it.
fn read_evidence(corpus: &Corpus, mut hits: Vec<Hit>) -> Vec<Evidence> {
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
        .map(|(index, (hit, lines))| compose(corpus, format!("E{}", index + 1), hit, lines))
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
    model: &str,
) -> Result<Value, String> {
    progress(&format!(
        "answering from {} records ({model})",
        evidence.len()
    ));
    let blocks: Vec<&str> = evidence.iter().map(|item| item.block.as_str()).collect();
    let user = format!(
        "Question: {}\nToday: {}\nTimes: every time below is local time, {}\n\nRecords ({} of them, \
         by E number; ▶ marks the record the search found, the lines around it are its context):\n\n{}",
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
    let (person, agents) = thread::scope(|scope| {
        let agents = (options.agent_per_term > 0)
            .then(|| scope.spawn(|| corpus.agents(terms, filters, options.agent_per_term)));
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
            searched.counts.push(TermCount {
                term: term.clone(),
                who,
                returned: found.hits.len(),
                total: found.total,
            });
            for hit in &found.hits {
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
                    "C{} | {} | {} | {} | {}",
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
                    text::one_line(&text::window(&hit.text, &hit.terms, 360))
                )
            })
            .collect();
        let user = format!(
            "Question: {}\n\nCandidates:\n{}",
            options.question,
            lines.join("\n")
        );
        let result = caller.call(
            "screen",
            &Request {
                model: &options.screen_model,
                effort: "low",
                max_tokens: 6000,
                system: prompts::SCREEN_SYSTEM,
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
            Err(error) => {
                // Keep the best-ranked third of the chunk rather than lose it, and say so.
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

/// The block the answer model reads for one record: a header, then the record (▶) among its
/// neighbours.
fn compose(corpus: &Corpus, id: String, hit: Hit, lines: Result<Vec<Line>, String>) -> Evidence {
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
        let body = match hit.who {
            Who::Person => text::clip_middle(full, 1800),
            Who::Agent => text::clip_middle(full, 1500),
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
    }
}

struct Summary<'a> {
    terms: &'a [String],
    counts: &'a [TermCount],
    found: usize,
    screened: Option<(usize, usize)>,
    kept: usize,
    rounds: usize,
    synced_through: Option<&'a str>,
    notes: &'a Notes,
}

impl<'a> Summary<'a> {
    fn of(pool: &'a Pool, kept: usize, notes: &'a Notes) -> Summary<'a> {
        Summary {
            terms: &pool.terms,
            counts: &pool.counts,
            found: pool.found.len(),
            screened: pool.screened.then_some((pool.threes, pool.twos)),
            kept,
            rounds: pool.rounds,
            synced_through: pool.synced_through.as_deref(),
            notes,
        }
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
                    "{} ({} {}/{})",
                    count.term,
                    if count.who == Who::Person {
                        "person"
                    } else {
                        "agent"
                    },
                    count.returned,
                    count.total
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
        lines.push(format!(
            "The corpus has read the logs through {} (run `ebira sync` first to include the latest)",
            self.synced_through
                .map(|time| format!("{} {}", corpus.local(time), corpus.zone()))
                .unwrap_or_else(|| "an unknown time".to_string())
        ));
        for check in &self.notes.checks {
            lines.push(format!(
                "Reversal check {} ({}, {} 「{}」): strings {} -> {} candidates -> {} may change it",
                check.id,
                check.date,
                check.kind,
                text::clip_head(&text::one_line(&check.quote), 30),
                check.terms.join(", "),
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
                    Some(count) => object.raw(
                        key,
                        &crate::json::Object::new()
                            .number("returned", count.returned as u64)
                            .number("total", count.total)
                            .finish(),
                    ),
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
                if answer.is_some() {
                    "answered"
                } else {
                    "no_records"
                },
            )
            .name("mode", "ask")
            .name("api", &options.endpoint.name)
            .text("question", &options.question)
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
    match answer {
        None => out.push_str(
            "No record holds the search strings. Try other words, or name them with --term.\n\n",
        ),
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
    fn cleans_terms() {
        let terms = clean_terms(
            ["  色が ", "MapLegend", "色が", "", "凡例"]
                .into_iter()
                .map(String::from),
        );
        assert_eq!(terms, vec!["色が", "MapLegend", "凡例"]);
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
        let summary = Summary::of(&pool, 1, &notes);
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
                "Held by many records, of which only the newest were taken: colors (person 60/120)"
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
        assert!(json.starts_with(r#"{"disposition":"answered","mode":"ask","api":"anthropic","question":"地図の色の条件は？","answer":"10/4 に条件を外した。""#), "{json}");
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
}
