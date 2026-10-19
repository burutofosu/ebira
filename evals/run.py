#!/usr/bin/env python3
"""An evaluation of `ebira ask` on made-up logs with known answers.

The conversations in evals/scenarios/ hold the decisions; evals/cases.json asks about them,
with the points a right answer makes, the mistakes to watch for, and the records it should
cite. Filler sessions in the same projects, generated the same way every time, give the
search something to sift through.

    python3 evals/run.py --out DIR [--ebira PATH] [--quick] [--cases A1,B2] [--jobs 3]
        writes the logs and a corpus under DIR, asks every case, and writes report.md
        (cost, time, quotes, citations) and judge.md, the answers for a judge to score
        against the key points (see evals/JUDGE.md)
    python3 evals/run.py --out DIR --build-only
        writes the logs and the corpus only
    python3 evals/run.py --out DIR --score
        reads DIR/judgments.json and writes score.md

Only the standard library is used. `ebira ask` uses the API it is set up for and spends
under that key: about 30 US cents a case with the default models, a tenth with --quick.
"""

import argparse
import concurrent.futures
import datetime
import json
import os
import random
import re
import shutil
import subprocess
import sys
import time
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
JST = datetime.timezone(datetime.timedelta(hours=9))
NAMESPACE = uuid.UUID("6f1c2d3e-4b5a-4c6d-8e7f-9a0b1c2d3e4f")
LINE = re.compile(r"^(\d\d):(\d\d) (you|agent|wrapped|summary|interrupt|tool)\b(.*)$")
SUMMARY_LEAD = (
    "This session is being continued from a previous conversation that ran out of context. "
    "The summary below covers the earlier portion of the conversation.\n\n"
)


# ---------------------------------------------------------------------------------------
# The conversations


def parse_scenarios(directory):
    """Every session of every scenario file, in the order written."""
    sessions = []
    for path in sorted(directory.glob("*.txt")):
        project = cwd = None
        current = None
        for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            where = f"{path.name}:{number}"
            if not raw.strip() or raw.startswith("#"):
                continue
            if raw.startswith("project "):
                _, project, cwd = raw.split(maxsplit=2)
                continue
            if raw.startswith("session "):
                _, app, date, label = raw.split()
                current = new_session(app, project, cwd, date, f"{project}/{label}")
                sessions.append(current)
                continue
            if current is None:
                raise SystemExit(f"{where}: a record before any session")
            if raw.startswith("  => "):
                current["records"][-1]["output"].append(raw[5:])
                continue
            if raw.startswith("  "):
                current["records"][-1]["text"] += "\n" + raw.strip()
                continue
            match = LINE.match(raw)
            if not match:
                raise SystemExit(f"{where}: cannot read {raw!r}")
            hour, minute, kind, rest = match.groups()
            head, _, text = rest.partition(": ")
            words = head.split()
            record = new_record(f"{hour}:{minute}", kind, text.strip())
            if kind == "tool":
                if not words:
                    raise SystemExit(f"{where}: a tool needs a name")
                record["tool"] = words.pop(0)
            if words:
                record["key"] = words[0]
            current["records"].append(record)
    return sessions


def new_session(app, project, cwd, date, label):
    if app not in ("claude", "codex"):
        raise SystemExit(f"{label}: the app is claude or codex, not {app}")
    return {"app": app, "project": project, "cwd": cwd, "date": date, "label": label, "records": []}


def new_record(clock, kind, text):
    return {"time": clock, "kind": kind, "text": text, "output": [], "key": None, "tool": None}


# ---------------------------------------------------------------------------------------
# Filler: everyday work in the same projects that settles nothing the cases ask about


FILLER_PROJECTS = {
    "kurage-app": ("/work/kurage-app", "ja", "claude",
                   ["src/screens/HomeScreen.tsx", "src/components/Header.tsx", "src/api/client.ts", "src/utils/format.ts"],
                   ("npx eslint src", "✖ 3 problems (3 errors, 0 warnings)\n  src/api/client.ts  12:7  error  'res' is assigned a value but never used"),
                   ("npx prettier --write src", "src/utils/format.ts 31ms")),
    "shiori": ("/work/shiori", "ja", "claude",
               ["app/notes/page.tsx", "app/components/NoteCard.tsx", "lib/db.ts", "lib/format.ts"],
               ("npx eslint app lib", "✖ 2 problems (2 errors, 0 warnings)\n  lib/db.ts  40:3  error  Unexpected console statement"),
               ("npx prettier --write app lib", "app/notes/page.tsx 44ms")),
    "ledger-cli": ("/work/ledger-cli", "ja", "codex",
                   ["src/main.rs", "src/args.rs", "src/format.rs", "src/report.rs"],
                   ("cargo clippy", "warning: this `if` has identical blocks\n  --> src/args.rs:58:5"),
                   ("cargo fmt", "")),
    "tsumiki-game": ("/work/tsumiki-game", "ja", "codex",
                     ["scripts/stage.gd", "scripts/ui.gd", "scripts/camera.gd", "scenes/main.tscn"],
                     ("gdlint scripts", "scripts/ui.gd:22: Error: Function name \"UpdateHud\" is not valid"),
                     ("gdformat scripts", "1 file reformatted, 7 files left unchanged")),
    "blog": ("/work/blog", "ja", "claude",
             ["layouts/base.html", "content/about.md", "scripts/build.js", "styles/main.css"],
             ("npx stylelint styles", "styles/main.css\n  14:3  ✖  Unexpected duplicate selector \".nav\""),
             ("npx prettier --write layouts scripts", "scripts/build.js 25ms")),
    "notifier": ("/work/notifier", "en", "claude",
                 ["src/store.py", "src/cli.py", "src/util.py", "src/models.py"],
                 ("ruff check src", "src/util.py:8:1: F401 [*] `os` imported but unused\nFound 1 error."),
                 ("ruff format src", "1 file reformatted, 9 files left unchanged")),
    "dotfiles": ("/work/dotfiles", "ja", "claude",
                 [".zshrc", ".config/nvim/init.lua", ".gitconfig", "install.sh"],
                 ("shellcheck install.sh", "In install.sh line 12:\ncp $src $dst\n   ^-- SC2086: Double quote to prevent globbing"),
                 ("shfmt -w install.sh", "")),
    "scraper": ("/work/scraper", "ja", "codex",
                ["scraper/main.py", "scraper/parse.py", "scraper/store.py", "scraper/cli.py"],
                ("ruff check scraper", "scraper/parse.py:31:5: E722 Do not use bare `except`\nFound 1 error."),
                ("ruff format scraper", "2 files reformatted, 4 files left unchanged")),
    "kakeibo-app": ("/work/kakeibo-app", "ja", "claude",
                    ["src/summary.ts", "src/components/SpendChart.tsx", "src/export/csv.ts", "src/app.ts"],
                    ("npx eslint src", "✖ 1 problem (1 error, 0 warnings)\n  src/app.ts  9:3  error  'startup' is defined but never used"),
                    ("npx prettier --write src", "src/summary.ts 28ms")),
    "meibo-tool": ("/work/meibo-tool", "ja", "codex",
                   ["meibo/list.py", "meibo/dedupe.py", "meibo/export.py", "meibo/search.py"],
                   ("ruff check meibo", "meibo/list.py:14:1: E302 expected 2 blank lines\nFound 1 error."),
                   ("ruff format meibo", "1 file reformatted, 5 files left unchanged")),
}

FILLER_TASKS = {
    "ja": [
        ("README の誤字直して", "Edit", "README.md", "誤字を直しました。"),
        ("{file} の関数名わかりにくいから直して", "Edit", "{file}", "関数名を中身に合わせて変えました。"),
        ("lint 通して", "lint", None, "lint の指摘を直しました。"),
        ("フォーマッタかけて", "fmt", None, "整形しました。"),
        ("{file} が長いから分けて", "Edit", "{file}", "3つに分けました。"),
        ("{file} のコメントが古いところ直して", "Edit", "{file}", "古いコメントを書き直しました。"),
        ("デバッグ用のログ消しといて", "Edit", "{file}", "デバッグ用のログを消しました。"),
        (".gitignore に .DS_Store 足しといて", "Edit", ".gitignore", "追加しました。"),
        ("CI のキャッシュが効いてないっぽい", "Edit", ".github/workflows/ci.yml", "キャッシュのキーを直しました。"),
        ("{file} にドキュメントコメント足して", "Edit", "{file}", "公開している関数に説明を足しました。"),
        ("差分見せて", "Bash", "git diff --stat", "変更は3ファイルです。"),
        ("今日はここまで、コミットして", "Bash", "git commit -am wip", "コミットしました。"),
        ("{file} の重い処理キャッシュできない？", "Edit", "{file}", "結果を覚えておくようにしました。"),
        ("エディタの設定で保存時に整形されるようにして", "Edit", ".vscode/settings.json", "保存時に整形されるようにしました。"),
    ],
    "en": [
        ("Fix the typos in the README.", "Edit", "README.md", "Fixed the typos."),
        ("The function names in {file} are confusing, rename them.", "Edit", "{file}", "Renamed them after what they do."),
        ("Make the linter pass.", "lint", None, "Fixed what the linter reported."),
        ("Run the formatter.", "fmt", None, "Formatted."),
        ("{file} is too long, split it.", "Edit", "{file}", "Split it into three modules."),
        ("Remove the debug logging.", "Edit", "{file}", "Removed the debug logging."),
        ("Add .DS_Store to .gitignore.", "Edit", ".gitignore", "Added."),
        ("The CI cache doesn't seem to work.", "Edit", ".github/workflows/ci.yml", "Fixed the cache key."),
        ("Add docstrings to {file}.", "Edit", "{file}", "Added docstrings to the public functions."),
        ("Show me the diff.", "Bash", "git diff --stat", "Three files changed."),
        ("That's it for today, commit.", "Bash", "git commit -am wip", "Committed."),
    ],
}

FILLER_ACKS = {"ja": ["了解です。", "見てみます。", "対応します。"], "en": ["Sure.", "On it.", "Let me look."]}
FILLER_THANKS = {"ja": ["ありがと", "いいね", "ok", "助かる"], "en": ["thanks", "nice", "ok", "great"]}
BASH_OUTPUTS = {
    "git diff --stat": " 3 files changed, 42 insertions(+), 10 deletions(-)",
    "git commit -am wip": "[main 3f9c2ab] wip\n 3 files changed, 42 insertions(+), 10 deletions(-)",
}


def filler_sessions(seed=20261018, per_project=16):
    """The same filler every time: a few small tasks a session, on any day of September."""
    rng = random.Random(seed)
    sessions = []
    for project, (cwd, language, app, files, lint, fmt) in FILLER_PROJECTS.items():
        for index in range(per_project):
            day = rng.randint(1, 30)
            session_app = app if rng.random() < 0.7 else ("codex" if app == "claude" else "claude")
            session = new_session(session_app, project, cwd, f"2026-09-{day:02d}",
                                  f"{project}/filler-{index}")
            # Minutes since midnight; a session ends before 23:00.
            now = rng.randint(8 * 60, 21 * 60)

            def at(step):
                nonlocal now
                now += step
                return f"{now // 60:02d}:{now % 60:02d}"

            shell = "Bash" if session_app == "claude" else "shell"
            for _ in range(rng.randint(2, 4)):
                if now > 22 * 60:
                    break
                request, tool, target, done = rng.choice(FILLER_TASKS[language])
                file = rng.choice(files)
                session["records"].append(new_record(at(0), "you", request.format(file=file)))
                session["records"].append(new_record(at(1), "agent", rng.choice(FILLER_ACKS[language])))
                if tool in ("lint", "fmt"):
                    command, output = lint if tool == "lint" else fmt
                    record = new_record(at(2), "tool", command)
                    record["tool"] = shell
                    record["output"] = (output or "(no output)").splitlines()
                elif tool == "Bash":
                    record = new_record(at(2), "tool", target)
                    record["tool"] = shell
                    record["output"] = BASH_OUTPUTS[target].splitlines()
                else:
                    path = target.format(file=file)
                    if session_app == "claude":
                        record = new_record(at(2), "tool", path)
                        record["tool"] = tool
                    else:
                        record = new_record(at(2), "tool", f"apply_patch {path}")
                        record["tool"] = shell
                    record["output"] = [f"The file {path} has been updated successfully."]
                session["records"].append(record)
                session["records"].append(new_record(at(1), "agent", done))
                if rng.random() < 0.5:
                    session["records"].append(new_record(at(1), "you", rng.choice(FILLER_THANKS[language])))
                at(rng.randint(3, 12))
            sessions.append(session)
    return sessions


# ---------------------------------------------------------------------------------------
# The logs, as Claude Code and Codex write them


class Clock:
    """Each record's time: the minute written, one second after the record before it at the
    latest, in UTC."""

    def __init__(self, date):
        self.date = date
        self.last = None

    def at(self, hhmm):
        local = datetime.datetime.fromisoformat(f"{self.date}T{hhmm}:00").replace(tzinfo=JST)
        moment = local.astimezone(datetime.timezone.utc)
        if self.last is not None and moment <= self.last:
            moment = self.last + datetime.timedelta(seconds=1)
        self.last = moment
        return moment

    def next(self):
        self.last = self.last + datetime.timedelta(seconds=1)
        return self.last


def stamp(moment):
    return moment.strftime("%Y-%m-%dT%H:%M:%S.000Z")


def claude_lines(session, sid):
    """[(key, record)] for a Claude Code transcript."""
    clock = Clock(session["date"])
    lines = []
    parent = None
    count = 0

    def add(key, record, moment):
        nonlocal parent, count
        count += 1
        uid = str(uuid.uuid5(NAMESPACE, f"{sid}/{count}"))
        full = {"parentUuid": parent, "isSidechain": False, "userType": "external",
                "cwd": session["cwd"], "sessionId": sid, "version": "2.1.0"}
        full.update(record)
        full["uuid"] = uid
        full["timestamp"] = stamp(moment)
        lines.append((key, full))
        parent = uid

    for record in session["records"]:
        moment = clock.at(record["time"])
        kind, key, text = record["kind"], record["key"], record["text"]
        if kind in ("you", "wrapped"):
            add(key, {"type": "user", "message": {"role": "user", "content": text}}, moment)
        elif kind == "agent":
            add(key, {"type": "assistant", "message": {
                "role": "assistant", "model": "claude-opus-5-5", "type": "message",
                "content": [{"type": "text", "text": text}], "stop_reason": "end_turn"}}, moment)
        elif kind == "tool":
            tool_id = "toolu_" + uuid.uuid5(NAMESPACE, f"{sid}/tool/{count}").hex[:24]
            argument = {"Bash": "command"}.get(record["tool"], "file_path")
            add(None, {"type": "assistant", "message": {
                "role": "assistant", "model": "claude-opus-5-5", "type": "message",
                "content": [{"type": "tool_use", "id": tool_id, "name": record["tool"],
                             "input": {argument: text}}], "stop_reason": "tool_use"}}, moment)
            add(key, {"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tool_id,
                 "content": "\n".join(record["output"])}]}}, clock.next())
        elif kind == "summary":
            add(None, {"type": "system", "subtype": "compact_boundary",
                       "content": "Conversation compacted", "level": "info"}, moment)
            add(key, {"type": "user", "isCompactSummary": True, "message": {
                "role": "user", "content": SUMMARY_LEAD + text}}, clock.next())
        elif kind == "interrupt":
            add(key, {"type": "user", "message": {"role": "user", "content": [
                {"type": "text", "text": "[Request interrupted by user]"}]}}, moment)
    return lines


def codex_lines(session, sid):
    """[(key, record)] for a Codex rollout."""
    clock = Clock(session["date"])
    lines = []
    turn = 0
    first = clock.at(session["records"][0]["time"]) if session["records"] else None
    lines.append((None, {"timestamp": stamp(first), "type": "session_meta", "payload": {
        "id": sid, "timestamp": stamp(first), "cwd": session["cwd"],
        "originator": "codex_cli_rs", "cli_version": "0.60.0", "source": "cli"}}))

    def add(key, kind, payload, moment):
        lines.append((key, {"timestamp": stamp(moment), "type": kind, "payload": payload}))

    for record in session["records"]:
        moment = clock.at(record["time"])
        kind, key, text = record["kind"], record["key"], record["text"]
        if kind in ("you", "wrapped"):
            if kind == "wrapped":
                text = ("# Files mentioned by the user:\n\n## report.rs: "
                        f"{session['cwd']}/src/report.rs\n\n## My request for Codex:\n{text}")
            turn += 1
            add(None, "event_msg", {"type": "task_started", "turn_id": f"turn-{turn}"}, moment)
            # Codex writes the message twice at the same time: as an item and as an event.
            said = clock.next()
            add(key, "response_item", {"type": "message", "role": "user",
                                       "content": [{"type": "input_text", "text": text}]}, said)
            add(key, "event_msg", {"type": "user_message", "message": text, "images": []}, said)
        elif kind == "agent":
            add(key, "response_item", {"type": "message", "role": "assistant",
                                       "content": [{"type": "output_text", "text": text}]}, moment)
        elif kind == "tool":
            call_id = "call_" + uuid.uuid5(NAMESPACE, f"{sid}/{len(lines)}").hex[:24]
            add(None, "response_item", {"type": "function_call", "name": "shell", "call_id": call_id,
                                        "arguments": json.dumps({"command": ["bash", "-lc", text]})}, moment)
            add(key, "response_item", {"type": "function_call_output", "call_id": call_id,
                                       "output": "\n".join(record["output"])}, clock.next())
        elif kind == "summary":
            add(key, "compacted", {"message": text}, moment)
    return lines


def write_logs(sessions, home):
    """Writes every session's log under `home`, and returns where each keyed record is."""
    if home.exists():
        shutil.rmtree(home)
    keys = {}
    for session in sessions:
        sid = str(uuid.uuid5(NAMESPACE, session["label"]))
        if session["app"] == "claude":
            folder = home / ".claude" / "projects" / session["cwd"].replace("/", "-")
            path = folder / f"{sid}.jsonl"
            lines = claude_lines(session, sid)
        else:
            started = datetime.datetime.fromisoformat(
                f"{session['date']}T{session['records'][0]['time']}:00")
            folder = home / ".codex" / "sessions" / started.strftime("%Y/%m/%d")
            path = folder / f"rollout-{started.strftime('%Y-%m-%dT%H-%M-%S')}-{sid}.jsonl"
            lines = codex_lines(session, sid)
        folder.mkdir(parents=True, exist_ok=True)
        offset = 0
        with path.open("wb") as out:
            for key, record in lines:
                data = (json.dumps(record, ensure_ascii=False) + "\n").encode("utf-8")
                if key:
                    keys.setdefault(key, []).append(
                        {"path": str(path.resolve()), "byte_start": offset, "byte_len": len(data)})
                out.write(data)
                offset += len(data)
    return keys


# ---------------------------------------------------------------------------------------
# Asking


def load_cases(selected):
    cases = json.loads((HERE / "cases.json").read_text(encoding="utf-8"))["cases"]
    if selected:
        wanted = set(selected.split(","))
        cases = [case for case in cases if case["id"] in wanted]
        missing = wanted - {case["id"] for case in cases}
        if missing:
            raise SystemExit(f"no such case: {', '.join(sorted(missing))}")
    return cases


def run(command, env=None):
    result = subprocess.run(command, capture_output=True, text=True, encoding="utf-8", env=env)
    if result.returncode != 0:
        raise SystemExit(f"{' '.join(command[:2])} failed:\n{result.stderr}")
    return result.stdout


def build(out, ebira):
    sessions = parse_scenarios(HERE / "scenarios")
    sessions += filler_sessions()
    home = out / "home"
    keys = write_logs(sessions, home)
    for case in load_cases(None):
        for key in case["cite"]:
            if key not in keys:
                raise SystemExit(f"case {case['id']} cites {key}, which no scenario marks")
    (out / "records.json").write_text(json.dumps(keys, indent=1), encoding="utf-8")
    corpus = out / "corpus"
    if corpus.exists():
        shutil.rmtree(corpus)
    env = dict(os.environ, EBIRA_TZ_OFFSET="+09:00")
    run([ebira, "sync", "--corpus", str(corpus),
         "--source", str(home / ".claude" / "projects"),
         "--source", str(home / ".codex" / "sessions")], env)
    records = sum(len(session["records"]) for session in sessions)
    print(f"{len(sessions)} sessions, {records} records written under {home}; corpus at {corpus}")
    return keys


def ask(ebira, corpus, case, quick, extra=()):
    # Asked from the project, as an agent working there asks: the person's messages rarely
    # name the project they are in.
    command = [ebira, "ask", case["question"], "--cwd", case["cwd"], "--corpus", str(corpus)]
    if quick:
        command.append("--quick")
    command.extend(extra)
    started = time.time()
    result = subprocess.run(command, capture_output=True, text=True, encoding="utf-8")
    seconds = round(time.time() - started, 1)
    try:
        answer = json.loads(result.stdout)
    except json.JSONDecodeError:
        answer = None
    return {"id": case["id"], "seconds": seconds, "exit": result.returncode,
            "answer": answer, "stderr": result.stderr[-4000:]}


def cited(answer, keys, wanted):
    """Which of the records the case should cite the answer's findings cite."""
    found = set()
    for finding in (answer or {}).get("findings") or []:
        reference = finding.get("source_ref") or {}
        for key in wanted:
            for place in keys.get(key, []):
                if (os.path.realpath(reference.get("source_path") or "") == os.path.realpath(place["path"])
                        and reference.get("byte_start") == place["byte_start"]):
                    found.add(key)
    return sorted(found)


def ask_all(out, ebira, cases, quick, jobs, max_usd, extra=()):
    corpus = out / "corpus"
    keys = json.loads((out / "records.json").read_text(encoding="utf-8"))
    answers_dir = out / "answers"
    answers_dir.mkdir(exist_ok=True)
    spent = 0.0
    results = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
        pending = {}
        queue = list(cases)
        while queue or pending:
            while queue and len(pending) < jobs and (max_usd is None or spent < max_usd):
                case = queue.pop(0)
                pending[pool.submit(ask, ebira, corpus, case, quick, extra)] = case
            if not pending:
                break
            done, _ = concurrent.futures.wait(pending, return_when=concurrent.futures.FIRST_COMPLETED)
            for future in done:
                case = pending.pop(future)
                result = future.result()
                cost = ((result["answer"] or {}).get("cost_usd") or {}).get("total") or 0.0
                spent += cost
                result["cost"] = cost
                result["cited"] = cited(result["answer"], keys, case["cite"])
                results[case["id"]] = result
                (answers_dir / f"{case['id']}.json").write_text(
                    json.dumps(result, ensure_ascii=False, indent=1), encoding="utf-8")
                print(f"{case['id']}: {result['seconds']}s ${cost:.3f} exit {result['exit']}"
                      f" cited {len(result['cited'])}/{len(case['cite'])}", flush=True)
    skipped = [case["id"] for case in cases if case["id"] not in results]
    if skipped:
        print(f"not asked, the spending limit was reached: {', '.join(skipped)}")
    return results


# ---------------------------------------------------------------------------------------
# Reports


def answer_text(answer):
    if not answer:
        return "(no answer: ask failed)"
    # What ask says first when the records found do not settle the question.
    lead = [answer["not_settled"], ""] if answer.get("not_settled") else []
    if answer.get("disposition") == "no_records":
        return lead[0] if lead else "(no answer: ask found no records to answer from)"
    lines = lead + [answer.get("answer") or "(empty)"]
    findings = answer.get("findings") or []
    if findings:
        lines.append("\nFindings:")
        for finding in findings:
            who = finding.get("author") or "?"
            when = finding.get("local_time") or "?"
            mark = "" if finding.get("quote_verified") else " (not found in the records)"
            lines.append(f"- [{finding.get('kind')}] {when} {who}: 「{finding.get('quote')}」{mark}")
    gaps = answer.get("gaps") or []
    if gaps:
        lines.append("\nWhat the records do not settle:")
        lines.extend(f"- {gap}" for gap in gaps)
    return "\n".join(lines)


def write_reports(out, cases, results, quick):
    rows = []
    for case in cases:
        result = results.get(case["id"])
        if not result:
            continue
        answer = result["answer"] or {}
        findings = answer.get("findings") or []
        verified = sum(1 for finding in findings if finding.get("quote_verified"))
        rows.append((case, result, len(findings), verified))
    total = sum(result["cost"] for _, result, _, _ in rows)
    lines = [f"# ask on made-up logs ({'--quick' if quick else 'default models'})", "",
             f"{len(rows)} cases, ${total:.2f}, {sum(r['seconds'] for _, r, _, _ in rows):.0f} s of asking.", "",
             "| case | type | $ | s | quotes verified | records cited |", "|---|---|---|---|---|---|"]
    for case, result, count, verified in rows:
        cite = f"{len(result['cited'])}/{len(case['cite'])}" if case["cite"] else "-"
        state = "" if result["exit"] == 0 else " (failed)"
        lines.append(f"| {case['id']}{state} | {case['type']} | {result['cost']:.3f} | {result['seconds']:.0f} "
                     f"| {verified}/{count} | {cite} |")
    (out / "report.md").write_text("\n".join(lines) + "\n", encoding="utf-8")

    packets = ["# Answers to judge", "", "Score each answer as evals/JUDGE.md says.", ""]
    for case, result, _, _ in rows:
        packets += [f"## {case['id']}", "", f"Question: {case['question']}", "", "Key points:"]
        packets += [f"{number}. {point}" for number, point in enumerate(case["key_points"], 1)]
        packets += ["", "Mistakes to watch for:"]
        packets += [f"{number}. {error}" for number, error in enumerate(case["errors"], 1)]
        packets += ["", "Answer:", "", answer_text(result["answer"]), ""]
    (out / "judge.md").write_text("\n".join(packets), encoding="utf-8")
    summary = {case["id"]: {"cost": result["cost"], "seconds": result["seconds"], "exit": result["exit"],
                            "cited": result["cited"], "cite": case["cite"]} for case, result, _, _ in rows}
    (out / "summary.json").write_text(json.dumps(summary, indent=1), encoding="utf-8")
    print(f"wrote {out / 'report.md'} and {out / 'judge.md'}; ${total:.2f} spent")


def score(out):
    cases = {case["id"]: case for case in load_cases(None)}
    judgments = json.loads((out / "judgments.json").read_text(encoding="utf-8"))
    summary = json.loads((out / "summary.json").read_text(encoding="utf-8"))
    by_type = {}
    lines = ["# Scores", "", "| case | type | key points | mistakes | records cited |", "|---|---|---|---|---|"]
    for case_id, judged in sorted(judgments.items()):
        case = cases[case_id]
        points = judged["points"]
        made = sum(1 for mistake in judged["mistakes"] if mistake)
        share = sum(points) / len(points)
        entry = by_type.setdefault(case["type"], {"share": [], "mistaken": 0, "cases": 0})
        entry["share"].append(share)
        entry["mistaken"] += 1 if made else 0
        entry["cases"] += 1
        cite = summary.get(case_id, {})
        cited_text = f"{len(cite.get('cited', []))}/{len(cite.get('cite', []))}" if cite.get("cite") else "-"
        lines.append(f"| {case_id} | {case['type']} | {sum(points):g}/{len(points)} | {made} | {cited_text} |")
    lines += ["", "| type | key points made | answers with a mistake |", "|---|---|---|"]
    for kind, entry in by_type.items():
        lines.append(f"| {kind} | {100 * sum(entry['share']) / len(entry['share']):.0f}% "
                     f"| {entry['mistaken']}/{entry['cases']} |")
    shares = [share for entry in by_type.values() for share in entry["share"]]
    mistaken = sum(entry["mistaken"] for entry in by_type.values())
    lines += ["", f"All: {100 * sum(shares) / len(shares):.0f}% of the key points, "
                  f"{mistaken}/{len(shares)} answers with a mistake."]
    (out / "score.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("\n".join(lines))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", required=True, type=Path, help="where the logs, corpus, and results go")
    parser.add_argument("--ebira", default="ebira", help="the ebira to run (default: ebira on PATH)")
    parser.add_argument("--quick", action="store_true", help="ask with --quick")
    parser.add_argument("--ask-arg", action="append", default=[],
                        help="another option for every ask, such as --sweep (repeat for several)")
    parser.add_argument("--cases", help="comma-separated case ids (default: all)")
    parser.add_argument("--jobs", type=int, default=3, help="questions asked at once (default 3)")
    parser.add_argument("--max-usd", type=float, help="stop starting cases once this much is spent")
    parser.add_argument("--build-only", action="store_true", help="write the logs and the corpus only")
    parser.add_argument("--score", action="store_true", help="score DIR/judgments.json")
    args = parser.parse_args()
    out = args.out.resolve()
    if args.score:
        score(out)
        return
    out.mkdir(parents=True, exist_ok=True)
    build(out, args.ebira)
    if args.build_only:
        return
    cases = load_cases(args.cases)
    results = ask_all(out, args.ebira, cases, args.quick, args.jobs, args.max_usd, args.ask_arg)
    write_reports(out, cases, results, args.quick)


if __name__ == "__main__":
    sys.exit(main())
