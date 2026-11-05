"""Reads ext4-writes.sh's traces: how a build touched the files a hardlinked build directory shared
with the golden's copy, by kind of file, and which of the copy's files changed.

Usage: ext4-writes.py <W> <W.before> <G.manifest> <C.manifest> <strace log>...
"""

import re
import sys
from collections import Counter, defaultdict

W, BEFORE, G_MANIFEST, C_MANIFEST, *TRACES = sys.argv[1:]
W = W.rstrip("/") + "/"

LINE = re.compile(r"^(\d+) +([\d.]+) (\w+)\((.*)\) += (-?\d+|\?)(?: <[^>]*>)?(?: (\w+) \(.*\))?$")
UNFINISHED = re.compile(r"^(\d+) +([\d.]+) (\w+)\((.*) <unfinished \.\.\.>$")
RESUMED = re.compile(r"^(\d+) +([\d.]+) <\.\.\. (\w+) resumed>(.*)\) += (-?\d+|\?)")
STRING = re.compile(r'"((?:[^"\\]|\\.)*)"')
FD_PATH = re.compile(r"^-?\d+<(.*)>$")


def kind(path):
    """A coarse class for a path relative to the build directory."""
    parts = path.split("/")
    name = parts[-1]
    if "incremental" in parts:
        if name.endswith(".lock"):
            return "incremental lock"
        return "incremental session file"
    if ".fingerprint" in parts:
        return "fingerprint"
    if "build" in parts:
        i = parts.index("build")
        if len(parts) > i + 2 and parts[i + 2] == "out":
            return "build script out/"
        if name.startswith("build-script-"):
            return "build script program"
        if name.endswith(".d"):
            return "build script dep-info"
        return "build script " + name
    if "deps" in parts:
        for ext in (".rlib", ".rmeta", ".so", ".a", ".d", ".o"):
            if name.endswith(ext):
                return "deps/*" + ext
        if "." not in name:
            return "deps/ program"
        return "deps/ other"
    if len(parts) <= 2 and name in (".cargo-lock", ".rustc_info.json", "CACHEDIR.TAG"):
        return name
    if len(parts) == 2 or (len(parts) == 3 and parts[1] == "examples"):
        return "uplifted " + ("dep-info" if name.endswith(".d") else "artifact")
    return "other"


def manifest(path):
    files = {}
    with open(path) as f:
        for line in f:
            rel, inode, mtime, size, digest = line.rstrip("\n").split("\t")
            files[rel] = (mtime, digest)
    return files


shared = {}
with open(BEFORE) as f:
    for line in f:
        rel, inode = line.rstrip("\n").split("\t")
        shared[rel] = inode

events = []  # (time, syscall, args, result)
for trace in TRACES:
    pending = {}
    try:
        lines = open(trace, errors="replace")
    except FileNotFoundError:
        continue
    for line in lines:
        line = line.rstrip("\n")
        m = UNFINISHED.match(line)
        if m:
            pending[m.group(1)] = (float(m.group(2)), m.group(3), m.group(4))
            continue
        m = RESUMED.match(line)
        if m and m.group(1) in pending:
            t, call, args = pending.pop(m.group(1))
            events.append((t, call, args + m.group(4), m.group(5)))
            continue
        m = LINE.match(line)
        if m:
            events.append((float(m.group(2)), m.group(3), m.group(4), m.group(5)))
events.sort(key=lambda e: e[0])


def rel(path):
    return path[len(W):] if path.startswith(W) else None


# What first happened to each file W shared with the copy: only the first operation can reach the
# shared inode through the path (a rename or unlink breaks the link).
first = {}
writes = Counter()  # (kind, how) for any write-open of a shared, still-linked path
gone = set()


def note(path, how):
    r = rel(path)
    if r is None or r not in shared or r in gone:
        return
    first.setdefault(r, how)
    if how.startswith(("rename", "unlink")):
        gone.add(r)
    else:
        writes[(kind(r), how)] += 1


for t, call, args, result in events:
    if result in ("?",) or result.startswith("-"):
        continue
    strings = STRING.findall(args)
    if call in ("open", "openat", "creat"):
        if not strings:
            continue
        path = strings[0]
        flags = args.split(",")[2 if call == "openat" else 1] if "," in args else ""
        if call == "creat" or "O_WRONLY" in flags or "O_RDWR" in flags:
            if "O_EXCL" in flags:
                continue
            how = "open for writing, O_TRUNC" if (call == "creat" or "O_TRUNC" in flags) else "open for writing, no O_TRUNC"
            note(path, how)
    elif call in ("rename", "renameat", "renameat2"):
        if len(strings) >= 2:
            note(strings[1], "renamed over")
    elif call in ("unlink", "unlinkat"):
        if strings:
            note(strings[-1], "unlinked")
    elif call in ("utimensat", "utimes", "futimesat"):
        target = strings[0] if strings else None
        if target is None or "NULL" in args.split(",")[1]:
            m = FD_PATH.match(args.split(",")[0].strip())
            target = m.group(1) if m else None
        if target:
            note(target, "times set")
    elif call in ("chmod", "fchmodat"):
        if strings:
            note(strings[-1], "mode set")
    elif call in ("fchmod", "ftruncate"):
        m = FD_PATH.match(args.split(",")[0].strip())
        if m:
            note(m.group(1), "mode set" if call == "fchmod" else "ftruncate")
    elif call == "truncate":
        if strings:
            note(strings[0], "ftruncate")

by_kind = defaultdict(Counter)
for r, how in first.items():
    by_kind[kind(r)][how] += 1
print(f"  files shared with the golden's copy: {len(shared)}; touched by the build: {len(first)}")
for k in sorted(by_kind):
    print(f"  {k:28} first touched: " + ", ".join(f"{how} {n}" for how, n in by_kind[k].most_common()))
in_place = Counter()
for (k, how), n in writes.items():
    in_place[k] += n
if in_place:
    print("  shared files written or changed through the link: " + ", ".join(f"{k} {n}" for k, n in in_place.most_common()))
else:
    print("  shared files written or changed through the link: none")

g, c = manifest(G_MANIFEST), manifest(C_MANIFEST)
content = Counter(kind(r) for r in g if r in c and c[r][1] != g[r][1])
mtime = Counter(kind(r) for r in g if r in c and c[r][1] == g[r][1] and c[r][0] != g[r][0])
missing = sum(1 for r in g if r not in c)
print(f"  golden's copy afterwards: {sum(content.values())} files with other content"
      + (" (" + ", ".join(f"{k} {n}" for k, n in content.most_common()) + ")" if content else "")
      + f", {sum(mtime.values())} with only another mtime"
      + (" (" + ", ".join(f"{k} {n}" for k, n in mtime.most_common()) + ")" if mtime else "")
      + f", {missing} missing")
for r in sorted(r for r in g if r in c and c[r][1] == g[r][1] and c[r][0] != g[r][0]
                and kind(r).startswith(("deps/", "incremental session", "uplifted artifact", "build script program")))[:10]:
    print(f"    mtime only: {r} ({g[r][0]} -> {c[r][0]})")
