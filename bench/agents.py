"""Reads agents.sh's logs and prints how much the agents' builds overlapped.

agents.log: "<agent> <start> <end> <exit>" per agent. cargo.log: "<agent> <start> <end> <exit>
<args>" per cargo run. A build is a cargo run that compiles; `metadata`, `fmt` and `--version`
are not. Builds are the agent's own, the Stop hook's `check` (clippy with JSON messages), the
background `settle` it starts (`test --workspace --no-fail-fast` with JSON messages), or a warm's
(its agent's name with `-warm`).
"""

import sys
from collections import defaultdict

BUILDS = {"build", "check", "clippy", "test", "nextest", "run", "doc", "bench"}


def kind(run):
    name, args = run[0], run[4]
    if name.endswith("-warm"):
        return "warm"
    if "--message-format=json" not in args:
        return "own"
    return "settle" if "--no-fail-fast" in args else "check"


agents = {}
for line in open(sys.argv[1]):
    name, start, end, rc = line.split()
    agents[name] = (float(start), float(end), int(rc))
runs = []
for line in open(sys.argv[2]):
    name, start, end, rc, *args = line.split()
    sub = next((a for a in args if not a.startswith("-")), "")
    if sub in BUILDS:
        runs.append((name, float(start), float(end), int(rc), " ".join(args)))
if not agents:
    sys.exit("no agents finished")

t0 = min(a[0] for a in agents.values())
done = max(a[1] for a in agents.values())
t1 = max([done] + [r[2] for r in runs])
print(f"=== {len(agents)} agents done after {done - t0:.0f}s, settles after {t1 - t0:.0f}s; "
      f"{len(runs)} builds")
for name in sorted(agents, key=lambda n: int(n[1:])):
    start, end, rc = agents[name]
    parts = []
    for k in ("own", "check", "settle"):
        mine = [r for r in runs if r[0] == name and kind(r) == k]
        if mine:
            secs = sum(r[2] - r[1] for r in mine)
            failed = sum(1 for r in mine if r[3] != 0)
            parts.append(f"{k} {len(mine)} ({secs:.0f}s{f', {failed} failed' if failed else ''})")
    own = sum(r[2] - r[1] for r in runs if r[0] == name and kind(r) != "settle")
    print(f"{name}: {end - start:.0f}s, exit {rc}; builds: {', '.join(parts)}; "
          f"{100 * own / (end - start):.0f}% of its time waiting on its own builds and checks")


def overlap(lo, hi, which):
    """Share of [lo, hi] with k builds of `which` running, and their mean while any runs."""
    chosen = [r for r in runs if which(r)]
    events = sorted([(max(r[1], lo), 1) for r in chosen if r[1] < hi]
                    + [(min(r[2], hi), -1) for r in chosen if r[1] < hi])
    time_at = defaultdict(float)
    k, last = 0, lo
    for t, step in events:
        time_at[k] += t - last
        k, last = k + step, t
    time_at[k] += hi - last
    shares = ", ".join(f"{k}: {100 * time_at[k] / (hi - lo):.0f}%" for k in sorted(time_at))
    busy = (hi - lo) - time_at[0]
    mean = sum(k * v for k, v in time_at.items()) / busy if busy else 0
    return f"{shares}; while any runs, {mean:.2f} at once on average, at most {max(time_at)}"


print("while agents work, builds running at once (own and check), share of the time: "
      + overlap(t0, done, lambda r: kind(r) in ("own", "check")))
print("over the whole run, every build including settles: " + overlap(t0, t1, lambda r: True))
for k in ("own", "check", "settle", "warm"):
    mine = sorted((r for r in runs if kind(r) == k), key=lambda r: r[2] - r[1])
    if mine:
        mid = mine[len(mine) // 2]
        print(f"{k} builds: {len(mine)}, median {mid[2] - mid[1]:.1f}s, "
              f"longest {mine[-1][2] - mine[-1][1]:.1f}s")
