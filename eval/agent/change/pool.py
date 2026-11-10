#!/usr/bin/env python3
"""Pools several runs. Each arm is compared with plain inside its own run, since
costs drift between runs, and intervals resample tasks across all of them.
Usage: pool.py TAG [TAG ...]"""
import collections
import json
import os
import random
import sys

WORK = os.environ.get("VEXUS_EVAL_WORK", os.path.expanduser("~/.cache/vexus-eval"))
runs = {}
for tag in sys.argv[1:]:
    rows = [json.loads(line) for line in open(f"{WORK}/runs/{tag}.jsonl")]
    by = collections.defaultdict(lambda: collections.defaultdict(list))
    for r in rows:
        if r.get("cost") is not None:
            by[r["arm"]][(r["set"], r["task"])].append(r)
    runs[tag] = by


def mean(v, f):
    return sum(float(x.get(f) or 0) for x in v) / len(v)


def pairs(arm, sets):
    """(plain cost, arm cost, plain passed, arm passed) per run and task."""
    out = []
    for by in runs.values():
        if arm not in by:
            continue
        for k in by["plain"]:
            if k in by[arm] and (sets is None or k[0] in sets):
                out.append((mean(by["plain"][k], "cost"), mean(by[arm][k], "cost"),
                            mean(by["plain"][k], "passed"), mean(by[arm][k], "passed"), k))
    return out


def report(arm, sets, label):
    ps = pairs(arm, sets)
    if not ps:
        return
    change = (sum(p[1] for p in ps) / sum(p[0] for p in ps) - 1) * 100
    tasks = sorted({p[4] for p in ps})
    by_task = collections.defaultdict(list)
    for p in ps:
        by_task[p[4]].append(p)
    random.seed(11)
    d = []
    for _ in range(4000):
        pick = [x for t in random.choices(tasks, k=len(tasks)) for x in by_task[t]]
        d.append((sum(p[1] for p in pick) / sum(p[0] for p in pick) - 1) * 100)
    d.sort()
    passed = (sum(p[3] for p in ps) - sum(p[2] for p in ps)) / len(ps) * 100
    print(f"  {label:10} {arm:6} {change:+5.1f}% [{d[100]:+.0f},{d[3899]:+.0f}]  n={len(ps):3}  pass {passed:+.1f} pts")


arms = sorted({a for by in runs.values() for a in by if a != "plain"})
for label, sets in (("changes", {"small", "named", "vague"}), ("small", {"small"}), ("named", {"named"}), ("vague", {"vague"}), ("questions", {"questions"})):
    for arm in arms:
        report(arm, sets, label)
