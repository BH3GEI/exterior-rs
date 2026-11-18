#!/usr/bin/env python3
"""Download the Alpine packages exterior needs (and everything they depend on) into ../vendor-apk,
from the same Alpine release as the Lazycat base image. apk checks their signatures at install time."""
import io, os, sys, tarfile, urllib.request
REL, ARCH = "v3.20", "x86_64"
WANT = ["openssh-client-default", "openssh-keygen", "sshpass", "git", "curl", "tzdata"]
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "vendor-apk")
idx = {}; provides = {}
for repo in ("main", "community"):
    raw = urllib.request.urlopen(f"https://dl-cdn.alpinelinux.org/alpine/{REL}/{repo}/{ARCH}/APKINDEX.tar.gz", timeout=60).read()
    text = tarfile.open(fileobj=io.BytesIO(raw)).extractfile("APKINDEX").read().decode()
    for block in text.split("\n\n"):
        f = {}
        for line in block.splitlines():
            if ":" in line: k, v = line.split(":", 1); f.setdefault(k, v)
        if "P" not in f: continue
        f["repo"] = repo
        idx[f["P"]] = f
        for p in f.get("p", "").split():
            provides.setdefault(p.split("=")[0], f["P"])
def resolve(dep):
    dep = dep.lstrip("!")
    for op in ("<=", ">=", "~", "=", "<", ">"):
        dep = dep.split(op)[0]
    return dep if dep in idx else provides.get(dep)
seen, todo = set(), list(WANT)
while todo:
    n = resolve(todo.pop())
    if not n or n in seen: continue
    seen.add(n)
    todo += [d for d in idx[n].get("D", "").split() if not d.startswith("!")]
os.makedirs(OUT, exist_ok=True)
for n in sorted(seen):
    f = idx[n]; name = f"{n}-{f['V']}.apk"; dst = os.path.join(OUT, name)
    if not os.path.exists(dst):
        urllib.request.urlretrieve(f"https://dl-cdn.alpinelinux.org/alpine/{REL}/{f['repo']}/{ARCH}/{name}", dst)
    print(name)
