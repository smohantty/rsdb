# RSDB Testing

Snapshot date: 2026-04-11

## Current Verification Surface

- Rust compile/test pass:
  `cargo test --workspace`
- Build-only compile check:
  `cargo test --workspace --no-run`
- Live shell and transfer smoke against a reachable daemon:
  `./scripts/test/rsdb-regression.sh`
- Live machine-facing smoke against a reachable daemon:
  `./scripts/test/rsdb-agent-regression.sh`
- Local end-to-end harness:
  `./scripts/test/local-loopback-smoke.sh --spawn-daemon`

## What Each Layer Proves

- `cargo test --workspace`:
  compiles all workspace members and runs the checked-in Rust unit and
  integration tests
- `rsdb-regression.sh`:
  validates PTY shell behavior and bulk file transfer against a live daemon
- `rsdb-agent-regression.sh`:
  validates the structured `rsdb agent` contract against a live daemon
- `local-loopback-smoke.sh`:
  combines the Rust tests with both live smoke suites against a locally spawned
  daemon

## Current Testing Gaps

- No checked-in CI workflow runs any of these commands automatically.
- The live device scripts require a reachable daemon and are not hermetic.
- Release packaging has no checked-in automation script and is not covered by a
  separate CI job.
- Documentation freshness is not mechanically tested today.

## Expected Maintenance Behavior

- If a protocol or command surface changes, update or extend the live smoke
  script that covers it.
- If a new recurring bug class appears, prefer a scriptable regression over a
  one-off manual note.
- If a change only touches docs, still verify links and command references
  against the checked-in scripts and source.
