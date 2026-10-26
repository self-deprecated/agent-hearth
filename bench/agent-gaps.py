#!/usr/bin/env python3
"""How coding agents interleave file writes and cargo builds.

Reads Claude Code and Codex transcripts and prints aggregate numbers only: how long agents
wait on cargo, how long before each cargo call the last write landed, how much of the wait
a silent build started after each write could have hidden, and how much of it repeats a
command whose result a cache could replay.

    python3 -I bench/agent-gaps.py [--claude DIR] [--codex DIR] [--debug]

Defaults: --claude ~/.claude/projects, --codex ~/.codex/sessions. --debug adds one line per
cargo call (session index, times, kind) on stderr; it never prints paths or commands.
"""

import argparse
import collections
import datetime
import glob
import json
import math
import os
import re
import statistics
import sys
import zlib

# cargo as a command word: at the start of a shell segment or after a wrapper.
CARGO = re.compile(
    r"(?:^|[;&|(\n`{]|\$\(|\s--\s|\s-\w*c\s+"
    r"|\b(?:agent-hearth|time|exec|env|nice|command|xargs|then|do)\s+"
    r"|\btimeout\s+\S+\s+|\bdevenv\s+shell\s+|\bdevbox\s+run\s+|\bdirenv\s+exec\s+\S+\s+)"
    r"\s*(?P<env>(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*)(?:[^\s;&|]*/)?"
    r"(?P<cargo>cargo(?:\s+\+\S+)?(?:\s+-[^\s;&|]+)*\s+(?P<sub>[a-z][a-z0-9-]*))"
)
KIND = {
    "test": "test", "t": "test", "nextest": "test",
    "build": "build/check", "b": "build/check", "check": "build/check", "c": "build/check",
    "clippy": "clippy",
}
KINDS = ("test", "build/check", "clippy", "several")
WRITE_TOOLS = {"Edit", "Write", "MultiEdit", "NotebookEdit"}
HEREDOC = re.compile(r"<<-?\s*(['\"]?)(\w+)\1([^\n]*\n).*?^\s*\2\s*$", re.S | re.M)
QUOTED = re.compile(r"'[^']*'|\"(?:[^\"\\]|\\.)*\"", re.S)
RUNS_QUOTED = re.compile(r"(?:\s-\w*c|\bssh(?:\s+-\S+)*\s+[^\s-]\S*|\beval)\s*$")
SEGMENT_END = re.compile(r"[;&|)\n`]")
FD_REDIRECT = re.compile(r"\d*>&\d+|&>>?\s*\S+")
REDIRECT = re.compile(r"\s\d*>>?\s*\S+|\s<\s*\S+")
INFO = re.compile(r"\s(?:--version|-V|--help|-h)(?:\s|$)")
CD = re.compile(r"(?:^|[;&|(\s])(?:cd|pushd)\s+(\S+)")
# Shell commands that probably write a project file. Checked against Claude's own record of
# files changed by Bash it is imprecise (about half its hits change no tracked file), so a
# separate shell command matching it counts only as a possible write. Before cargo in the
# same command it counts as writing before cargo, even where Claude recorded no net change
# (agents edit, build and restore a file in one command).
WRITE_HINT = re.compile(
    r"\bsed\s+(-\w+\s+)*-i|\bperl\s+-\w*i|\btee\s+(-a\s+)?(?!/tmp|/dev)[\w~.]"
    r"|(?<![0-9&>])>>?\s*(?!/dev/|/tmp/|&)[\w~.$\"'][^\s]*\.\w+|write_text|\.write\(|\bpatch\s"
)
# Something before cargo in the same command that can write the files Claude recorded.
BEFORE = re.compile(r"<<|\b(?:sed|perl|python3?|tee|cp|mv|rm|touch|patch|jj|git)\s|(?<![0-9&])>[^&]")
# Weaker cues before cargo that make a command too uncertain to count as an exact repeat.
MAYBE = re.compile(r"<<|\b(?:sed|perl|python3?|cp|mv|rm|touch|patch|git|jj)\s")
FAILED = re.compile(r"\berror(?:\[E\d{4}\])?:|test result: FAILED|could not compile|\bpanicked at\b")


def ts(s):
    return datetime.datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()


def shell_code(cmd):
    """The parts of a shell command that run as commands: heredoc bodies are dropped and
    quoted strings (commit messages, patterns, Python edit scripts) become an opaque token
    per content, except strings handed to `sh -c`, `ssh host` or `eval`, which are kept and
    cleaned too."""
    cmd = HEREDOC.sub(lambda m: m.group(3), cmd)

    def unquote(m):
        if RUNS_QUOTED.search(m.string[: m.start()]):
            return " ; " + shell_code(m.group()[1:-1]) + " ; "
        return f"_q{zlib.crc32(m.group().encode()):x}"

    return QUOTED.sub(unquote, cmd)


def classify(cmd, cwd=None):
    """Return (kind or None, cargo subcommands, key) for a shell command. The key is the
    same for identical cargo invocations: working directory, cd targets before cargo, and
    each cargo's VAR=value prefixes and argument list up to the next shell operator."""
    code = FD_REDIRECT.sub(" ", shell_code(cmd))
    ms = list(CARGO.finditer(code))
    subs, kinds, segs = [], set(), []
    for m in ms:
        end = SEGMENT_END.search(code, m.end())
        args = " ".join(REDIRECT.sub(" ", " " + m.group("cargo") + code[m.end(): end.start() if end else len(code)]).split())
        if INFO.search(args):  # cargo clippy --version and the like
            continue
        subs.append(m.group("sub"))
        kinds |= {KIND[m.group("sub")]} if m.group("sub") in KIND else set()
        segs.append((" ".join(sorted(m.group("env").split())), args))
    kind = None if not kinds else kinds.pop() if len(kinds) == 1 else "several"
    key = (cwd, tuple(CD.findall(code[: ms[0].start()])) if ms else (), tuple(segs))
    return kind, subs, key


def read_jsonl(path):
    with open(path, errors="replace") as f:
        for line in f:
            try:
                o = json.loads(line)
            except ValueError:
                continue
            if isinstance(o, dict):
                yield o


def is_manifest(path):
    return os.path.basename(str(path)) == "Cargo.toml"


def text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(x.get("text", "") for x in content if isinstance(x, dict))
    return ""


# Events, in a session's timeline:
#   {"start", "end", "write": True, "manifest"}   a recorded write
#   {"start", "end", "hint": True}                a shell command that may have written
#   cargo_event(...)                              a shell command that runs cargo; kind is
#                                                 None when it runs only uncounted subcommands
# A cargo event's inline: it wrote before cargo; after: it changed files, not before cargo
# (tests or later steps); may_write: it may have written before cargo (no record available).

def cargo_event(start, end, kind, subs, key, ok, bg=False, inline=False, after=False,
                may_write=False, manifest=False):
    return {"start": start, "end": end, "kind": kind, "subs": subs, "key": key, "ok": ok,
            "bg": bg, "inline": inline, "after": after, "may_write": may_write,
            "manifest": manifest, "add": "add" in subs,
            "other": [s for s in subs if s not in KIND]}


def cues(cmd, subs, recorded=False):
    """(inline, may_write) from the text before a command's last cargo. Without a record of
    changed files, cargo fmt (not --check) may also have written."""
    before = cmd[: cmd.rfind("cargo")]
    inline = bool(WRITE_HINT.search(before))
    fmt = not recorded and "fmt" in subs and "--check" not in cmd
    return inline, not inline and (fmt or bool(MAYBE.search(before)))


def claude_events(path, seen):
    calls, events = {}, []
    for o in read_jsonl(path):
        msg = o.get("message") if isinstance(o.get("message"), dict) else {}
        content = msg.get("content")
        if not isinstance(content, list) or "timestamp" not in o:
            continue
        t = ts(o["timestamp"])
        for b in content:
            if not isinstance(b, dict):
                continue
            if o.get("type") == "assistant" and b.get("type") == "tool_use":
                if not b.get("id") or b["id"] in seen:  # resumed sessions repeat history
                    continue
                seen.add(b["id"])
                name = b.get("name", "").split("__")[-1].split("_")[-1]
                calls[b["id"]] = {"start": t, "end": None, "name": name, "cwd": o.get("cwd"),
                                  "input": b.get("input") or {}}
            elif o.get("type") == "user" and b.get("type") == "tool_result":
                c = calls.get(b.get("tool_use_id"))
                if c is None or c["end"] is not None:
                    continue
                c["end"] = t
                c["error"] = bool(b.get("is_error"))
                c["output"] = text(b.get("content"))
                r = o.get("toolUseResult")
                c["result"] = r if isinstance(r, dict) else None
    # Claude Code records the files a successful Bash command changed (bashEditDiff), but
    # not in every session (not in subagents or outside a git checkout, it seems). Sessions
    # with no record at all are handled like Codex.
    tracked = any(c.get("result") and "bashEditDiff" in c["result"] for c in calls.values())
    stats = {"bash_writes": 0, "tracked": tracked}
    for c in calls.values():
        if c["end"] is None:
            continue
        inp = c["input"]
        if c["name"] in WRITE_TOOLS:
            if not c["error"]:
                p = inp.get("file_path") or inp.get("notebook_path") or ""
                events.append({"start": c["start"], "end": c["end"], "write": True,
                               "manifest": is_manifest(p)})
        elif c["name"] == "Bash":
            cmd = inp.get("command") or ""
            kind, subs, key = classify(cmd, c["cwd"])
            # Failed commands (result is an error string) and untracked sessions have no
            # record; "shared" marks a checkout other processes also write.
            result = c["result"] if tracked else None
            diff = (result or {}).get("bashEditDiff") or {}
            changed = [] if diff.get("shared") else [
                p for p in diff.get("changedFiles") or [f.get("filePath") for f in diff.get("files") or []]
                if os.path.basename(str(p)) != "Cargo.lock"]
            manifest = any(is_manifest(p) for p in changed)
            if subs:
                bg = bool(inp.get("run_in_background") or (c["result"] or {}).get("backgroundTaskId"))
                ok = not c["error"] and not bg and not FAILED.search(c["output"])
                inline, may_write = cues(cmd, subs, result is not None)
                if changed and not inline:
                    # Changed files count as written before cargo only when something before
                    # cargo could write them; otherwise the tests or a later step wrote them.
                    inline = "fmt" in subs or bool(BEFORE.search(cmd[: cmd.rfind("cargo")]))
                    may_write = may_write and not inline
                after = bool(changed) and not inline
                events.append(cargo_event(c["start"], c["end"], kind, subs, key, ok, bg, inline,
                                          after, may_write, manifest))
            elif changed:
                stats["bash_writes"] += 1
                events.append({"start": c["start"], "end": c["end"], "write": True, "manifest": manifest})
            elif result is None and WRITE_HINT.search(cmd):
                events.append({"start": c["start"], "end": c["end"], "hint": True})
    return events, stats


def codex_events(path, seen):
    events, stats = [], {"sub": False, "tracked": False}
    for o in read_jsonl(path):
        p = o.get("payload") if isinstance(o.get("payload"), dict) else {}
        if o.get("type") == "session_meta":
            stats["sub"] = isinstance(p.get("source"), dict)  # spawned by another agent
        if p.get("type") != "item_completed" or not isinstance(p.get("item"), dict):
            continue
        it = p["item"]
        if (it.get("id") and it["id"] in seen) or "completed_at_ms" not in p:
            continue
        seen.add(it.get("id") or object())
        end = p["completed_at_ms"] / 1000
        if it.get("type") == "FileChange":  # apply_patch
            if it.get("status") == "completed":
                events.append({"start": p.get("started_at_ms", p["completed_at_ms"]) / 1000,
                               "end": end, "write": True,
                               "manifest": any(is_manifest(k) for k in it.get("changes") or {})})
        elif it.get("type") == "CommandExecution":
            cmd = it.get("command") or ""
            if isinstance(cmd, list):
                cmd = cmd[-1] if len(cmd) == 3 and cmd[1] in ("-c", "-lc") else " ".join(cmd)
            d = it.get("duration") or {}
            dur = d.get("secs", 0) + d.get("nanos", 0) / 1e9 if isinstance(d, dict) else 0
            start = p["started_at_ms"] / 1000 if "started_at_ms" in p else end - dur
            kind, subs, key = classify(cmd, it.get("cwd"))
            if subs:
                ok = it.get("exit_code") == 0 and not FAILED.search(it.get("aggregated_output") or "")
                inline, may_write = cues(cmd, subs)
                events.append(cargo_event(start, end, kind, subs, key, ok, inline=inline,
                                          may_write=may_write,
                                          manifest="Cargo.toml" in cmd[: cmd.rfind("cargo")]))
            elif WRITE_HINT.search(cmd):
                events.append({"start": start, "end": end, "hint": True})
    return events, stats


def builds(events, hints=False):
    """One record per counted cargo call: writes since the previous counted call, and
    whether an identical command ran earlier with no write since.

    Three write counters: recorded (writes the main numbers use), loose (recorded plus
    files a cargo command changed after cargo started) and all (loose plus possible shell
    writes and unrecorded cargo fmt). An exact repeat needs "all" unchanged since the
    identical command started."""
    out, pending, manifest, prev = [], [], False, None
    rec = loose = every = 0
    last = {}
    for e in sorted(events, key=lambda e: e["start"]):
        if e.get("write") or (hints and e.get("hint")):
            pending.append(e)
            rec, loose, every = rec + 1, loose + 1, every + 1
            continue
        if e.get("hint"):
            every += 1
            continue
        if "kind" not in e:
            continue
        manifest = manifest or e["add"] or e["manifest"]
        if e["inline"]:  # the same shell command wrote, then ran cargo: nothing to hide
            pending.append({"end": e["start"]})
            rec, loose, every = rec + 1, loose + 1, every + 1
        elif e["may_write"]:
            every += 1
        if e["kind"] is None:  # cargo fmt, run, add, ...: not waited on here
            if e["after"]:
                pending.append({"end": e["end"]})
                rec, loose, every = rec + 1, loose + 1, every + 1
            continue
        b = {"kind": e["kind"], "bg": e["bg"], "start": e["start"], "writes": len(pending),
             "duration": None if e["bg"] else e["end"] - e["start"], "inline": e["inline"],
             "same_kind": prev == e["kind"], "prev_kind": prev, "key": e["key"]}
        if pending:
            b["gap"] = max(0.0, e["start"] - pending[-1]["end"])
            b["window"] = max(0.0, e["start"] - pending[0]["end"])
            b["manifest"] = manifest or any(w.get("manifest") for w in pending)
        seen = last.get(e["key"])
        b["repeat"] = bool(seen) and seen[2] == every
        b["repeat_ok"] = b["repeat"] and seen[3]
        b["repeat_loose"] = bool(seen) and seen[1] == loose
        if pending:
            b["cause"] = None
        elif b["repeat"]:
            b["cause"] = "repeat"
        elif prev is None:
            b["cause"] = "first"
        elif seen and seen[0] == rec:
            b["cause"] = "maybe changed"
        else:
            b["cause"] = "different"
        out.append(b)
        last[e["key"]] = (rec, loose, every, e["ok"])
        if e["after"]:
            loose, every = loose + 1, every + 1
        pending, manifest, prev = [], False, e["kind"]
    return out


def dist(xs):
    if not xs:
        return "n=0"
    xs = sorted(xs)
    q = statistics.quantiles(xs, n=20, method="inclusive") if len(xs) > 1 else [xs[0]] * 19
    return (f"n={len(xs)} median={statistics.median(xs):.1f} p25={q[4]:.1f} p75={q[14]:.1f} "
            f"p90={q[17]:.1f} mean={statistics.fmean(xs):.1f}")


def hours(s):
    return f"{s / 3600:.2f} h"


def pct(x, d):
    return f"{100 * x / d:.0f}%" if d else "n/a"


def totals(bs):
    timed = [b for b in bs if b["duration"] is not None]
    edited = [b for b in timed if b["writes"]]
    return (timed, edited, sum(b["duration"] for b in timed), sum(b["duration"] for b in edited),
            sum(min(b["duration"], b["gap"]) for b in edited),
            sum(min(b["duration"], b["window"]) for b in edited))


def waited(bs):
    return sum(b["duration"] for b in bs)


def report(name, bs, out, transitions=False):
    timed, edited, total, edited_total, lo, hi = totals(bs)
    reruns = [b for b in timed if not b["writes"]]
    inline = sum(1 for b in edited if b["inline"])
    out.append(f"  [{name}] cargo calls {len(bs)}: timed {len(timed)}, after new writes {len(edited)} "
               f"({inline} of them wrote in the same shell command), reruns without new writes "
               f"{len(reruns)} ({sum(1 for b in reruns if b['same_kind'])} the same kind as the "
               f"previous cargo call)")
    out.append(f"    duration s, all timed      {dist([b['duration'] for b in timed])}")
    out.append(f"    duration s, after writes   {dist([b['duration'] for b in edited])}")
    out.append(f"    gap s                      {dist([b['gap'] for b in edited])}")
    out.append(f"    edit window s              {dist([b['window'] for b in edited])}")
    out.append(f"    writes per build           {dist([b['writes'] for b in edited])}")
    if edited:
        over = sum(1 for b in edited if b["gap"] >= b["duration"])
        zero = sum(1 for b in edited if b["gap"] < 1)
        out.append(f"    after writes: gap >= duration in {over} ({pct(over, len(edited))}); "
                   f"gap < 1 s in {zero} ({pct(zero, len(edited))})")
    out.append(f"    waiting {hours(total)}: after writes {hours(edited_total)}, reruns "
               f"{hours(total - edited_total)}")
    out.append(f"    hideable lower {hours(lo)} = {pct(lo, total)} of all waiting, "
               f"{pct(lo, edited_total)} of waiting after writes")
    out.append(f"    hideable upper {hours(hi)} = {pct(hi, total)} of all waiting, "
               f"{pct(hi, edited_total)} of waiting after writes")
    rep = [b for b in timed if b["repeat"]]
    ok = [b for b in rep if b["repeat_ok"]]
    out.append(f"    cache: exact repeats {len(rep)}, {hours(waited(rep))} = {pct(waited(rep), total)} "
               f"of all waiting; of them after a successful identical run {len(ok)}, "
               f"{hours(waited(ok))} = {pct(waited(ok), total)}")
    by = {c: [b for b in reruns if b["cause"] == c] for c in ("first", "maybe changed", "different")}
    out.append(f"    other reruns: different command {len(by['different'])}, "
               f"{hours(waited(by['different']))} = {pct(waited(by['different']), total)}; "
               f"session's first cargo call {len(by['first'])}, {hours(waited(by['first']))}; "
               f"identical but files may have changed since {len(by['maybe changed'])}, "
               f"{hours(waited(by['maybe changed']))}")
    if transitions:
        pairs = collections.defaultdict(list)
        for b in by["different"]:
            pairs[(b["prev_kind"], b["kind"])].append(b)
        top = sorted(pairs.items(), key=lambda kv: -len(kv[1]))[:5]
        out.append("    different-command reruns, top kind transitions: " + "; ".join(
            f"{a} -> {k} {len(v)} ({hours(waited(v))})" for (a, k), v in top))
        loose = [b for b in timed if b["repeat_loose"]]
        out.append(f"    cache, if possible shell writes and unrecorded cargo fmt are ignored: exact "
                   f"repeats {len(loose)}, {pct(waited(loose), total)} of all waiting")


def section(bs, out):
    report("all kinds", bs, out, transitions=True)
    for kind in KINDS:
        kb = [b for b in bs if b["kind"] == kind]
        if len(kb) >= 20:
            report(kind, kb, out)


def without_heaviest(sessions, out):
    """The headline shares without the top 5% of sessions by cargo calls."""
    n = math.ceil(len(sessions) * 0.05)
    ranked = sorted(sessions, key=len, reverse=True)
    rest = [b for s in ranked[n:] for b in s]
    heavy = sum(len(s) for s in ranked[:n])
    timed, _, total, _, lo, hi = totals(rest)
    rep = [b for b in timed if b["repeat"]]
    ok = [b for b in rep if b["repeat_ok"]]
    diff = [b for b in timed if b["cause"] == "different"]
    out.append(f"  sensitivity, without the {n} sessions with the most cargo calls (top 5%, "
               f"{heavy} of {heavy + len(rest)} calls): waiting {hours(total)}; hideable lower "
               f"{pct(lo, total)} / upper {pct(hi, total)}; exact repeats {pct(waited(rep), total)} "
               f"(after success {pct(waited(ok), total)}); different-command reruns "
               f"{pct(waited(diff), total)} of all waiting")


HEADER = """\
agent-gaps: cargo waits vs. the time since the agent's last file write
Wall-clock seconds from transcript timestamps; aggregates only.

Definitions
  cargo call    a shell command that runs cargo test/t/nextest, build/b, check/c or clippy
                (directly or via devenv shell, agent-hearth, sh -c, ssh, timeout, ...).
                A command that runs several of these counts once, kind "several".
                cargo fmt/run/add/metadata/... are not counted.
  duration      tool call start -> result: how long the agent waited.
  writes        Claude: Edit/Write/MultiEdit/NotebookEdit, and Bash commands for which
                Claude Code recorded changed files (Cargo.lock alone ignored).
                Codex: apply_patch. Both: a shell write (sed -i, > file, .write(, ...)
                before cargo in the same command, recorded or not (agents edit, build
                and restore a file in one command, or edit another checkout).
                Writes are counted since the previous cargo call.
  rerun         a cargo call with no write since the previous cargo call.
  gap           end of the last write -> start of the cargo call (0 when the same
                command wrote first).
  edit window   end of the first write since the previous cargo call -> cargo call.
  hideable      if a silent build of the same duration had started after the writes:
                lower = min(duration, gap): only the build started after the last write
                counts, as an edit invalidates the build before it.
                upper = min(duration, edit window): building continuously from the first
                write, as if every earlier build's work carried over.
  exact repeat  a cargo call identical to an earlier one in the same session, with no
                write since that earlier one started (other cargo calls between are
                fine). Besides writes, these also break it: files the earlier cargo
                command itself changed; where no record exists, shell commands that
                look like writes and cargo fmt without --check; and anything before
                cargo in the command that might write (heredoc, python, sed, perl, cp,
                mv, rm, touch, patch, git, jj).
  identical     same working directory (the transcript's, plus any cd before cargo) and,
                for every cargo in the command in order, the same argument list and
                VAR=value prefix. Ignored: wrappers before cargo (devenv shell --,
                agent-hearth, sh -c, timeout, nice, env), redirections such as 2>&1, and
                everything after the next |, ;, && or ) (so | tail -5 is ignored).
                Quoted arguments are compared by content. agent-hearth cargo test and
                cargo test are identical.
  successful    Claude: the tool result is not an error (exit code 0) and the output the
                agent saw has no cargo failure text (error:, error[E...]:, test result:
                FAILED, could not compile, panicked at). Codex: exit_code 0 and the same
                text check. The text check catches failures a pipe such as | tail hides
                from the exit code; truncated output can still hide one.
  other reruns  reruns that are not exact repeats: a different command (no identical
                command since the last write), the session's first cargo call, or an
                identical command after which files may have changed (see exact repeat).
"""

CAVEATS = """\
Caveats
- Assumes the silent build takes as long as the agent's. For test calls the duration also
  runs the tests; a silent compile-only build hides at most the compile part.
- Claude's bashEditDiff decides which separate Bash commands wrote files. It is missing in
  failed commands, in subagent transcripts, in shared checkouts and in whole sessions
  (most Claude sessions here; likely outside a git checkout); those fall back to the
  Codex heuristics. A cargo command's own recorded changes count as a write before cargo
  (gap 0) only when an edit or cargo fmt precedes cargo in it; otherwise they count only
  against exact repeats.
- Exact repeats are deliberately strict (any possible write breaks them); the "if
  possible shell writes ... are ignored" line is the looser count.
- Codex gaps under 1 s are an apply_patch and a cargo call inside one tool call (code
  mode), which is writing in the same command in effect.
- Codex (and Claude without a record) has no list of files changed by shell commands:
  separate shell writes are not counted in the main numbers (see the sensitivity line),
  so some reruns are really builds after writes. Exact repeats count them as writes.
- An exact repeat replays the same inputs only if nothing outside the transcript changed
  files: other agents in the same checkout, subagents, the human, or cargo's own
  environment (toolchain, registry, network). Each transcript is its own timeline.
- Background cargo calls (Claude run_in_background) are counted but not timed. Codex
  long commands are timed from process start to exit, even if the agent polled them.
- gap and edit window include time the human took between turns; durations are capped
  by min(), so totals are unaffected, but the gap percentiles' upper tail is human time.
- One user's sessions on one machine. Claude Code deletes old transcripts, so the Claude
  dates cover a few days only. Some sessions run cargo repeatedly without edits to
  measure builds, which inflates reruns and repeats (see the top-5% sensitivity line).
- Commands that build through a project script (check, devenv test, make, just) without
  the word cargo are missed.
"""


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--claude", default="~/.claude/projects", help="Claude Code projects dir")
    ap.add_argument("--codex", default="~/.codex/sessions", help="Codex sessions dir")
    ap.add_argument("--debug", action="store_true", help="per-call lines on stderr")
    args = ap.parse_args()

    out, every_session = [HEADER], []
    for name, root, parse in (("claude", args.claude, claude_events),
                              ("codex", args.codex, codex_events)):
        files = sorted(glob.glob(os.path.join(os.path.expanduser(root), "**", "*.jsonl"),
                                 recursive=True))
        seen, sessions = set(), []
        for f in files:
            events, stats = parse(f, seen)
            stats["sub"] = stats["sub"] if "sub" in stats else f"{os.sep}subagents{os.sep}" in f
            bs = builds(events)
            if bs:
                sessions.append((events, bs, stats))
        bs = [b for _, s, _ in sessions for b in s]
        every_session += [s for _, s, _ in sessions]
        out.append(f"== {name}: {len(files)} transcript files, {len(sessions)} sessions with a "
                   f"cargo call ({sum(1 for *_, st in sessions if st['sub'])} of them subagent "
                   f"sessions)")
        if not bs:
            out.append("")
            continue
        starts = sorted(b["start"] for b in bs)
        dates = [datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime("%Y-%m-%d")
                 for t in (starts[0], starts[-1])]
        evs = [e for ev, _, _ in sessions for e in ev]
        out.append(f"  dates {dates[0]}..{dates[1]}; untimed background cargo calls "
                   f"{sum(1 for b in bs if b['bg'])}; uncounted cargo subcommands "
                   f"{sum(len(e.get('other', ())) for e in evs)}; cargo add calls "
                   f"{sum(1 for e in evs if e.get('add'))}; builds after a Cargo.toml change "
                   f"or cargo add {sum(1 for b in bs if b.get('manifest'))}")
        if name == "claude":
            untracked = [s for _, s, st in sessions if not st["tracked"]]
            out.append(f"  Bash commands counted as writes: "
                       f"{sum(st['bash_writes'] for *_, st in sessions)}; sessions without any "
                       f"Bash change record {len(untracked)} ({sum(map(len, untracked))} cargo calls)")
        hinted = [b for ev, _, _ in sessions for b in builds(ev, hints=True)]
        _, edited, total, _, lo, hi = totals(hinted)
        out.append(f"  sensitivity, possible shell writes where no record exists counted as "
                   f"writes ({sum(1 for e in evs if e.get('hint'))} of them): after writes "
                   f"{len(edited)}, hideable lower {pct(lo, total)} / upper {pct(hi, total)} "
                   f"of all waiting")
        without_heaviest([s for _, s, _ in sessions], out)
        section(bs, out)
        out.append("")
        if args.debug:
            for i, (_, s, _) in enumerate(sessions):
                for b in s:
                    print(name, i, f"{b['start']:.3f}", b["kind"], b["bg"], b["inline"], b["duration"],
                          b["writes"], b.get("gap"), b.get("window"), b["cause"], b["repeat_ok"],
                          file=sys.stderr)
    out.append("== combined")
    without_heaviest(every_session, out)
    section([b for s in every_session for b in s], out)
    out.append("")
    out.append(CAVEATS)
    print("\n".join(out), end="")


if __name__ == "__main__":
    main()
