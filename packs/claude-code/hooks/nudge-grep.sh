#!/usr/bin/env bash
# DEPRECATED: hooks.json now invokes `vexus hook nudge-grep` directly (no
# shell dependency, works on Windows). This script is shipped for one more
# release so hooks.json files installed by older versions keep working;
# re-run `vexus init --agent claude-code --force` to migrate.
exec vexus hook nudge-grep
