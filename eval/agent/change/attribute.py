#!/usr/bin/env python3
"""Where a run's money went, per arm: the prefix every turn re-reads, each
tool's results, the model's output, and subagents. Reads the session
transcripts run.py keeps, so it costs nothing.

  attribute.py TAG [TAG ...]

Prices come from the runs themselves: each session reports its cost and its
token counts per model, and a least-squares fit recovers the per-token prices
exactly. A tool result is charged for being written to the cache once and
read back on every later turn. Its size is its share, by characters, of how
much the context grew across that turn. That, and splitting a turn between
several results, are estimates; output and the totals are exact.
"""
import collections
import glob
import json
import os
import sys

WORK = os.environ.get("VEXUS_EVAL_WORK", os.path.expanduser("~/.cache/vexus-eval"))
COLUMNS = ("input", "write5m", "write1h", "read", "output")


def load(path):
    out = []
    for line in open(path, errors="replace"):
        try:
            out.append(json.loads(line))
        except ValueError:
            pass
    return out


def model_usage(u):
    return [u["inputTokens"], u.get("cacheCreationEphemeral5mInputTokens", 0), u.get("cacheCreationEphemeral1hInputTokens", 0),
            u["cacheReadInputTokens"], u["outputTokens"]]


def solve(rows, ys):
    """Least squares by the normal equations. Columns that are always zero get
    a price of zero, so they can't make the system singular."""
    used = [j for j in range(len(COLUMNS)) if any(r[j] for r in rows)]
    a = [[sum(r[i] * r[j] for r in rows) / 1e12 for j in used] + [sum(r[i] * y for r, y in zip(rows, ys)) / 1e6] for i in used]
    for c in range(len(used)):
        p = max(range(c, len(used)), key=lambda r: abs(a[r][c]))
        a[c], a[p] = a[p], a[c]
        for r in range(len(used)):
            if r != c and a[c][c]:
                f = a[r][c] / a[c][c]
                a[r] = [x - f * y for x, y in zip(a[r], a[c])]
    price = dict.fromkeys(COLUMNS, 0.0)
    for c, j in enumerate(used):
        price[COLUMNS[j]] = a[c][-1] / a[c][c] / 1e6 if a[c][c] else 0.0
    return price


def prices(sessions):
    by = collections.defaultdict(lambda: ([], []))
    for msgs in sessions:
        result = next((m for m in msgs if m.get("type") == "result"), None)
        for model, u in (result or {}).get("modelUsage", {}).items():
            if u.get("costUSD"):
                by[model][0].append(model_usage(u))
                by[model][1].append(u["costUSD"])
    return {model: solve(rows, ys) for model, (rows, ys) in by.items() if len(rows) >= len(COLUMNS)}


def tool_kind(name, args):
    if name == "Read":
        return "Read (range)" if args.get("offset") or args.get("limit") else "Read (whole file)"
    if name == "Bash":
        word = (args.get("command") or "").strip().split(" ")[0]
        return "Bash grep/find" if word in ("grep", "rg", "find", "ls", "cat", "head", "tail", "sed", "wc") else "Bash other"
    if name.startswith("mcp__vexus__"):
        return "vexus " + name.split("__")[-1]
    return name


def turn_cost(u, p):
    c = u.get("cache_creation") or {}
    write5m = c.get("ephemeral_5m_input_tokens", 0)
    write1h = c.get("ephemeral_1h_input_tokens", u.get("cache_creation_input_tokens", 0) - write5m)
    return (u.get("input_tokens", 0) * p["input"] + write5m * p["write5m"] + write1h * p["write1h"]
            + u.get("cache_read_input_tokens", 0) * p["read"])


def attribute(msgs, price):
    """One session's cost, split into parts."""
    result = next((m for m in msgs if m.get("type") == "result"), None)
    if not result or not result.get("total_cost_usd"):
        return None
    turns, uses, gaps = {}, {}, collections.defaultdict(list)
    sub_input = 0.0
    last = None
    for m in msgs:
        if m.get("type") == "assistant":
            msg = m["message"]
            p = price.get(msg.get("model")) or next(iter(price.values()))
            if m.get("parent_tool_use_id"):
                sub_input += turn_cost(msg.get("usage", {}), p) if msg.get("id") not in turns else 0
                turns.setdefault(msg.get("id"), None)
                continue
            if msg.get("id") not in turns:
                turns[msg["id"]] = dict(usage=msg.get("usage", {}), price=p, out_chars=0)
            last = msg["id"]
            for c in msg.get("content", []):
                turns[last]["out_chars"] += len(json.dumps(c.get("input") or c.get("text") or c.get("thinking") or ""))
                if c.get("type") == "tool_use":
                    uses[c["id"]] = tool_kind(c["name"], c.get("input") or {})
        elif m.get("type") == "user" and not m.get("parent_tool_use_id") and last:
            content = m["message"].get("content")
            for c in content if isinstance(content, list) else []:
                if c.get("type") == "tool_result":
                    gaps[last].append((uses.get(c.get("tool_use_id"), "?"), len(json.dumps(c.get("content")))))
    main = [(k, t) for k, t in turns.items() if t]
    if not main:
        return None
    size = lambda u: u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0)
    p = main[0][1]["price"]
    parts = collections.Counter()
    parts["prefix"] = turn_cost(main[0][1]["usage"], p) + size(main[0][1]["usage"]) * p["read"] * (len(main) - 1)
    for i, (key, t) in enumerate(main[:-1]):
        grew = max(size(main[i + 1][1]["usage"]) - size(t["usage"]), 0)
        chars = sum(n for _, n in gaps[key]) + t["out_chars"]
        later = len(main) - i - 2
        for kind, n in gaps[key]:
            tokens = grew * n / chars if chars else 0
            parts["tool: " + kind] += tokens * (p["write1h"] or p["write5m"]) + tokens * p["read"] * later
    out = sum(u["outputTokens"] * price.get(model, p)["output"] for model, u in result.get("modelUsage", {}).items())
    parts["output"] = out
    parts["subagents (input)"] = sub_input
    parts["other context"] = result["total_cost_usd"] - sum(parts.values())
    return dict(cost=result["total_cost_usd"], turns=len(main), prefix=size(main[0][1]["usage"]), parts=parts,
                calls=collections.Counter(k for k in uses.values()))


def main():
    sessions = collections.defaultdict(list)
    for tag in sys.argv[1:]:
        arms = {json.loads(line)["arm"] for line in open(f"{WORK}/runs/{tag}.jsonl")}
        for path in glob.glob(f"{WORK}/runs/{tag}/*.jsonl"):
            name = os.path.basename(path)[:-6]
            arm = max((a for a in arms if name.endswith("-" + a)), key=len, default=None)
            if arm:
                sessions[arm].append(load(path))
    price = prices([s for ss in sessions.values() for s in ss])
    for model, p in price.items():
        print(f"{model}: " + ", ".join(f"{k} ${v * 1e6:.3f}/M" for k, v in p.items() if v))
    for arm in sorted(sessions):
        done = [x for x in (attribute(s, price) for s in sessions[arm]) if x]
        total = sum(x["cost"] for x in done)
        parts = sum((x["parts"] for x in done), collections.Counter())
        calls = sum((x["calls"] for x in done), collections.Counter())
        n = len(done)
        print(f"\n{arm}: {n} sessions, ${total / n:.4f} a session, {sum(x['turns'] for x in done) / n:.1f} turns, "
              f"prefix {sum(x['prefix'] for x in done) / n / 1000:.1f}k tokens")
        for part, cost in sorted(parts.items(), key=lambda kv: -kv[1]):
            kind = part.removeprefix("tool: ")
            extra = f"  {calls[kind] / n:4.1f} calls a session" if part.startswith("tool: ") else ""
            print(f"  {part:28} {cost / total * 100:5.1f}%  ${cost / n:.4f}{extra}")


if __name__ == "__main__":
    main()
