# RSDB Tool Bootstrap For Coding Agents

Add the instruction block below to your coding agent's user-level config so
every conversation knows how to use `rsdb` without manual introduction.

## Instruction block

```md
This host has `rsdb`, a CLI for working with remote target devices (like adb/sdb/ssh but unified). When the user says "target", "device", "on device", or refers to deploying, installing, fetching logs, running commands, or testing on something other than this host — use `rsdb agent` for that work. Build and compile locally; deploy, inspect, and test on the target via rsdb.

Run `rsdb agent schema` to get the full command contract. Query the schema before acting instead of relying on memory or asking the user to restate the tool.
```

## User-level locations

Known user-level locations:

- Codex: `~/.codex/AGENTS.md`
- Claude Code: `~/.claude/CLAUDE.md`
