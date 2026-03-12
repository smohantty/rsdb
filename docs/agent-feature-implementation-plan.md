# RSDB Agent Feature Implementation Plan

This document is the implementation work order for the agent feature set defined in
[`docs/agent-feature-plan.md`](./agent-feature-plan.md).

The goal is to make `rsdb agent` a small, scriptable, machine-facing contract that
autonomous agents and wrapper scripts can rely on without falling back to the
human-oriented top-level commands.

## Scope

Implement the minimum v1 surface under `rsdb agent`:

- `discover`
- `exec`
- `fs`
- `transfer`

Do not add any additional top-level `rsdb agent` subcommands in the first pass.

Do not expose top-level human commands as part of the agent surface:

- `connect`
- `disconnect`
- `devices`
- `discover`
- `ping`
- `shell`
- `push`
- `pull`

## Desired end state

After this work:

- a coding agent can use only `rsdb agent ...`
- every `rsdb agent` command emits a stable JSON envelope on stdout
- every target-scoped `rsdb agent` command requires an explicit target
- agents can do command execution, small-file inspection/mutation, and bulk transfer
- skills and tool wrappers can present one small playbook instead of a mixed CLI

## Current code touchpoints

Primary files:

- [`crates/rsdb-cli/src/main.rs`](/home/suv-jetson/work/rsdb/crates/rsdb-cli/src/main.rs)
- [`crates/rsdb-proto/src/lib.rs`](/home/suv-jetson/work/rsdb/crates/rsdb-proto/src/lib.rs)
- [`device/rsdbd/src/main.rs`](/home/suv-jetson/work/rsdb/device/rsdbd/src/main.rs)

Current constraints:

- the CLI has only top-level human commands
- the daemon already supports direct `Exec`
- the daemon does not support remote fs primitives beyond push/pull
- the daemon does not support session or lock semantics
- protocol errors are still coarse

## CLI contract to implement

Use one machine-facing shape:

```text
rsdb agent discover [--probe-addr <addr>] [--port <port>] [--timeout-ms <n>]
rsdb agent --target <addr> exec [flags] [--stream] -- <command> [args...]
rsdb agent --target <addr> fs stat <path> [flags]
rsdb agent --target <addr> fs list <path> [flags]
rsdb agent --target <addr> fs read <path> [flags]
rsdb agent --target <addr> fs write <path> [flags]
rsdb agent --target <addr> fs mkdir <path> [flags]
rsdb agent --target <addr> fs rm <path> [flags]
rsdb agent --target <addr> fs move <source> <destination> [flags]
rsdb agent --target <addr> transfer push <src...> <remote-destination> [flags]
rsdb agent --target <addr> transfer pull <remote...> <local-destination> [flags]
```

Target rules:

- `discover` does not take `--target`
- every other `rsdb agent` command requires `--target <addr>`
- `rsdb agent` must not read or mutate the saved target registry
- no implicit fallback to the current target selected by human commands

## JSON envelope

All `rsdb agent` commands must print exactly one JSON document to stdout unless
event streaming is explicitly requested.

Success shape:

```json
{
  "schema_version": "agent.v1",
  "command": "exec",
  "ok": true,
  "data": {
    "target": "192.168.0.42:27101",
    "status": 0,
    "stdout": "ok\n",
    "stderr": "",
    "duration_ms": 12,
    "timed_out": false
  }
}
```

Error shape:

```json
{
  "schema_version": "agent.v1",
  "command": "exec",
  "ok": false,
  "error": {
    "code": "connection.refused",
    "message": "daemon not reachable",
    "target": "192.168.0.42:27101",
    "retryable": true,
    "details": {}
  }
}
```

Contract rules:

- `stdout` contains only machine-readable JSON
- `stderr` is reserved for transport diagnostics only
- optional command fields should be present as `null` when unknown
- command-specific payloads live inside `data`
- `exec --stream` is the first explicit exception to the single-document rule and must emit machine-readable JSON events on stdout until completion

## Minimum subcommand semantics

### `discover`

Purpose:

- first call when no target is known
- return candidate targets with minimal compatibility data
- stay shallow and cheap

Required `data` fields in phase 0:

- `targets`
- each target entry must include:
- `target`
- `server_id`
- `protocol_version`
- `compatible`
- `supported_operations`

Optional phase 0 fields:

- `device_name`
- `platform`

Important implementation detail:

- `supported_operations` means what this CLI can safely do against that target
- phase 0 may derive `supported_operations` from the discovery response plus local CLI knowledge
- discovery must not turn into a deep target-introspection command in v1

### `exec`

Purpose:

- structured, non-interactive command execution

Phase 0 flags:

- `--stream`
- `--cwd <path>` is deferred
- `--env KEY=VALUE` is deferred
- `--stdin-file <path>` is deferred
- `--timeout-secs <n>` is optional if it can be applied client-side at first
- `--check`

Phase 0 must:

- use `ControlRequest::Exec`
- never call `shell` for one-shot command execution
- support `--stream` for incremental stdout and stderr delivery from direct process execution
- return `status`, `stdout`, `stderr`, `duration_ms`, and `timed_out`

When `--stream` is set:

- stdout remains machine-readable by emitting JSONL events rather than raw process bytes
- event records should distinguish `stdout`, `stderr`, and terminal completion
- the final event should include at least `status`, `duration_ms`, and `timed_out`
- the command should not require shell syntax just to get live output

Phase 1 extends:

- working directory
- environment variables
- stdin support
- output limits
- explicit daemon-side timeout result

### `fs`

`fs` is for small, structured remote filesystem operations.

Phase 1 minimum operations:

- `stat`
- `list`
- `read`
- `write`
- `mkdir`
- `rm`
- `move`

Phase 1 minimum behavior:

- `stat` optionally computes `sha256` only when requested
- `list` supports path, `--recursive`, and `--max-depth`
- `read` supports `utf8` and `base64`
- `write` supports `create_parent`, `atomic`, `if_missing`, and `if_sha256`
- `mkdir` supports `parents`
- `rm` supports `recursive`, `force`, and `if_exists`
- `move` supports overwrite control

Phase 1 limits:

- keep `read` and `write` for small files only
- enforce a fixed maximum payload size such as 1 MiB or 4 MiB
- use `transfer` for larger artifacts or trees

### `transfer`

`transfer` is for bulk file and directory movement.

Phase 0:

- may be CLI wrappers over existing `PushBatch` and `PullBatch`
- emit the agent JSON envelope instead of human text

Phase 2:

- `transfer push` supports `--verify size|sha256`
- `transfer push` supports `--atomic` by staging to a temporary remote path and finalizing with `fs move`
- `transfer push` supports `--if-changed`
- `transfer pull` supports verification and richer result metadata

## Protocol changes

## Phase 0 protocol strategy

Do not add new daemon requests before the first usable surface exists.

Phase 0 uses:

- existing `ControlRequest::Exec`
- existing discovery helpers where available
- existing `PushBatch` and `PullBatch`

This keeps the first delivery small and proves the CLI contract before larger
daemon work begins.

## Phase 1 protocol additions

Add these `ControlRequest` and `ControlResponse` variants in
[`crates/rsdb-proto/src/lib.rs`](/home/suv-jetson/work/rsdb/crates/rsdb-proto/src/lib.rs):

- `FsStat`
- `FsList`
- `FsRead`
- `FsWrite`
- `FsMkdir`
- `FsRm`
- `FsMove`

Add supporting structs:

- `AgentFsStat`
- `AgentFsEntry`
- `AgentFsReadResult`
- `AgentFsWriteResult`
- `AgentFsMkdirResult`
- `AgentFsRmResult`
- `AgentFsMoveResult`
- `HashAlgorithm`
- `ContentEncoding`

Recommended compatibility approach:

- add new variants rather than replacing existing human-oriented ones
- add fields with `#[serde(default)]` when extending existing structs
- avoid changing frame format or protocol version unless framing itself changes

## Error code implementation

Expand [`rsdb_proto::ErrorCode`](/home/suv-jetson/work/rsdb/crates/rsdb-proto/src/lib.rs) to support the agent error catalog.

Recommended initial remote-side variants:

- `CapabilityMissing`
- `ExecStartFailed`
- `ExecTimeout`
- `FsNotFound`
- `FsPermissionDenied`
- `FsPreconditionFailed`
- `FsNoSpace`
- `TransferInterrupted`
- `TransferVerificationFailed`

CLI-side mapping is also required for local failures:

- connection refused -> `connection.refused`
- connect timeout -> `connection.timeout`
- protocol mismatch -> `connection.protocol_mismatch`
- missing target -> `target.required`
- unknown target -> `target.not_found`
- nonzero exec under `--check` -> `exec.nonzero`

Implementation rule:

- daemon responses use `rsdb_proto::ErrorCode`
- CLI wraps both local and remote errors into the same dotted-code catalog in the agent JSON envelope

## Capability naming

The daemon capability set should eventually advertise these names:

- `agent.exec.v1`
- `agent.fs.stat.v1`
- `agent.fs.list.v1`
- `agent.fs.read.v1`
- `agent.fs.write.v1`
- `agent.fs.mkdir.v1`
- `agent.fs.rm.v1`
- `agent.fs.move.v1`
- `agent.transfer.push.v1`
- `agent.transfer.pull.v1`

Phase 0 rule:

- `discover.targets[*].supported_operations` may be derived without these names
- do not claim dedicated `agent.*.v1` support until the daemon actually supports it

## File-by-file implementation plan

### 1. CLI work in `crates/rsdb-cli/src/main.rs`

Tasks:

- add a top-level `Agent` subcommand
- add nested subcommands for `discover`, `exec`, `fs`, and `transfer`
- add `resolve_agent_target()` for target-scoped commands that requires `--target` and never reads the saved registry
- add JSON envelope structs and printers
- implement `agent discover` by wrapping the existing discovery response and deriving compatibility data
- implement `agent exec` on top of `ControlRequest::Exec`
- implement phase 0 `agent transfer push/pull` as JSON wrappers over existing batch transfer functions

Strong recommendation:

- do not start with a large refactor of the existing CLI file
- keep the first pass in `main.rs` if that is the shortest path
- only extract helpers if readability becomes a problem

Acceptance criteria:

- no `rsdb agent` path prints human tables or human summary text
- no `rsdb agent` path touches the saved target registry
- one-shot remote command execution never uses `shell`

### 2. Protocol work in `crates/rsdb-proto/src/lib.rs`

Tasks:

- add new request and response variants for phase 1 fs support
- add agent result structs
- extend the error code enum
- add serde round-trip tests for each new request and response

Acceptance criteria:

- all new request/response variants round-trip in unit tests
- older commands remain unchanged

### 3. Daemon work in `device/rsdbd/src/main.rs`

Tasks:

- add fs handlers for stat, list, read, write, mkdir, rm, and move
- add hash computation helpers
- add precondition checks for `FsWrite`
- add capability advertisement for `agent.*.v1`
- extend `Exec` support if and when phase 1 adds cwd/env/stdin/timeout controls

Recommended implementation order:

1. `FsStat`
2. `FsRead`
3. `FsWrite`
4. `FsMove`
5. `FsList`
6. `FsMkdir`
7. `FsRm`

Acceptance criteria:

- fs handlers never shell out
- `FsWrite --atomic` writes to a temp file and renames into place
- `FsWrite --if-sha256` rejects changed targets with a stable precondition error
- capability advertisement matches real daemon support

## Delivery phases

### Phase 0: first usable agent surface

Deliver:

- `rsdb agent discover`
- `rsdb agent exec`
- `rsdb agent exec --stream`
- agent JSON envelope
- explicit target resolution for target-scoped operations
- JSON output for `agent transfer push/pull`
- prompt and wrapper contract for agents

Do not block phase 0 on:

- daemon fs protocol work
- expanded protocol error enums
- event streaming beyond `agent exec --stream`

Exit criteria:

- an autonomous agent can start with `discover` when no target is known
- run remote commands with `exec`
- stream long-running command output with `exec --stream`
- push and pull artifacts through `agent transfer`
- do all of that without using top-level human commands

### Phase 1: core agent protocol

Deliver:

- `fs stat`
- `fs list`
- `fs read`
- `fs write`
- `fs mkdir`
- `fs rm`
- `fs move`
- versioned capability names

Exit criteria:

- an autonomous agent no longer needs `shell` for basic file inspection or mutation
- `discover.targets[*].supported_operations` is sufficient for target selection in the common case

### Phase 2: verified transfer semantics

Deliver:

- `transfer push --verify`
- `transfer push --atomic`
- `transfer push --if-changed`
- verified `transfer pull`
- optional event streaming for long-running transfers

Exit criteria:

- agents can trust transfer postconditions without ad hoc shell verification

## Skill and prompt introduction plan

This feature set is only useful if agents are taught to use it consistently.

The introduction layer should live in:

- a repo-local skill
- a wrapper tool manifest
- or a system/developer prompt for the coding agent

That introduction layer should define exactly this playbook:

- use only `rsdb agent`
- if no target is known, call `discover` first
- choose a target from discovery results using `target`, `protocol_version`, `compatible`, and `supported_operations`
- use `exec` for command execution
- use `fs.*` for small-file work
- use `transfer.*` for bulk movement
- never call `shell`, `push`, `pull`, `ping`, `connect`, or `disconnect`
- keep `--target` explicit for every target-scoped call

Recommended tool shape for wrappers:

```json
{
  "name": "rsdb_agent",
  "required": ["operation"],
  "target_required_for": [
    "exec",
    "fs.stat",
    "fs.list",
    "fs.read",
    "fs.write",
    "fs.mkdir",
    "fs.rm",
    "fs.move",
    "transfer.push",
    "transfer.pull"
  ],
  "operations": [
    "discover",
    "exec",
    "fs.stat",
    "fs.list",
    "fs.read",
    "fs.write",
    "fs.mkdir",
    "fs.rm",
    "fs.move",
    "transfer.push",
    "transfer.pull"
  ]
}
```

Reason:

- the wrapper contract itself should teach the operation set
- adding extra top-level commands increases surface area without adding remote capability

## Testing plan

Protocol tests:

- add serde round-trip tests for all new request and response variants
- add tests for new error code serialization

CLI tests:

- add tests that each `rsdb agent` path emits only the JSON envelope
- add tests that `rsdb agent` never uses registry fallback
- add tests that `agent exec --check` maps nonzero exit to `exec.nonzero`
- add tests that `agent exec --stream` emits JSONL events and a final completion event

Daemon tests:

- add focused unit tests for fs preconditions and atomic write behavior where feasible

Integration tests:

- add a new script at `scripts/test/rsdb-agent-regression.sh`
- validate `discover`, `exec`, `exec --stream`, `fs stat`, `fs read`, `fs write`, `fs move`, and `transfer push/pull`
- include at least one conflict case for `fs write --if-sha256`

Skill/wrapper tests:

- add a lintable rule or test fixture ensuring wrappers call only `rsdb agent`

## Recommended implementation order for coding agents

1. Add CLI `agent` subcommand, JSON envelope, and explicit target handling.
2. Implement `agent discover` by wrapping the existing discovery response.
3. Implement `agent exec` on the existing `Exec` request.
4. Extend `agent exec` with `--stream` on the direct process path.
5. Add JSON `agent transfer push/pull` wrappers.
6. Expand protocol types for fs operations.
7. Implement daemon fs handlers and capability advertisement.
8. Implement CLI `fs` commands.
9. Add verified transfer semantics.
10. Add the new regression script.
11. Add or update the skill or tool prompt that teaches the minimum playbook.

## Done definition

This feature set is done when all of the following are true:

- `rsdb agent` exposes only `discover`, `exec`, `fs`, and `transfer`
- the surface is usable without top-level human commands
- the daemon advertises versioned agent capabilities for implemented remote features
- the CLI emits stable agent JSON envelopes for every command
- `agent exec --stream` supports incremental machine-readable output for long-running commands
- a wrapper skill can teach the entire contract in a short operation list
- regression coverage exists for both protocol and end-to-end behavior
