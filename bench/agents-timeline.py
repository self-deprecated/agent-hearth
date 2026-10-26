"""Reads agents.sh's timeline.log and prints how agents interleave edits and cargo runs.

timeline.log: "<agent> <time> <pre|post> <tool> <runs cargo: true|false>" per hook call. A cargo
run is a Bash call that runs cargo, from its PreToolUse to its PostToolUse; an edit is the
PostToolUse of an Edit/Write/MultiEdit/NotebookEdit call (Claude Code) or an apply_patch call
(Codex). For each cargo run: how long since the agent's last edit (when there was one since its
previous cargo run), and how long it took. Bash calls that write files are not edits here, so
builds in the same command as an edit count as reruns.
"""

import sys
from collections import defaultdict

EDITS = {"Edit", "Write", "MultiEdit", "NotebookEdit", "apply_patch"}

events = defaultdict(list)
for line in open(sys.argv[1]):
    parts = line.split()
    if len(parts) == 5:
        agent, time, event, tool, cargo = parts
        events[agent].append((float(time), event, tool, cargo == "true"))

gaps, after_edit, reruns, edits = [], [], [], 0
for agent, timeline in events.items():
    timeline.sort()
    last_edit, edited, started = None, False, None
    for time, event, tool, cargo in timeline:
        if event == "post" and tool in EDITS:
            last_edit, edited, edits = time, True, edits + 1
        elif tool == "Bash" and cargo and event == "pre":
            started = (time, time - last_edit if edited else None)
        elif tool == "Bash" and cargo and event == "post" and started:
            begin, gap = started
            if gap is None:
                reruns.append(time - begin)
            else:
                gaps.append(gap)
                after_edit.append(time - begin)
            edited, started = False, None


def median(values):
    values = sorted(values)
    return values[len(values) // 2] if values else 0.0


waiting = sum(after_edit) + sum(reruns)
print(f"{len(events)} agents, {edits} edits, {len(after_edit) + len(reruns)} cargo runs: "
      f"{len(after_edit)} after an edit, {len(reruns)} with no edit since the previous one")
if gaps:
    quick = sum(1 for gap in gaps if gap < 1)
    print(f"last edit to cargo run: median {median(gaps):.1f}s, under 1s in {quick} of {len(gaps)}, "
          f"over 10s in {sum(1 for gap in gaps if gap > 10)}")
print(f"waiting on cargo: {waiting:.0f}s, {sum(after_edit):.0f}s after an edit "
      f"(median run {median(after_edit):.1f}s), {sum(reruns):.0f}s with no edit since the previous "
      f"run (median {median(reruns):.1f}s)")
hideable = sum(min(gap, run) for gap, run in zip(gaps, after_edit))
if waiting:
    print(f"a build started at each last edit could have hidden at most {hideable:.0f}s, "
          f"{100 * hideable / waiting:.0f}% of the waiting")
