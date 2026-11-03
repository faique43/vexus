#!/usr/bin/env python3
"""Builds what run.py needs under $VEXUS_EVAL_WORK.

  setup.py repos    pinned Express and NestJS, seeded bugs, runtime deps
  setup.py real     NestJS at each bug's pre-fix commit, with its issue text
  setup.py index    index everything with $VEXUS_BIN
  setup.py large    a repo far bigger than NestJS, from recent real fixes

Needs git, gh, bun and node. `real` runs each maintainer spec before and after
the real fix, and a task only counts if it fails before and passes after.
"""
import concurrent.futures as cf
import json
import os
import shutil
import subprocess
import sys

import tasks as T

HERE = os.path.dirname(os.path.abspath(__file__))
WORK = os.environ.get("VEXUS_EVAL_WORK", os.path.expanduser("~/.cache/vexus-eval"))
BIN = os.environ.get("VEXUS_BIN", "vexus")
BASE, REAL, LARGE = f"{WORK}/base", f"{WORK}/real", f"{WORK}/large"
FULL = f"{REAL}/nest-full"
IGNORE = "node_modules\n.vexus\n.claude\nCLAUDE.md\n"


def sh(cmd, cwd=None, timeout=900):
    return subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True, timeout=timeout)


def fresh_git(path):
    """A clean one-commit repo, so `git status` shows only what the agent changed."""
    shutil.rmtree(f"{path}/.git", ignore_errors=True)
    if os.path.isfile(f"{path}/.git"):
        os.remove(f"{path}/.git")
    open(f"{path}/.gitignore", "a").write(IGNORE)
    sh("git init -q && git add -A && git -c user.email=e@e -c user.name=e commit -qm base", cwd=path)


def repos():
    os.makedirs(BASE, exist_ok=True)
    for repo, url, ref in (("express", "https://github.com/expressjs/express", T.EXPRESS_REF),
                           ("nest", "https://github.com/nestjs/nest", T.NEST_REF)):
        path = f"{BASE}/{repo}"
        if not os.path.isdir(path):
            sh(f"git clone -q --filter=blob:none {url} {path}")
            sh(f"git checkout -q {ref}", cwd=path)
        for rel, good, bad in T.SEEDS[repo]:
            text = open(f"{path}/{rel}").read()
            assert good in text or bad in text, rel
            open(f"{path}/{rel}", "w").write(text.replace(good, bad))
        if repo == "express":
            sh("bun install --production --ignore-scripts --no-save", cwd=path)
        fresh_git(path)


def spec_passes(path, specs):
    link = f"{path}/node_modules"
    if not os.path.lexists(link):
        os.symlink(f"{FULL}/node_modules", link)
    try:
        out = sh("./node_modules/.bin/vitest run " + " ".join(specs) + " 2>&1 | tail -25", cwd=path, timeout=240).stdout
        return "Tests " in out and " failed" not in out.split("Tests ")[-1].split("\n")[0] and "FAIL" not in out
    finally:
        os.unlink(link)


def show(rev, rel, dest):
    done = subprocess.run(["git", "show", f"{rev}:{rel}"], cwd=FULL, capture_output=True)
    if done.returncode == 0:
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        open(dest, "wb").write(done.stdout)
    return done.returncode == 0


def real():
    os.makedirs(f"{REAL}/tasks", exist_ok=True)
    if not os.path.isdir(FULL):
        sh(f"git clone -q --filter=blob:none https://github.com/nestjs/nest {FULL}")
        sh("bun install --ignore-scripts --no-save", cwd=FULL)
    specs = json.load(open(f"{HERE}/real_tasks.json"))
    valid = []
    for t in specs:
        path = f"{REAL}/tasks/{t['n']}"
        if not os.path.isdir(path):
            sh(f"git worktree add -q --detach {path} {t['base']}", cwd=FULL)
        for spec in t["spec"]:
            show(t["merge"], spec, f"{path}/{spec}")
        before = spec_passes(path, t["spec"])
        for src in t["src"]:
            show(t["merge"], src, f"{path}/{src}")
        after = spec_passes(path, t["spec"])
        sh("git checkout -q . && git clean -fdq", cwd=path)
        print(t["n"], "ok" if after and not before else "DROP", flush=True)
        if after and not before:
            issue = json.loads(sh(f"gh issue view {t['issue'][0]} -R nestjs/nest --json title,body").stdout)
            t["prompt"] = f"{issue['title']}\n\n{issue['body'][:3000]}"
            valid.append(t)
            fresh_git(path)
    sh("git worktree prune", cwd=FULL)
    json.dump(valid, open(f"{REAL}/tasks.json", "w"), indent=1)
    print(len(valid), "of", len(specs), "tasks valid")


def index_one(path, seed=None):
    if seed:
        shutil.rmtree(f"{path}/.vexus", ignore_errors=True)
        shutil.copytree(seed, f"{path}/.vexus")
    out = sh(f"{BIN} index . 2>/dev/null | tail -2", cwd=path, timeout=3000).stdout.strip().replace("\n", " | ")
    print(os.path.relpath(path, WORK), out, flush=True)


def index():
    for repo in ("express", "nest"):
        index_one(f"{BASE}/{repo}")
    tasks = [f"{REAL}/tasks/{t['n']}" for t in json.load(open(f"{REAL}/tasks.json"))]
    with cf.ThreadPoolExecutor(3) as pool:
        list(pool.map(lambda p: index_one(p, f"{BASE}/nest/.vexus"), tasks))


def large():
    """Recent vscode fixes with a linked issue, touching one to three source files.
    No spec here: the score is whether the agent changed the files the fix did."""
    os.makedirs(f"{LARGE}/tasks", exist_ok=True)
    clone = f"{LARGE}/vscode"
    repo = "microsoft/vscode"
    if not os.path.isdir(clone):
        sh(f"git clone -q --filter=blob:none --no-checkout https://github.com/{repo} {clone}", timeout=1800)
    fields = "number,title,files,mergeCommit,closingIssuesReferences,mergedAt"
    prs = json.loads(sh(f"gh pr list -R {repo} --state merged --limit 600 --search 'fix in:title' --json {fields}").stdout)
    keep = []
    for pr in prs:
        src = [f["path"] for f in pr["files"] if f["path"].endswith(".ts") and f["path"].startswith("src/") and ".test." not in f["path"] and "/test/" not in f["path"]]
        rest = [f["path"] for f in pr["files"] if f["path"] not in src and "/test/" not in f["path"]]
        if pr["closingIssuesReferences"] and 1 <= len(src) <= 3 and not rest and pr["mergeCommit"]:
            keep.append(dict(n=pr["number"], title=pr["title"], src=src, merge=pr["mergeCommit"]["oid"], issue=pr["closingIssuesReferences"][0]["number"]))
    keep = keep[:int(os.environ.get("VEXUS_EVAL_LARGE_TASKS", 12))]
    head = keep[0]["merge"]
    sh(f"git worktree add -q --detach {LARGE}/index-base {head}^1", cwd=clone, timeout=1800)
    index_one(f"{LARGE}/index-base")
    valid = []
    for t in keep:
        path = f"{LARGE}/tasks/{t['n']}"
        sh(f"git worktree add -q --detach {path} {t['merge']}^1", cwd=clone, timeout=900)
        issue = json.loads(sh(f"gh issue view {t['issue']} -R {repo} --json title,body").stdout or "{}")
        if not issue.get("body"):
            continue
        t["prompt"] = f"{issue['title']}\n\n{issue['body'][:3000]}"
        fresh_git(path)
        index_one(path, f"{LARGE}/index-base/.vexus")
        valid.append(t)
    json.dump(valid, open(f"{LARGE}/tasks.json", "w"), indent=1)
    print(len(valid), "large tasks")


if __name__ == "__main__":
    {"repos": repos, "real": real, "index": index, "large": large}[sys.argv[1]]()
