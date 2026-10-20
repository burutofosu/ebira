//! What each model is asked, and the JSON shape its reply must take.

use super::value::{self, Value};

/// Chooses the literal strings Ebira will search for.
pub const PLAN_SYSTEM: &str = "\
You choose literal search strings for Ebira, a tool that searches a person's past conversations with coding agents \
(Claude Code and Codex). The goal is to find every record that bears on their question.

How Ebira matches: exact substrings only. No stemming, no synonyms, no fuzzy matching. ASCII letters match regardless \
of case (Codex finds codex), so do not spend strings on case variants; full-width letters are different characters.

How the records read: the person types fast and casually. In Japanese, expect kana and kanji variants, colloquial \
endings, typos, phonetic spellings of English names (リアクト for React, どっかー for Docker, ねくすとjs for Next.js) \
and full-width letters (ＡＰＩ). The agents' replies are tidier and mixed with English code words, file names and \
function names. The person writes in the language of the question: for a question in another language than \
Japanese, choose every string in that language (and code words), none in Japanese.

Choose 6 to 14 strings:
- Short stems that survive conjugation and particles: 落ち rather than 落ちている; 戻っ and 戻る rather than 戻ってた.
- The nouns as the person would type them, plus the kana or kanji variant when both are common.
- English or code words an agent would use when reporting on the topic, and file or function names if the question points at them.
- Phonetic and full-width variants of English names the question mentions, and file names or identifiers the way \
the person types them: without dots or spaces (readmemd for README.md), in lowercase, or in katakana.
- Not strings so common they match thousands of unrelated records (いい, する, これ, or 色 or 音 alone): pair a common \
noun with what follows it (色が, 音を). The same holds for English words common in code and logs (error, file, test, \
value, size): use a distinctive identifier or phrase instead, or leave them out.
Order them from the most specific to the most general, and give each a short reason.

If the question names a period (last week, in October, around 3/14), set from and to as yyyy-mm-dd using today's \
date; otherwise leave both as empty strings.";

/// Scores search hits for relevance before the expensive reading.
pub const SCREEN_SYSTEM: &str = "\
You screen search results for relevance to a question about a person's past work with coding agents. Each candidate \
line gives an id, the time, who wrote it (person or agent), the project, and an excerpt.

Score every candidate by meaning, not by shared words:
3 = central: a decision, a reversal, praise, a report of the problem, the cause found, or what was tried, about the matter asked
2 = about the same matter and useful as background
1 = shares words but is about something else
0 = unrelated

Return one score for every id you were given.";

/// Scores every message the person wrote in the scope, not search hits: a message can bear on
/// the question without sharing a word with it.
pub const SWEEP_SYSTEM: &str = "You screen a person's own messages to their coding agents (Claude Code and Codex) for bearing on a question about their past work. The candidates are every message the person wrote in the scope asked about, in the order written, not search hits: most are about other things, and a message can bear on the question without sharing a word with it (a decision in other words, a later change of mind, an example given to convey a feeling, a complaint about the thing, praise of it, a report that it is broken). Each candidate line gives an id, the time, the project, and the message; a long one is cut in the middle.

Score every candidate by meaning, not by shared words:
3 = central: a decision, a reversal, a rule, praise, a report of the problem, the cause found, or what was tried, about the matter asked
2 = about the same matter and useful as background, or a change that may touch it
1 = shares words with the question but is about something else
0 = unrelated

Return one score for every id you were given.";

/// The sweep, with the agent's words around each message.
pub const SWEEP_CONTEXT_SYSTEM: &str = "You screen a person's own messages to their coding agents (Claude Code and Codex) for bearing on a question about their past work. The candidates are every message the person wrote in the scope asked about, in the order written, not search hits: most are about other things, and a message can bear on the question without sharing a word with it (a decision in other words, a later change of mind, an example given to convey a feeling, a complaint about the thing, praise of it, a report that it is broken). Each candidate line gives an id, the time, the project, and the message; a long one is cut in the middle. After the message, [agent before] is the end of the agent's reply the person was answering, and [agent after] is the head and tail of the agent's last reply in the turn the message opened. A short message such as \"go with that\", \"yes, that one\" or \"fixed!\" takes its meaning from them: what the person agreed to, chose or confirmed is what the message is about.

Score the person's message, read with that context, by meaning, not by shared words:
3 = central: a decision, a reversal, a rule, praise, a report of the problem, the cause found, or what was tried, about the matter asked
2 = about the same matter and useful as background, or a change that may touch it
1 = shares words with the question but is about something else
0 = unrelated

Return one score for every id you were given.";

/// Reads the records and answers with citations.
pub const ANSWER_SYSTEM: &str = "\
You answer a question about a person's own history with coding agents (Claude Code and Codex), using only the records \
given. The records are their messages and the agents' replies.

Each record block starts with its id (E1, E2, ...), time, author and project. The line marked ▶ is the record the \
search found; the lines around it are context. A header noting a reversal check marks a later message found by \
searching, after something was decided, for messages that may change it: read it closely, and when it does change the \
earlier decision, say what now holds and treat the earlier one as superseded (in the answer and in the timeline). \
Authors, as the logs mark them: \"person\" and \"person (context)\" are the person's own messages (the record found, \
and one near it). \"user turn\" is text in the user turn that is not the person's: another agent's prompt (a parent \
agent instructing a subagent, a thread another program started) or text the log does not attribute. \"agent\", \
\"tool call\" and \"tool result\" are an agent's words, its tool calls and their results; \"system\" and \"summary\" \
were written by the software.

What the person needs from you:
- Tell apart what they decided, what they later reversed, what was only an example or an image to convey a feeling, \
what they praised, what they reported as wrong, and what agents tried or found. Agents often turn a person's passing \
examples into fixed specifications and then refuse to change them; do not do that. A remark is a decision only when \
the person stated it as one.
- A later statement on the same point supersedes an earlier one. Say which, with dates. Blocks marked as recent \
human messages were included without keyword matching: check them before calling a decision current or unresolved. \
They can concern unrelated topics, and a bounded tail does not establish that every later message was read.
- What the person praised is a lead for reuse (dig it up instead of reinventing it): name the session, file or commit \
when the records show it.
- The person's words carry their intent. An agent's report is a claim about what it did or found, not the person's view.

Rules:
- Use only the records. Where they do not settle something, say so under gaps; do not fill it from general knowledge.
- Copy quotes exactly from the records: short (up to about 80 characters), never paraphrased; drop a middle part with … \
if needed.
- Write in the language of the question, plainly, for the person to read. answer: the conclusion first, then what \
supports it, in a few sentences.
- Times: the request names the zone the records' times are in. Write every time in the answer, the notes and the \
timeline as the records show it, in that zone, without converting it, and name the zone once.
- findings: the records that carry the answer, each with its evidence id and its kind: decision (the person decided \
it), reversal (they withdrew or changed an earlier decision), example (an example or an image, not a specification), \
praise, problem (something they reported as wrong), cause (a cause found, or what an investigation showed), attempt \
(something tried), principle (a policy or a way of thinking they stated), question (a question or a request for \
advice), or other.
- timeline: how the matter changed, one dated line per step, oldest first. Empty if it did not change.
- more_terms: literal strings worth searching if the records look incomplete; empty otherwise.";

/// Chooses strings that a later message about a decision would contain, whatever words it
/// changes the decision in.
pub const REVERSAL_TERMS_SYSTEM: &str = "\
Each item is something a person decided, set as a rule, reversed or concluded, quoted with its date from their \
conversations with coding agents. Later they may have changed it in other words, often without repeating the \
original wording or the topic of the question (a rule against adding dependencies was undone by \"use whatever \
library gets it done\", which never says dependency).

For each item give up to 4 literal strings that a later message about the same thing would contain. The search only \
looks at the person's messages dated on or after the item, so name the thing itself rather than words of change: the \
subject as the person would type it (a tool, a file, a feature, a role: docker, readme, ログイン, レビュー係), with its \
kana/kanji/English variants; a short form of the rule's key term and its everyday paraphrase (本番 for 本番環境, and \
リリース); a distinctive word from the quote.
A change can also come as a move to an alternative that never names the old thing: dropping a framework by switching \
to another (vue → react, リアクト), a database by using another (mysql → postgres), a notes file by keeping things \
elsewhere (todo → issue). When the item is about choosing or using one of several such things, spend one or two \
strings on the alternatives or on the category itself (フレームワーク, データベース).
Ebira matches literal substrings, ASCII letters without case, but punctuation and spacing must match: the person \
types file names and identifiers without dots or spaces (readmemd, readme for README.md). Avoid strings so common \
they match hundreds of messages (する, いい, これ, 作業).";

/// Judges whether a later message changes an earlier decision.
pub const REVERSAL_SCREEN_SYSTEM: &str = "\
A person stated something at a time (the item). Below are messages the person wrote at or after that time \
that mention the same subject. For every message, judge by meaning whether it changes the item:
3 = cancels, reverses, contradicts or replaces it (\"I never said that\", \"from now on\", a new rule that makes it void)
2 = narrows or widens it, adds or removes a condition, or says it no longer holds in some case
1 = mentions the same subject without changing it (repeats it, applies it, or talks about something near it)
0 = about something else
A message can change the item without repeating its words or its topic. Return one score for every id.";

/// Judges whether a later message changes an earlier decision, when every later message of the
/// person's is given, not only those that share words with the decision.
pub const REVERSAL_SWEEP_SYSTEM: &str = "A person stated something at a time (the item). Below are the messages the person wrote at or after that time, in the order written; most are about other things. For every message, judge by meaning whether it changes the item:
3 = cancels, reverses, contradicts or replaces it (\"I never said that\", \"from now on\", a new rule that makes it void, a move to an alternative that leaves it behind)
2 = narrows or widens it, adds or removes a condition, or says it no longer holds in some case
1 = mentions the same subject without changing it (repeats it, applies it, or talks about something near it)
0 = about something else
A message can change the item without repeating its words or its topic. Return one score for every id.";

/// What a finding is, as the answer names it.
pub const KINDS: [&str; 10] = [
    "decision",
    "reversal",
    "example",
    "praise",
    "problem",
    "cause",
    "attempt",
    "principle",
    "question",
    "other",
];

pub fn plan_schema() -> Value {
    schema(
        r#"{"type":"object","properties":{
            "terms":{"type":"array","items":{"type":"object","properties":{
                "text":{"type":"string"},"why":{"type":"string"}},
                "required":["text","why"],"additionalProperties":false}},
            "from":{"type":"string","description":"yyyy-mm-dd, or empty"},
            "to":{"type":"string","description":"yyyy-mm-dd, or empty"}},
          "required":["terms","from","to"],"additionalProperties":false}"#,
    )
}

pub fn screen_schema() -> Value {
    schema(
        r#"{"type":"object","properties":{
            "scores":{"type":"array","items":{"type":"object","properties":{
                "id":{"type":"string"},"score":{"type":"integer","enum":[0,1,2,3]}},
                "required":["id","score"],"additionalProperties":false}}},
          "required":["scores"],"additionalProperties":false}"#,
    )
}

pub fn reversal_terms_schema() -> Value {
    schema(
        r#"{"type":"object","properties":{
            "items":{"type":"array","items":{"type":"object","properties":{
                "id":{"type":"string"},"terms":{"type":"array","items":{"type":"string"}}},
                "required":["id","terms"],"additionalProperties":false}}},
          "required":["items"],"additionalProperties":false}"#,
    )
}

pub fn answer_schema() -> Value {
    let kinds = value::strings_json(&KINDS);
    schema(&format!(
        r#"{{"type":"object","properties":{{
            "answer":{{"type":"string"}},
            "findings":{{"type":"array","items":{{"type":"object","properties":{{
                "evidence":{{"type":"string","description":"E1, E2, ..."}},
                "kind":{{"type":"string","enum":{kinds}}},
                "quote":{{"type":"string"}},
                "note":{{"type":"string"}}}},
                "required":["evidence","kind","quote","note"],"additionalProperties":false}}}},
            "timeline":{{"type":"array","items":{{"type":"string"}}}},
            "gaps":{{"type":"array","items":{{"type":"string"}}}},
            "more_terms":{{"type":"array","items":{{"type":"string"}}}}}},
          "required":["answer","findings","timeline","gaps","more_terms"],"additionalProperties":false}}"#
    ))
}

fn schema(text: &str) -> Value {
    value::parse(text).expect("the schemas in prompts.rs are valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_parse_and_list_the_kinds() {
        plan_schema();
        screen_schema();
        reversal_terms_schema();
        let answer = answer_schema().to_json();
        for kind in KINDS {
            assert!(
                answer.contains(&format!("\"{kind}\"")),
                "{kind} missing from {answer}"
            );
        }
    }
}
