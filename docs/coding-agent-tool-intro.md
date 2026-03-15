# RSDB Tool Bootstrap For Coding Agents

Add the instruction block below to your coding agent's user-level config so
every conversation knows how to use `rsdb` without manual introduction.

## Instruction block

```md
This host has `rsdb`, a CLI for working with remote target devices (like adb/sdb/ssh but unified). When the user says "target", "device", "on device", or refers to deploying, installing, fetching logs, running commands, or testing on something other than this host — use `rsdb agent` for that work. Build and compile locally; deploy, inspect, and test on the target via rsdb.

Run `rsdb agent schema` once at the start of the session before the first target interaction. Cache the returned schema and reuse it for the rest of the session. Do not call `rsdb agent schema` again unless the user explicitly asks to refresh it, or a command fails in a way that suggests the schema or contract has changed.
```

## User-level locations

Known user-level locations:

- Codex: `~/.codex/AGENTS.md`
- Claude Code: `~/.claude/CLAUDE.md`
