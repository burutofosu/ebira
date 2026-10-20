# Evaluating `ebira ask`

Fifty-four questions about made-up conversations, whose answers are known. Forty-two test what
is hard about reading a person's history with coding agents: six ways with five questions each
(one of the five in English), and four more with three each. Twelve more strain one step of
`ask` each, on logs that `stress.py` generates:

| Type | What the answer has to do |
| --- | --- |
| `lost_in_summary` | Bring back a condition the person gave that a compaction summary or the agent's recap left out, after which the work went against it |
| `example_not_spec` | Keep an example, a guess, or a placeholder the person offered apart from what they decided |
| `reversed_in_other_words` | Give the later state of a decision reversed in words that do not repeat it |
| `latest_the_same_day` | Give a decision changed later the same day, and not take the earlier message for a change of it |
| `agent_not_person` | Say who decided, reported, or claimed something: the agent, or the person |
| `no_record` | Say that the records do not settle the question, and not make an answer up |
| `repeated_principle` | Give a rule the person stated several times in different words, and count or date the repetitions; an agent's restatement is not one |
| `attempt_history` | Give each thing tried for a problem with how the person said it turned out, and the one that holds now |
| `origin_of_rule` | Say that a rule the records carry as the person's was written by an agent or a summary, and what the person actually said |
| `other_words` | Find the message that settles the question though it shares no noun with it |
| `short_reply` | Find a short agreement ("go with that") to an agent's proposal, and say what was agreed to; the agreement means nothing without the agent's words around it, and newer records bury the proposal |
| `long_message` | Read what settles the question in the middle of a long spec the person pasted |
| `repeated_principle` (M1) | The same, among some 660 design remarks of the person's, more than the digest holds |
| `span_of_repeats` | Give the first and the last time a rule was said, among the same remarks |
| `agent_found_cause` | Find the cause an agent found months ago, under fifty and more newer agent records holding the same words, and that the person confirmed the fix |

The conversations are in eight projects, as Claude Code and Codex would log them, with
compaction summaries, tool calls and their output, and a Codex message wrapped with the
files it mentions. Generated filler sessions in the same projects, the same every time,
settle nothing the questions ask about; they give the search something to sift through.
Each question is asked with `--cwd` naming its project, as an agent working there would
ask: the person's messages, as in real logs, do not name the project they are in. The stress
cases are in three more projects: `tabi-app` (short agreements, and a pasted spec),
`atelier` (hundreds of design remarks, three rules restated), and `sensor-hub` (Codex logs
of causes found long ago).

## What it does not test

- The logs are small: about 380 sessions and 5,000 records, a little over three megabytes. Real
  histories run to gigabytes, where choosing the search strings matters more, and the
  person's messages in a project run to a thousand or more, where screening them all
  matters more.
- The person's lines were written for this, by a model. Real messages are terser and more
  ambiguous, and they run into failures nobody thought to write a case for.
- One run per question. The search strings a model chooses vary from run to run, and so
  do the answers.

A change that scores better here should still be tried on a real history.

## Running

```sh
cargo build --release
python3 evals/run.py --out /tmp/ebira-eval --ebira target/release/ebira
```

This writes the logs and a corpus under `/tmp/ebira-eval`, asks the fifty-four questions
(three at a time; `--jobs` changes that), and writes `report.md`, with the cost, time,
verified quotes, and whether each answer cites the records the case names, and `judge.md`,
the answers for a judge to score as [JUDGE.md](JUDGE.md) says. `--quick` asks with
`--quick`, `--cases A1,D3` asks some cases, `--max-usd` stops starting cases past a sum, and
`--build-only` writes the logs and the corpus only, and `--ask-arg=<option>` (repeatable)
passes an option to every `ask`, such as `--ask-arg=--no-sweep`.

To compare ways of finding and handing over the records without paying for answers,
`--evidence` stops each `ask` before its answer (`--evidence-only`) and writes `funnel.md`:
for every record a case should cite, how far it got (read with its context, shown in the
digest, judged relevant but not handed over, screened and judged not relevant, found and not
screened, or missed), and whether the words that settle a long-message case reached the
reader. `--terms-from <dir>` makes each case search the strings that an earlier run in `<dir>`
searched in its first round, with that run's period, so that runs differ only in their
options; each run's own draft still chooses its second round. A funnel run costs about two
US cents a question.

`ebira ask` uses the API it is set up for (see `ebira help ask-setup`) and spends under that
key. On logs this small a question costs less than on a real history: with the default
models, about 5 US cents, so about $2.70 for the fifty-four; with `--quick`, under a cent.
Python 3.8 or later is needed, with the standard library only.

## Files

- `scenarios/*.txt`: the conversations, one project a file.
- `cases.json`: the questions, the key points a right answer makes, the mistakes to watch
  for, and the records it should cite.
- `stress.py`: generates the logs of the stress cases, the same every time.
- `run.py`: writes the logs, builds the corpus, asks, and reports.
- `JUDGE.md`: how to score the answers.
- `results/`: the scores of earlier runs.

A scenario file reads:

```text
project shiori /work/shiori

session claude 2026-09-15 auth
09:00 you c2_link: パスワード管理したくないから、メールにリンク送る方式にしよ
09:02 agent: マジックリンク方式に切り替えます。
09:20 tool Edit: app/auth/login.ts
  => The file app/auth/login.ts has been updated successfully.
```

`session` takes the app (`claude` or `codex`), the date, and a name. A record is a time
(UTC+09:00), a kind (`you`, `agent`, `tool <name>`, `summary`, `interrupt`, or `wrapped`,
the person's message in Codex's wrapper), an optional key that `cases.json` cites, and the
text; lines starting with two spaces continue it, and `  => ` lines are a tool's output.
