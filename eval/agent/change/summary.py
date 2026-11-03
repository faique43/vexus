#!/usr/bin/env python3
"""Compares arms task by task. Intervals resample tasks, since rounds of one
task aren't independent. Usage: summary.py TAG [BASELINE_ARM]"""
import collections
import json
import os
import random
import sys

WORK = os.environ.get("VEXUS_EVAL_WORK", os.path.expanduser("~/.cache/vexus-eval"))
tag = sys.argv[1]
base = sys.argv[2] if len(sys.argv) > 2 else "plain"
rows = [json.loads(line) for line in open(f"{WORK}/runs/{tag}.jsonl")]
rows = [r for r in rows if r.get("cost") is not None]
print("sessions", len(rows), "total $%.2f" % sum(r["cost"] for r in rows), "timeouts", sum(bool(r.get("timed_out")) for r in rows))
A = collections.defaultdict(lambda: collections.defaultdict(list))
for r in rows:
    A[r["arm"]][(r["set"], r["task"])].append(r)
keys = sorted(set.intersection(*[set(A[a]) for a in A]))


def mean(v, f):
    return sum(float(x.get(f) or 0) for x in v) / len(v)


def total(arm, f, ks):
    return sum(mean(A[arm][k], f) for k in ks)


def change(arm, f, ks):
    return (total(arm, f, ks) / total(base, f, ks) - 1) * 100


def interval(arm, f, ks):
    random.seed(5)
    d = []
    for _ in range(3000):
        s = random.choices(ks, k=len(ks))
        if f == "passed":
            d.append((total(arm, f, s) - total(base, f, s)) / len(s) * 100)
        else:
            d.append(change(arm, f, s))
    d.sort()
    return d[75], d[2924]


sets = sorted({k[0] for k in keys})
for label, ks in [("ALL", keys)] * (len(sets) > 1) + [(s, [k for k in keys if k[0] == s]) for s in sets]:
    n = sum(len(A[base][k]) for k in ks)
    print(f"\n{label} ({len(ks)} tasks)\n  {base:12} ${total(base, 'cost', ks):.3f}  calls {total(base, 'calls', ks):6.1f}  ctx {total(base, 'context', ks) / 1000:6.1f}k  time {total(base, 'ms', ks) / 1000:5.0f}s  passed {sum(x['passed'] for k in ks for x in A[base][k])} of {n} sessions")
    for arm in [a for a in A if a != base]:
        lo, hi = interval(arm, "cost", ks)
        plo, phi = interval(arm, "passed", ks)
        cheaper = sum(mean(A[arm][k], "cost") < mean(A[base][k], "cost") for k in ks)
        print(f"  {arm:12} ${total(arm, 'cost', ks):.3f} {change(arm, 'cost', ks):+4.0f}% [{lo:+.0f},{hi:+.0f}] cheaper on {cheaper}/{len(ks)}  calls {change(arm, 'calls', ks):+4.0f}%  ctx {change(arm, 'context', ks):+4.0f}%  time {change(arm, 'ms', ks):+4.0f}%  passed {sum(x['passed'] for k in ks for x in A[arm][k])} ({(total(arm, 'passed', ks) - total(base, 'passed', ks)) / len(ks) * 100:+.1f} pts [{plo:+.0f},{phi:+.0f}])")
