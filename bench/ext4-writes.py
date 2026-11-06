"""Reads ext4-writes.sh's traces: how a build touched the inodes a hardlinked build directory shared
with the golden's copy, by kind of file, following each inode through renames and links; and which
of the copy's files changed.

Every name under W starts as a name of a shared inode. A rename carries the inode to its new name
(mold renames an existing output aside, then writes it); a rename over a name, or an unlink, drops
that name; a link adds one. Opening a name of a shared inode for writing, truncating it, or
writing, mapping writable or extending a descriptor of one, reaches the shared inode, whatever its
name by then. The traces show descriptors by path (`strace -y`), as resolved when the call ran.

Usage: ext4-writes.py <W> <W.before> <G.manifest> <C.manifest> <strace log>...
"""

import re
import sys
from collections import Counter, defaultdict

W, BEFORE, G_MANIFEST, C_MANIFEST, *TRACES = sys.argv[1:]
W = W.rstrip("/") + "/"

LINE = re.compile(r"^(\d+) +([\d.]+) (\w+)\((.*)\) += (-?\d+|\?|0x[0-9a-f]+)(?: ?<[^>]*>)?(?: (\w+) \(.*\))?$")
UNFINISHED = re.compile(r"^(\d+) +([\d.]+) (\w+)\((.*) <unfinished \.\.\.>$")
RESUMED = re.compile(r"^(\d+) +([\d.]+) <\.\.\. (\w+) resumed>(.*)\) += (-?\d+|\?|0x[0-9a-f]+)")
STRING = re.compile(r'"((?:[^"\\]|\\.)*)"')
FD_PATH = re.compile(r"^-?\d+<(.*?)(?: \(deleted\))?>$")

# What Agent Hearth hardlinks (`adopt::linkable`): rustc's outputs, cargo's links to them, sessions.
LINKED_KINDS = ("deps/ program", "deps/*.rlib", "deps/*.so", "deps/*.a", "uplifted artifact",
                "build script program", "incremental session file")


def kind(path):
    """A coarse class for a path relative to the build directory."""
    parts = path.split("/")
    name = parts[-1]
    if "incremental" in parts:
        if name.endswith(".lock"):
            return "incremental lock"
        if name.endswith(".rmeta"):
            return "incremental metadata"
        return "incremental session file"
    if ".fingerprint" in parts:
        return "fingerprint"
    if "build" in parts:
        i = parts.index("build")
        if len(parts) > i + 2 and parts[i + 2] == "out":
            return "build script out/"
        if name.startswith("build-script-") or (name.startswith("build_script_") and "." not in name):
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
    if len(parts) <= 2 and name in (".rustc_info.json", "CACHEDIR.TAG"):
        return name
    if len(parts) <= 2 and name.startswith(".cargo-"):
        return "cargo lock (" + name + ")"
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

# Each current name (absolute) of a shared inode, and the original name (relative to W) it had.
alias = {W + rel: rel for rel in shared}
first = {}                      # original rel -> how it was first touched
reached = defaultdict(Counter)  # original rel -> how the shared inode was reached, and how often
renamed_aside = Counter()       # kind -> shared inodes renamed to another name


def fd_path(arg):
    m = FD_PATH.match(arg.strip())
    return m.group(1) if m else None


def note_first(rel, how):
    first.setdefault(rel, how)


def reach(path, how):
    rel = alias.get(path)
    if rel is not None:
        note_first(rel, how)
        reached[rel][how] += 1


for t, call, args, result in events:
    if result == "?" or result.startswith("-"):
        continue
    strings = STRING.findall(args)
    fields = args.split(", ")
    if call in ("open", "openat", "creat"):
        if not strings:
            continue
        path = strings[0]
        flags = fields[2 if call == "openat" else 1] if len(fields) > 1 else ""
        if call == "creat" or "O_WRONLY" in flags or "O_RDWR" in flags:
            if "O_EXCL" in flags:
                continue
            reach(path, "opened for writing, O_TRUNC" if (call == "creat" or "O_TRUNC" in flags)
                  else "opened for writing, no O_TRUNC")
    elif call in ("rename", "renameat", "renameat2"):
        if len(strings) >= 2:
            src, dst = strings[0], strings[1]
            if dst in alias:
                note_first(alias.pop(dst), "renamed over")
            if src in alias:
                rel = alias.pop(src)
                alias[dst] = rel
                note_first(rel, "renamed aside")
                renamed_aside[kind(rel)] += 1
    elif call in ("link", "linkat"):
        if len(strings) >= 2 and strings[0] in alias:
            alias[strings[1]] = alias[strings[0]]
    elif call in ("unlink", "unlinkat"):
        if strings and strings[-1] in alias:
            note_first(alias.pop(strings[-1]), "unlinked")
    elif call == "truncate":
        if strings:
            reach(strings[0], "truncated")
    elif call in ("ftruncate", "fallocate"):
        path = fd_path(fields[0])
        if path:
            reach(path, "truncated" if call == "ftruncate" else "fallocated")
    elif call in ("write", "pwrite64", "writev", "pwritev", "pwritev2"):
        path = fd_path(fields[0])
        if path:
            reach(path, "written")
    elif call == "mmap":
        # mmap(addr, length, prot, flags, fd, offset)
        if len(fields) >= 5 and "PROT_WRITE" in fields[2] and "MAP_SHARED" in fields[3]:
            path = fd_path(fields[4])
            if path:
                reach(path, "mapped shared and writable")
    elif call in ("copy_file_range", "sendfile", "sendfile64"):
        out = fields[2] if call == "copy_file_range" else fields[0]
        path = fd_path(out)
        if path:
            reach(path, "written")
    elif call in ("utimensat", "utimes", "futimesat"):
        target = strings[0] if strings else fd_path(fields[0])
        if target:
            reach(target, "times set")
    elif call in ("chmod", "fchmodat"):
        if strings:
            reach(strings[-1], "mode set")
    elif call == "fchmod":
        path = fd_path(fields[0])
        if path:
            reach(path, "mode set")

by_kind = defaultdict(Counter)
for rel, how in first.items():
    by_kind[kind(rel)][how] += 1
print(f"  files shared with the golden's copy: {len(shared)}; touched by the build: {len(first)}")
for k in sorted(by_kind):
    print(f"  {k:28} first touched: " + ", ".join(f"{how} {n}" for how, n in by_kind[k].most_common()))
if renamed_aside:
    print("  shared inodes renamed aside, then followed: " + ", ".join(f"{k} {n}" for k, n in renamed_aside.most_common()))
in_place = defaultdict(Counter)
for rel, hows in reached.items():
    for how in hows:
        in_place[kind(rel)][how] += 1
if in_place:
    print("  shared inodes written or changed through any name (files, by how):")
    for k in sorted(in_place):
        mark = "  <- hardlinked by Agent Hearth" if k in LINKED_KINDS else ""
        print(f"    {k:28} " + ", ".join(f"{how} {n}" for how, n in in_place[k].most_common()) + mark)
else:
    print("  shared inodes written or changed through any name: none")
linked_reached = sorted(rel for rel in reached if kind(rel) in LINKED_KINDS)
print(f"  of what Agent Hearth hardlinks, reached in place: {len(linked_reached)}"
      + (" (" + ", ".join(linked_reached[:5]) + (", ..." if len(linked_reached) > 5 else "") + ")" if linked_reached else ""))

g, c = manifest(G_MANIFEST), manifest(C_MANIFEST)
content = Counter(kind(r) for r in g if r in c and c[r][1] != g[r][1])
mtime = Counter(kind(r) for r in g if r in c and c[r][1] == g[r][1] and c[r][0] != g[r][0])
missing = sum(1 for r in g if r not in c)
print(f"  golden's copy afterwards: {sum(content.values())} files with other content"
      + (" (" + ", ".join(f"{k} {n}" for k, n in content.most_common()) + ")" if content else "")
      + f", {sum(mtime.values())} with only another mtime"
      + (" (" + ", ".join(f"{k} {n}" for k, n in mtime.most_common()) + ")" if mtime else "")
      + f", {missing} missing")
linked_changed = sorted(r for r in g if r in c and kind(r) in LINKED_KINDS and c[r] != g[r])
print(f"  of what Agent Hearth hardlinks, changed in the golden's copy: {len(linked_changed)}"
      + (" (" + ", ".join(linked_changed[:5]) + (", ..." if len(linked_changed) > 5 else "") + ")" if linked_changed else ""))
