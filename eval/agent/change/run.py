#!/usr/bin/env python3
"""Live cost and quality of Vexus on real change requests.

  run.py tasks --sets small,named,vague --arms plain,hook --rounds 3 --tag final
  run.py long --arms plain,hook --rounds 6 --tag final
  run.py questions --rounds 3 --tag final

Arms: plain (no vexus), hook (the prompt hook), mcp (the MCP server), both, and any
of those with -tests, where the agent may also run the project's test command.
Every arm in a run shares one time window, in random order, with caches warmed
first. Costs real tokens: about $0.05 a short session.
"""
import argparse
import json
import os
import queue
import random
import shutil
import subprocess
import tempfile
import threading
import time
import uuid

import tasks as T

HERE = os.path.dirname(os.path.abspath(__file__))
WORK = os.environ.get("VEXUS_EVAL_WORK", os.path.expanduser("~/.cache/vexus-eval"))
BIN = shutil.which(os.environ.get("VEXUS_BIN", "vexus"))
os.environ["PATH"] = os.path.dirname(BIN) + os.pathsep + os.environ["PATH"]
MODEL = os.environ.get("VEXUS_AGENT_MODEL", "claude-sonnet-5-5")
OUT = f"{WORK}/runs"
FULL = f"{WORK}/real/nest-full"
LOCK = threading.Lock()
TEST_COMMAND = "Bash(./node_modules/.bin/vitest:*)"
NO_MCP = '{"mcpServers":{}}'
VEXUS_MCP = json.dumps({"mcpServers": {"vexus": {"command": "vexus", "args": ["serve", "."]}}})


def command(prompt, arm, tools, session=None, first=True):
    name, _, extra = arm.partition("-")
    c = ["claude", "-p", prompt, "--setting-sources", "project", "--strict-mcp-config", "--mcp-config", VEXUS_MCP if name in ("mcp", "both") else NO_MCP,
         "--disable-slash-commands", "--tools", tools + (",Bash" if extra == "tests" else ""), "--permission-mode", "acceptEdits",
         "--model", MODEL, "--max-budget-usd", "4", "--output-format", "stream-json", "--verbose", "--include-hook-events"]
    allowed = ["mcp__vexus__*"] if name in ("mcp", "both") else []
    allowed += [TEST_COMMAND] if extra == "tests" else []
    if allowed:
        c += ["--allowedTools", ",".join(allowed)]
    if session is None:
        return c + ["--no-session-persistence"]
    return c + (["--session-id", session] if first else ["--resume", session])


def copy_repo(source, dest, arm):
    subprocess.run(["cp", "-cRp", source, dest], check=True)
    name = arm.partition("-")[0]
    if name == "plain":
        shutil.rmtree(f"{dest}/.vexus", ignore_errors=True)
    if name in ("hook", "both"):
        subprocess.run(["vexus", "init", "--agent", "claude-code", "."], cwd=dest, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)


def measure(path):
    messages, result = [], None
    for line in open(path, errors="replace"):
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if m.get("type") == "assistant":
            messages.append(m["message"])
        elif m.get("type") == "result":
            result = m
    if not result or not messages:
        return {}
    u = messages[-1].get("usage", {})
    names = [c["name"] for m in messages for c in m.get("content", []) if c.get("type") == "tool_use"]
    return {"calls": len(names), "context": u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0),
            "cost": result.get("total_cost_usd"), "ms": result.get("duration_ms"), "subtype": result.get("subtype"), "answer": result.get("result", ""),
            "tools": {n: names.count(n) for n in sorted(set(names))}}


def record(tag, row):
    row.pop("answer", None)
    with LOCK, open(f"{OUT}/{tag}.jsonl", "a") as f:
        f.write(json.dumps(row) + "\n")


def pool(jobs, workers, work):
    q = queue.Queue()
    for j in jobs:
        q.put(j)

    def worker():
        while True:
            try:
                j = q.get_nowait()
            except queue.Empty:
                return
            try:
                work(*j)
            except Exception as e:
                print("ERR", repr(e)[:200], flush=True)

    threads = [threading.Thread(target=worker) for _ in range(workers)]
    [t.start() for t in threads]
    [t.join() for t in threads]


def warm(arms, tools):
    d = tempfile.mkdtemp()
    for arm in {a.partition("-")[0] for a in arms}:
        subprocess.run(command("reply ok", arm, tools), cwd=d, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=120)


def real_spec_passes(path, t):
    for spec in t["spec"]:
        done = subprocess.run(["git", "show", f"{t['merge']}:{spec}"], cwd=FULL, capture_output=True)
        os.makedirs(os.path.dirname(f"{path}/{spec}"), exist_ok=True)
        open(f"{path}/{spec}", "wb").write(done.stdout)
    link = f"{path}/node_modules"
    if not os.path.lexists(link):
        os.symlink(f"{FULL}/node_modules", link)
    try:
        out = subprocess.run("./node_modules/.bin/vitest run " + " ".join(t["spec"]) + " 2>&1 | tail -25", shell=True, cwd=path, capture_output=True, text=True, timeout=240).stdout
    except subprocess.TimeoutExpired:
        return False
    return "Tests " in out and " failed" not in out.split("Tests ")[-1].split("\n")[0] and "FAIL" not in out


def load_tasks(sets):
    out = []
    if "small" in sets:
        out += [dict(set="small", id=k, dir=f"{WORK}/base/express", repo="express", prompt=v) for k, v in T.EXPRESS.items()]
        out += [dict(set="small", id=k, dir=f"{WORK}/base/nest", repo="nest", prompt=v) for k, v in T.NEST.items()]
    if {"named", "vague"} & set(sets):
        for t in json.load(open(f"{WORK}/real/tasks.json")):
            d = dict(dir=f"{WORK}/real/tasks/{t['n']}", id=str(t["n"]), real=t)
            if "named" in sets:
                out.append(dict(d, set="named", prompt="Fix this GitHub issue in this repository.\n\n" + t["prompt"]))
            if "vague" in sets:
                out.append(dict(d, set="vague", prompt="Fix this problem in this repository.\n\n" + t["vague"]))
    if "large" in sets:
        for t in json.load(open(f"{WORK}/large/tasks.json")):
            out.append(dict(set="large", id=str(t["n"]), dir=f"{WORK}/large/tasks/{t['n']}", prompt="Fix this GitHub issue in this repository.\n\n" + t["prompt"], src=t["src"]))
    return out


def run_task(rnd, t, arm, tag):
    name = f"r{rnd}-{t['set']}-{t['id']}-{arm}"
    path = f"{OUT}/{tag}/{name}"
    copy_repo(t["dir"], path, arm)
    if arm.endswith("-tests") and "real" in t:
        os.symlink(f"{FULL}/node_modules", f"{path}/node_modules")
    prompt = t["prompt"]
    if arm.endswith("-tests") and "real" in t:
        prompt += "\n\nRun tests with `./node_modules/.bin/vitest run <spec file>`."
    timed_out = False
    with open(f"{path}.jsonl", "w") as f:
        try:
            subprocess.run(command(prompt, arm, "Read,Grep,Glob,Edit,Write"), cwd=path, stdin=subprocess.DEVNULL, stdout=f, stderr=subprocess.DEVNULL, timeout=600)
        except subprocess.TimeoutExpired:
            timed_out = True
    row = measure(f"{path}.jsonl")
    files = T.changed_files(path)
    if timed_out:
        passed, right = False, False
    elif t["set"] == "small":
        passed, right = (T.passes_express if t["id"][0] == "E" else T.passes_nest)(path, t["id"]), None
    elif t["set"] == "large":
        right = set(t["src"]) <= set(files)
        passed = right
    else:
        right = set(t["real"]["src"]) <= set(files)
        passed = real_spec_passes(path, t["real"])
    row.update(round=rnd, set=t["set"], task=t["id"], arm=arm, timed_out=timed_out, passed=bool(passed), right_files=right)
    record(tag, row)
    print(time.strftime("%H:%M:%S"), name, row.get("cost"), row.get("calls"), passed, flush=True)
    shutil.rmtree(f"{path}/.vexus", ignore_errors=True)


def run_long(rnd, repo, prompts, arm, tag):
    """Every short task of a repo, one prompt at a time, in one session."""
    path = f"{OUT}/{tag}/r{rnd}-{repo}-{arm}"
    copy_repo(f"{WORK}/base/{repo}", path, arm)
    session = str(uuid.uuid4())
    order = list(prompts)
    random.Random(rnd).shuffle(order)
    cost = calls = 0
    context = 0
    started = time.time()
    for i, task in enumerate(order):
        with open(f"{path}-{i}.jsonl", "w") as f:
            subprocess.run(command(prompts[task], arm, "Read,Grep,Glob,Edit,Write", session, i == 0), cwd=path, stdin=subprocess.DEVNULL, stdout=f, stderr=subprocess.DEVNULL, timeout=600)
        m = measure(f"{path}-{i}.jsonl")
        # total_cost_usd is cumulative across --resume, so the last one is the session's.
        cost = m.get("cost") or cost
        calls += m.get("calls", 0)
        context = m.get("context") or context
    check = T.passes_express if repo == "express" else T.passes_nest
    passed = sum(bool(check(path, task)) for task in order)
    record(tag, dict(round=rnd, set="long", task=repo, arm=arm, cost=cost, calls=calls, context=context, ms=(time.time() - started) * 1000, passed=passed, of=len(order)))
    print(time.strftime("%H:%M:%S"), rnd, repo, arm, round(cost, 3), calls, f"{passed}/{len(order)}", flush=True)


def run_question(rnd, repo, task, question, names, arm, tag):
    path = f"{OUT}/{tag}/r{rnd}-{task}-{arm}"
    copy_repo(f"{WORK}/base/{repo}", path, arm)
    with open(f"{path}.jsonl", "w") as f:
        subprocess.run(command(question, arm, "Read,Grep,Glob"), cwd=path, stdin=subprocess.DEVNULL, stdout=f, stderr=subprocess.DEVNULL, timeout=300)
    row = measure(f"{path}.jsonl")
    answer = (row.get("answer") or "").lower()
    row.update(round=rnd, set="questions", task=task, arm=arm, covered=sum(n.lower() in answer for n in names), of=len(names), passed=sum(n.lower() in answer for n in names))
    record(tag, row)
    print(time.strftime("%H:%M:%S"), task, arm, row.get("cost"), row.get("calls"), f"{row['covered']}/{row['of']}", flush=True)
    shutil.rmtree(path, ignore_errors=True)


def questions():
    agent = os.path.dirname(HERE)
    out = []
    for repo, files in (("express", ["tasks-express.txt", "tasks-exact-express.txt"]), ("nest", ["tasks-nest.txt", "tasks-exact-nest.txt"])):
        for name in files:
            for i, line in enumerate(open(f"{agent}/{name}")):
                if "|" in line:
                    q, names = line.split("|", 1)
                    out.append((repo, f"{name[6:-4]}{i}", q.strip(), [n.strip() for n in names.split(",")]))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["tasks", "long", "questions"])
    ap.add_argument("--sets", default="small,named,vague")
    ap.add_argument("--arms", default="plain,hook")
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--workers", type=int, default=12)
    ap.add_argument("--tag", default="run")
    ap.add_argument("--only", default="", help="comma-separated set:id, for a quick check")
    a = ap.parse_args()
    arms = a.arms.split(",")
    os.makedirs(f"{OUT}/{a.tag}", exist_ok=True)
    jobs = []
    if a.mode == "tasks":
        warm(arms, "Read,Grep,Glob,Edit,Write")
        for rnd in range(1, a.rounds + 1):
            chosen = [t for t in load_tasks(a.sets.split(",")) if not a.only or f"{t['set']}:{t['id']}" in a.only.split(",")]
            batch = [(rnd, t, arm, a.tag) for t in chosen for arm in arms]
            random.shuffle(batch)
            jobs += batch
        pool(jobs, a.workers, run_task)
    elif a.mode == "long":
        warm(arms, "Read,Grep,Glob,Edit,Write")
        jobs = [(rnd, repo, prompts, arm, a.tag) for rnd in range(1, a.rounds + 1) for repo, prompts in (("express", T.EXPRESS), ("nest", T.NEST)) for arm in arms]
        random.shuffle(jobs)
        pool(jobs, a.workers, run_long)
    else:
        warm(arms, "Read,Grep,Glob")
        for rnd in range(1, a.rounds + 1):
            batch = [(rnd, r, t, q, n, arm, a.tag) for r, t, q, n in questions() for arm in arms]
            random.shuffle(batch)
            jobs += batch
        pool(jobs, a.workers, run_question)
    print("finished", time.strftime("%H:%M:%S"), flush=True)


if __name__ == "__main__":
    main()
