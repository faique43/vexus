#!/usr/bin/env python3
"""Scores the prompt hook's hints offline, against the files each task's fix
changed. Runs `vexus hook prompt` on every eval prompt, so it costs no tokens
and takes under a minute: a ranking change gets a number here before it gets
a live run.

  locate_report.py --tag main
  locate_report.py --tag keyword --env VEXUS_EMBEDDER=none --baseline main
  VEXUS_BIN=$PWD/target/release/vexus locate_report.py --tag branch --baseline main

file@k: a fix file is among the first k distinct files hinted. Precision: the
share of shown hints that point into a fix file; every other hint can cost the
agent a Read. Silent: the hook said nothing, by design on short replies and on
long prompts that already name code.
"""
import argparse
import json
import os
import re
import subprocess
import time

import run as R
import tasks as T

LINE = re.compile(r"^(\S+?)(?::\d+-\d+)? ")


def truth(t):
    if t["set"] == "small":
        return T.FILES[t["id"]]
    return t["real"]["src"] if "real" in t else t["src"]


def hints(t, env):
    # A marker this fresh tells the hook not to start a background `vexus index`
    # in the shared task directory.
    os.makedirs(f"{t['dir']}/.vexus", exist_ok=True)
    open(f"{t['dir']}/.vexus/refreshed", "w").close()
    payload = json.dumps({"prompt": t["prompt"], "cwd": t["dir"], "hook_event_name": "UserPromptSubmit"})
    started = time.time()
    out = subprocess.run([R.BIN, "hook", "prompt"], input=payload, capture_output=True, text=True, env=env, timeout=60).stdout
    ms = (time.time() - started) * 1000
    if not out.strip():
        return [], ms
    text = json.loads(out)["hookSpecificOutput"]["additionalContext"]
    return [m.group(1) for m in map(LINE.match, text.splitlines()[1:]) if m], ms


def score(t, shown, ms):
    files = list(dict.fromkeys(shown))
    want = set(truth(t))
    rank = next((i + 1 for i, f in enumerate(files) if f in want), None)
    return dict(set=t["set"], task=t["id"], chars=len(t["prompt"]), shown=shown, want=sorted(want), rank=rank,
                right=sum(f in want for f in shown), ms=round(ms))


def summary(rows):
    n = len(rows)
    shown = sum(len(r["shown"]) for r in rows)
    ms = sorted(r["ms"] for r in rows)
    return dict(n=n, silent=sum(not r["shown"] for r in rows), at1=sum(r["rank"] == 1 for r in rows),
                at3=sum(bool(r["rank"]) and r["rank"] <= 3 for r in rows), shown=shown,
                precision=sum(r["right"] for r in rows) / shown if shown else 0.0, p50=ms[n // 2], p95=ms[int(n * 0.95)])


def table(rows, base=None):
    print(f"  {'set':9} {'n':>3} {'silent':>6} {'file@1':>6} {'file@3':>6} {'hints':>5} {'precision':>9} {'ms p50/p95':>11}")
    for label in sorted({r["set"] for r in rows}) + ["ALL"]:
        s = summary([r for r in rows if label in ("ALL", r["set"])])
        line = f"  {label:9} {s['n']:3} {s['silent']:6} {s['at1']:6} {s['at3']:6} {s['shown']:5} {s['precision']:9.2f} {s['p50']:5}/{s['p95']}"
        if base:
            b = summary([r for r in base if label in ("ALL", r["set"])])
            line += f"   vs base: file@1 {s['at1'] - b['at1']:+d}, file@3 {s['at3'] - b['at3']:+d}, precision {s['precision'] - b['precision']:+.2f}, silent {s['silent'] - b['silent']:+d}"
        print(line)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sets", default="small,named,vague")
    ap.add_argument("--tag", default="locate")
    ap.add_argument("--env", action="append", default=[], help="KEY=VALUE for the hook, repeatable")
    ap.add_argument("--baseline", help="an earlier tag to compare with")
    a = ap.parse_args()
    env = dict(os.environ, **dict(kv.split("=", 1) for kv in a.env))
    rows = [score(t, *hints(t, env)) for t in R.load_tasks(a.sets.split(","))]
    os.makedirs(f"{R.WORK}/locate", exist_ok=True)
    with open(f"{R.WORK}/locate/{a.tag}.jsonl", "w") as f:
        f.writelines(json.dumps(r) + "\n" for r in rows)
    base = None
    if a.baseline:
        base = [json.loads(line) for line in open(f"{R.WORK}/locate/{a.baseline}.jsonl")]
    print(f"{a.tag}: {os.path.realpath(R.BIN)} {' '.join(a.env)}")
    table(rows, base)
    if base:
        before = {(r["set"], r["task"]): r for r in base}
        for r in rows:
            b = before.get((r["set"], r["task"]))
            if b and (b["rank"], b["right"]) != (r["rank"], r["right"]):
                print(f"  {r['set']}:{r['task']} rank {b['rank']} -> {r['rank']}, right hints {b['right']} -> {r['right']}")


if __name__ == "__main__":
    main()
