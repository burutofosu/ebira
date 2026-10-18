# Judging the answers

`run.py` writes `judge.md`: for each case, the question, the key points a right answer
makes, the mistakes to watch for, and the answer `ebira ask` gave (its text, its findings
with their quotes, and what it says the records do not settle). When none of the records
found was judged relevant, the answer starts with what `ask` itself says about the search:
the strings searched and how many records they found; it is part of the answer. Judge
each answer on its own, by what it says, without knowing which version of Ebira or which
models wrote it. Any capable model or person can judge; give it this file and `judge.md`.

For every key point, in order:

- 1 when the answer states it, in any words;
- 0.5 when it states part of it, or states it so hedged that a reader cannot tell;
- 0 when it is missing or contradicted.

For every mistake, in order: `true` when the answer makes it, in its text or in a finding
it presents as the current state; `false` otherwise. A mistake the answer reports as a
former state ("first X, later Y") is not made. Saying who said something counts: a decision
the answer gives to the person that the case says the agent made is a mistake when the case
lists it.

An answer shown as "(no answer: …)" makes no key point and no mistake, with one exception:
when a case's only key point is that the records hold nothing, "(no answer: ask found no
records to answer from)" earns 0.5 for it. It says nothing wrong, but it does not say that
nothing was found, and the person is left to guess why.

Write `judgments.json` beside `judge.md`:

```json
{
  "A1": {"points": [1, 1, 0.5], "mistakes": [false, false], "note": "one line on anything doubtful"},
  "A2": {"points": [1, 1, 0], "mistakes": [true, false], "note": ""}
}
```

with as many points and mistakes as the case lists. Then
`python3 evals/run.py --out DIR --score` writes `score.md`.
