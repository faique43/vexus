# Change-request eval

What `compare.sh` can't show: whether Vexus saves money when an agent is asked
to change code, and whether the change still works. Everything in the README's
live agent table comes from here.

```sh
export VEXUS_BIN=$PWD/target/release/vexus VEXUS_EVAL_WORK=/tmp/vexus-eval
cd eval/agent/change
python3 setup.py repos && python3 setup.py real && python3 setup.py index
python3 run.py tasks --sets small,named,vague --arms plain,hook --rounds 3 --tag final
python3 run.py long --arms plain,hook --rounds 6 --tag long
python3 run.py questions --arms plain,hook,mcp --rounds 3 --tag questions
python3 summary.py final
```

Needs `claude`, `gh`, `git`, `bun` and `node`. A full run costs about $30 of
tokens and takes about an hour.

## Task sets

| Set | What it is | Scored by |
| --- | --- | --- |
| `small` | 14 short requests on Express and NestJS. Two start from a seeded bug, described only by its symptom. | Hidden tests on Express. On NestJS, whether the change landed, which undercounts valid fixes made elsewhere. |
| `named` | 17 real NestJS bugs, as filed, at the commit before the fix. | The maintainers' own specs, which the agent never sees. |
| `vague` | The same 17, rewritten by a cheap model with every code name removed. | The same specs. |
| `large` | Recent real fixes in VS Code. | Whether the agent changed the files the fix did. There's no spec, so this is weaker. |

`setup.py real` runs each spec before and after the real fix, and a task only
counts if it fails before and passes after.

## Reading the results

Arms run in one window, shuffled, with caches warmed, because a run an hour
later or a cold cache moves costs by more than most effects here. Compare arms
from the same run, never across runs.

An arm with `-tests` may also run the project's test command, and nothing else.
Prompts in `named` come from public issue text, so an agent there doesn't get
an open shell.

A saving also needs a control that does the same thing without Vexus. Steering
text once looked like a 21% saving, and the same advice with plain tools saved
as much.
