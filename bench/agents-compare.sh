#!/usr/bin/env bash
# Summarises agents.sh outputs side by side, one line per run: when the last agent finished (and
# the last settle), the agents' times summed, own builds' median, the mean first build per agent,
# all own builds summed, and how many agents left clippy failing or a settle failed.
# Usage: agents-compare.sh <agents.sh output>...
set -uo pipefail
export LC_ALL=C
for f in "$@"; do
  log() { sed -n '/=== cargo log/,$p' "$f" | grep -a -v message-format; }
  done_=$(grep -a -oE '^=== [0-9]+ agents done after [0-9]+s, settles after [0-9]+s' "$f" | grep -oE '[0-9]+s' | tail -2 | paste -sd/)
  sum=$(grep -a -E '^a[0-9]+: [0-9]+s, exit' "$f" | awk '{gsub("s,", "", $2); t += $2} END {print t}')
  median=$(grep -a -oE '^own builds: [0-9]+, median [0-9.]+s' "$f" | grep -oE '[0-9.]+s$')
  first=$(log | awk '$1 ~ /^a[0-9]+$/ && !($1 in s) {t = $2; sub("s", "", t); s[$1] = t; x += t; n++} END {printf "%.0f", x / n}')
  total=$(log | awk '$1 ~ /^a[0-9]+$/ {t = $2; sub("s", "", t); x += t} END {printf "%.0f", x}')
  clippy=$(grep -a -E '^a[0-9]+: check:' "$f" | grep -a -c -E 'FAILED|[0-9]+ diagnostic')
  settle=$(grep -a -E '^a[0-9]+: check:' "$f" | grep -a -c 'settle: fail')
  printf '%-30s done/settled %s; agents summed %ss; own median %s; first build mean %ss; own builds summed %ss; clippy failing %s; settle failed %s\n' \
    "$(basename "$f")" "$done_" "$sum" "$median" "$first" "$total" "$clippy" "$settle"
done
