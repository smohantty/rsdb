# RSDB Agent Feature Plan

## Summary

RSDB is already useful for a human operator:

- `discover` and `devices` help a person find targets
- `shell` gives an interactive remote shell
- `push` and `pull` move files and directories
- `capability` exposes a basic machine-readable feature list

That is not the same thing as being agent-friendly. An autonomous agent needs deterministic output, explicit preconditions, stable error contracts, and small composable operations that do not require screen-scraping or shell quoting tricks.

The recommendation is to keep the current human-oriented CLI intact and add a separate `rsdb agent ...` namespace with JSON-first semantics and explicit capability negotiation.

This plan is intentionally focused on the agent command surface first. Everything below `rsdb agent` is machine-facing and contractual, meant for autonomous agents or scripts composing larger tasks. Human-friendly workflows stay at the top level of the CLI.

## Why agents need a different surface

An autonomous agent is not using RSDB like a person at a terminal. It is trying to:

- inspect a target and decide what actions are safe
- run commands without PTY side effects
- push artifacts with verifiable postconditions
- mutate remote files without rebuilding shell snippets every time
- distinguish retryable transport failures from remote command failures
- avoid depending on mutable local state such as "current target"

The current CLI still pushes agents toward ad hoc shell usage for many of these tasks.

## Current gaps in RSDB

1. Most commands are human-formatted, not machine-formatted.
   `devices`, `discover`, `ping`, `push`, and `pull` print text optimized for a person.

2. `shell` is doing double duty.
   It supports both interactive shell sessions and one-shot command execution, but its contract is still shell-oriented rather than automation-oriented.

3. The protocol already has an `Exec` request, but the CLI does not expose it.
   This is a concrete low-risk opportunity for an agent-focused command surface.

4. Remote filesystem operations are coarse.
   Agents can push and pull trees, but still have to use shell commands for `stat`, listing, read/write small files, mkdir, rm, mv, and post-transfer verification.

5. Target selection is user-friendly, not automation-friendly.
   Reading or mutating a shared local target registry is convenient for a human and risky for autonomous tooling.

6. Error handling is not yet a stable machine contract.
   Agents need structured error categories, retry hints, and durable exit-code semantics.

## Design goals

- Keep the existing user-facing CLI behavior stable.
- Add an explicit agent surface instead of overloading the human one.
- Make JSON the default output format for agent commands.
- Require explicit targeting for target-scoped agent operations.
- Prefer non-interactive requests over shell emulation.
- Keep the v1 top-level agent surface minimal: `discover`, `exec`, `fs`, and `transfer`.
- Add small filesystem primitives so agents stop synthesizing shell one-liners.
- Avoid creating two competing machine-facing interfaces.
- Prefer thin primitives and preflight checks before higher-level workflow commands.
- Make write operations idempotent or at least verifiable.
- Version the capability names and JSON schemas.

## Non-goals

- Replacing the current human workflow
- Hiding the shell completely
- Solving every deployment workflow in one release
- Teaching autonomous agents to mix the legacy human CLI with the new agent CLI
- Adding host-side UX helpers before the core machine contract is solid
- Adding overlapping v1 agent subcommands when the same job can be composed from `discover`, `exec`, `fs`, and `transfer`
- Adding domain-specific orchestration before the core machine contract is solid

## Minimum agent surface

The minimum v1 surface under `rsdb agent` should be:

- `discover`
- `exec`
- `fs`
- `transfer`

Anything else should stay out of the first release unless repeated real workflows show that composition is still too painful.

### `rsdb agent discover`

Purpose: return a shallow list of candidate targets and the minimum compatibility data an agent needs to choose one.

Suggested data fields:

- `targets`
- each target entry should include:
- `target`
- `server_id`
- `device_name` optional
- `platform` optional
- `protocol_version`
- `compatible`
- `supported_operations`

Rationale:

- gives agents one standard bootstrap path when no target is known yet
- keeps discovery shallow and cheap instead of turning it into a deep target-inspection API
- returns enough information to choose a target and decide whether the local agent surface can be used against it

Implementation note:

- phase 0 can wrap the existing discovery response and derive `supported_operations`
- no separate deeper target-introspection command is part of the minimum v1 surface

### `rsdb agent exec`

Purpose: expose structured non-interactive command execution without PTY behavior.

Suggested flags:

- `--target <addr>`
- `--cwd <path>`
- `--env KEY=VALUE` repeated
- `--stdin-file <path>` or `--stdin -`
- `--timeout-secs <n>`
- `--check`
- `--stdout-limit <bytes>`
- `--stderr-limit <bytes>`

Suggested data fields:

- `target`
- `command`
- `args`
- `cwd`
- `status`
- `timed_out`
- `stdout`
- `stderr`
- `duration_ms`

Rationale:

- separates automation from interactive shell semantics
- eliminates shell prompt noise and PTY side effects
- gives the agent stable `stdout` and `stderr` capture
- can start as a CLI wrapper around the already implemented `ControlRequest::Exec`

Implementation note:

- first milestone can use the existing protocol request
- a later protocol revision can add richer fields like `cwd`, `env`, stdin, and output limits

### `rsdb agent fs stat`

Purpose: query remote path metadata without running shell commands.

Suggested data fields:

- `exists`
- `path`
- `kind`
- `size`
- `mode`
- `mtime_unix_ms`
- `sha256` optional

Rationale:

- agents often need a precondition check before writing or pulling
- avoids parsing `stat`, `ls`, or `find` output
- enables smarter transfer decisions

### `rsdb agent fs list`

Purpose: return a JSON listing for a directory or glob expression.

Suggested flags:

- `--recursive`
- `--max-depth <n>`
- `--glob <pattern>`
- `--include-hidden`
- `--hash sha256`

Rationale:

- lets agents inspect remote trees without pulling them
- simplifies artifact discovery, config inspection, and cleanup planning

### `rsdb agent fs read`

Purpose: read a small remote file as text or base64.

Suggested flags:

- `--encoding utf8|base64`
- `--max-bytes <n>`

Rationale:

- agents often need to inspect config files, version files, unit files, or logs
- avoids `shell cat ...` plus quoting and truncation guesswork

### `rsdb agent fs write`

Purpose: write a small remote file atomically.

Suggested flags:

- `--path <path>`
- `--input-file <path>` or `--stdin -`
- `--mode <octal>`
- `--create-parent`
- `--atomic`
- `--if-missing`
- `--if-sha256 <hash>`

Rationale:

- many agent tasks are "update one config file, then restart a service"
- atomic write plus optional preconditions makes those tasks safer and more idempotent

### `rsdb agent fs mkdir`

Purpose: structured `mkdir -p` without shell fallback.

Suggested flags:

- `--parents`
- `--mode <octal>`

Rationale:

- common primitive needed before artifact upload or workspace preparation

### `rsdb agent fs rm`

Purpose: structured file or directory removal.

Suggested flags:

- `--recursive`
- `--force`
- `--if-exists`

Rationale:

- removes the need for shell `rm -rf` in many automation paths
- supports clearer safety checks and error reporting

### `rsdb agent fs move`

Purpose: structured rename or move, including atomic finalize after staging.

Suggested flags:

- `--overwrite`
- `--no-clobber`

Rationale:

- rename is the core primitive for safe "stage then publish" workflows
- especially useful after uploads, config rewrites, and build artifact replacement

### `rsdb agent transfer push`

Purpose: agent-oriented wrapper over the existing batch push engine.

Suggested improvements over current `push`:

- JSON result envelope
- `--verify size|sha256`
- `--atomic` using remote staging and rename
- `--if-changed`
- `--manifest-out <path>`
- `--dry-run`

Rationale:

- agents need more than "bytes were sent"
- upload success should include a postcondition the agent can trust

### `rsdb agent transfer pull`

Purpose: agent-oriented wrapper over the existing batch pull engine.

Suggested improvements over current `pull`:

- JSON metadata and result envelope
- `--manifest-out <path>`
- `--verify size|sha256`
- `--if-changed`
- `--dry-run`

Rationale:

- lets agents reason about what was fetched without parsing text output

## Cross-cutting conventions

### 1. `rsdb agent` should be JSON-first

For agent commands:

- stdout should contain only structured machine output
- stderr should be reserved for transport diagnostics only
- every JSON document should include `schema_version`, `command`, `ok`, and either `data` or `error`
- the per-command fields listed earlier should live inside `data`

Example envelope:

```json
{"schema_version":"agent.v1","command":"exec","ok":true,"data":{"target":"192.168.0.42:27101","status":0,"stdout":"ok\n","stderr":"","duration_ms":12,"timed_out":false}}
{"schema_version":"agent.v1","command":"exec","ok":false,"error":{"code":"connection.refused","message":"daemon not reachable","retryable":true,"details":{"target":"192.168.0.42:27101"}}}
```

### 2. Agent commands should avoid implicit registry behavior

Recommended behavior:

- require `--target` for every target-scoped `rsdb agent` command
- do not write the saved target registry from `rsdb agent`
- do not silently depend on the last human-selected device

### 3. Capability names should be versioned

Examples:

- `agent.exec.v1`
- `agent.fs.stat.v1`
- `agent.fs.list.v1`
- `agent.fs.read.v1`
- `agent.fs.write.v1`
- `agent.fs.move.v1`
- `agent.transfer.push.v1`
- `agent.transfer.pull.v1`

Rationale:

- versioned capability names let an agent negotiate feature support without guessing

### 4. Error contracts should be explicit

Suggested naming scheme:

- use stable dotted codes such as `<domain>.<reason>`
- keep the message human-readable, but treat the code as the contract

Recommended initial catalog:

- `connection.refused`
- `connection.timeout`
- `connection.protocol_mismatch`
- `capability.missing`
- `target.required`
- `target.not_found`
- `exec.start_failed`
- `exec.nonzero`
- `exec.timeout`
- `exec.signal`
- `fs.not_found`
- `fs.permission_denied`
- `fs.precondition_failed`
- `fs.no_space`
- `transfer.interrupted`
- `transfer.verification_failed`

Each JSON error should ideally include:

- `code`
- `message`
- `target`
- `retryable`
- `details`

Rationale:

- agents need to branch on error type, not parse message text
- messages can evolve; codes should remain stable across releases

### 5. Long-running commands should support event streaming

Suggested option:

- `--events jsonl`

Useful event types:

- `started`
- `progress`
- `retry`
- `completed`
- `failed`

Rationale:

- agents often need visibility during large transfers or wait loops
- JSONL is easier to consume incrementally than partial human progress text

### 6. Skills and tool wrappers should expose only the agent surface

Recommended rule:

- if RSDB is presented to an autonomous system through a skill, tool manifest, wrapper script, or MCP-style adapter, expose only `rsdb agent ...`

Do not expose these human commands in the same tool surface:

- `connect`
- `disconnect`
- `devices`
- `discover`
- `ping`
- `shell`
- `push`
- `pull`

Preferred replacements:

- use `agent discover` instead of top-level `discover`
- use `agent exec` instead of one-shot `shell`
- use `agent transfer push` and `pull` instead of legacy `push` and `pull`

Presentation guidance:

- do not mirror every legacy CLI command into a same-level tool menu
- prefer either one `rsdb_agent` wrapper with structured operations or a small family grouped by domain such as `discover`, `exec`, `fs`, and `transfer`
- keep discovery or registry-management helpers in a separate human/setup path, not in the main autonomous action surface
- example snippets, skills, and agent docs should use only `rsdb agent` forms
- do not rely on `--help` text as part of the contract

Rationale:

- if both `push` and `agent transfer push` are presented as equal options, agents will mix them and the interface split stops being meaningful
- the point of the plan is not just new commands; it is one unambiguous machine contract

### 7. Skills and prompts should teach one small playbook

Recommended prompt or skill contract:

- RSDB automation uses only `rsdb agent`.
- If no target is known, the first call is `rsdb agent discover`.
- Choose a target from discovery results using `target`, `protocol_version`, `compatible`, and `supported_operations`.
- Use `exec` for command execution, not for file inspection if an `fs` primitive exists.
- Use `fs stat`, `list`, `read`, `write`, `mkdir`, `rm`, and `move` for remote filesystem work.
- Use `transfer push` and `pull` only for bulk artifact or tree transfer.
- Pass an explicit `--target` for every target-scoped operation.
- If discovery shows an operation is unsupported for a target, choose another target or fall back deliberately.
- Never call top-level human commands such as `shell`, `push`, `pull`, `ping`, `connect`, or `disconnect`.

Preferred wrapper shape:

- one tool named `rsdb_agent`
- one required `operation` field whose values map to the minimum surface:
  `discover`, `exec`, `fs.stat`, `fs.list`, `fs.read`, `fs.write`, `fs.mkdir`, `fs.rm`, `fs.move`, `transfer.push`, `transfer.pull`
- one required `target` field for every operation except `discover`

Rationale:

- agents perform better when the contract is short, repetitive, and unambiguous
- a small playbook helps the agent choose the cheapest correct primitive instead of exploring the CLI

## Critical evaluation against the current interface

This plan does improve agent capability over today's RSDB interface, but not every proposed item does so equally.

### Changes that are mostly reliability or ergonomics

- a shared JSON envelope
- dotted error codes
- explicit target resolution instead of implicit registry dependence

These changes matter because they reduce parsing ambiguity and hidden local state, but by themselves they do not give an agent fundamentally new remote operations.

### Changes that materially improve agent capability

- `rsdb agent exec` exposes the daemon's direct `Exec` request instead of forcing one-shot automation through `shell`, which currently still runs via `shell -c ...`
- `rsdb agent discover` becomes a single standard bootstrap path instead of making agents scrape human discovery output or depend on saved local state
- `rsdb agent fs stat`, `list`, `read`, `write`, `mkdir`, `rm`, and `move` give agents remote file operations they do not currently have in a structured form
- `agent transfer push` and `pull` with verification and atomic staging create stronger postconditions than today's byte-count completion messages
- versioned agent capabilities let clients branch safely across daemon versions instead of guessing support and failing late

### Where the plan would still be weak

- if implementation stops at phase 0 formatting work, the gain is mostly ergonomic rather than a real capability expansion
- `discover` becomes weak if it grows into a deep target-inspection API instead of staying a shallow bootstrap command
- the proposed error contract requires protocol changes because the current wire error enum is still coarse
- the plan should not claim success until at least `agent exec`, capability advertisement, and one read-side filesystem primitive ship together
- the plan would create unnecessary clutter if skills or wrappers expose both the human CLI and the agent CLI as equal machine-facing choices
- the plan would also create clutter if it adds extra top-level agent commands before the minimum surface proves insufficient

### Why I strongly believe this direction is worth doing

The current code already shows the right fault lines. The CLI exposes only human-oriented commands, while the daemon already has a non-shell `Exec` path. At the same time, one-shot `shell` still goes through `shell -c ...`, target selection still falls back to shared local registry state, and there are no structured remote filesystem primitives beyond transfer. Those are real capability gaps for agents, not cosmetic issues. This plan is valuable because it addresses those specific gaps directly, and it does so with a small enough surface that skills and wrappers can teach it reliably.

## Prioritized rollout

### Priority summary

| Priority | Deliverable | Effort | Why first |
|----------|-------------|--------|-----------|
| P0 | Shared JSON envelope, `agent discover`, `agent exec`, and explicit target resolution | Medium | Establishes the minimum usable agent surface with the least protocol risk |
| P0 | Stable dotted error-code catalog | Small | Needed for reliable retries and failure handling from day one |
| P0 | Clear skill/tool exposure rules for `rsdb agent` only | Small | Prevents a mixed machine surface from forming around legacy commands |
| P1 | Core filesystem primitives | Medium | Removes the most common shell one-liner fallbacks |
| P2 | Verified transfer semantics | Medium | Makes push and pull results trustworthy for automation |
| P3 | Deferred convenience commands only if justified by repeated composition pain | Medium to large | Keeps the surface small until real evidence says otherwise |

### Phase 0: low-risk CLI wins

1. Add `rsdb agent exec` backed by the existing `ControlRequest::Exec`.
2. Add `rsdb agent discover` that wraps the existing discovery response and derives minimal compatibility data.
3. Define one shared JSON envelope and dotted error-code schema.
4. Make target-scoped `rsdb agent` commands use explicit target resolution instead of mutating the saved registry.
5. Publish one agent-facing usage guide for skills and wrappers that names only the `rsdb agent` surface and the minimum playbook.

Why this phase first:

- it unlocks immediate agent value
- it reuses protocol pieces already present in the codebase
- it avoids large daemon changes before the contract is proven
- it gives agents one standard bootstrap call and one standard execution primitive
- it prevents legacy human commands from becoming a second unofficial machine API

### Phase 1: core protocol additions

1. Add filesystem metadata and mutation requests:
   `stat`, `list`, `read`, `write`, `mkdir`, `rm`, `move`.
2. Extend exec with `cwd`, `env`, stdin, and timeout controls.
3. Advertise versioned agent capabilities.

### Phase 2: safe transfer semantics

1. Add atomic staging plus rename for uploads.
2. Add remote hash verification.
3. Add `if-changed` and precondition support.
4. Add JSONL progress events for large transfers.

### Phase 3: only expand if the minimum surface proves insufficient

1. Reassess real workflow pain after `discover`, `exec`, `fs`, and `transfer` have been used in practice.
2. Add no new top-level agent commands unless repeated evidence shows that composition is still too costly.

### Compatibility track

If legacy human commands later gain `--json` or other script-friendly output:

- treat that as compatibility for shell scripts and humans
- do not advertise it as the preferred autonomous interface
- do not include those commands in agent capability names, skill docs, or tool manifests

## Recommended first milestone

If only one feature is implemented first, it should be `rsdb agent exec`.

Why:

- the daemon and protocol already have an `Exec` path
- it removes the biggest current mismatch between human shell use and agent automation
- it gives an immediate escape hatch for structured remote execution while richer filesystem APIs are still being built

After that, the next highest-value additions are:

1. shared JSON envelope plus stable dotted error codes
2. `rsdb agent discover`
3. `rsdb agent fs stat`
4. `rsdb agent fs read`
5. `rsdb agent transfer push --json --verify`

## Testing implications

This agent surface needs contract tests, not only happy-path smoke tests.

Recommended coverage:

- golden JSON output tests for each CLI command
- protocol compatibility tests for capability negotiation
- integration tests for `exec`, `fs stat`, and transfer verification
- failure-path tests that validate stable error codes
- tests or lint rules for any skill/tool wrapper to ensure it calls only `rsdb agent` commands
- a new live smoke script focused on agent workflows, separate from the current shell and transfer regression script

## Decision summary

The best path is not to make every existing RSDB command slightly more scriptable. The better path is:

1. keep the current human CLI stable
2. add a dedicated `rsdb agent` namespace
3. make that namespace JSON-first, explicit, and capability-driven
4. keep the initial surface minimal: `discover`, `exec`, `fs`, and `transfer`
5. expose only that namespace to autonomous skills and tools
6. teach agents one small playbook instead of a broad command catalog

That gives RSDB a clear split between "human terminal tool" and "machine contract for autonomous agents" without forcing either use case into the wrong interface.
