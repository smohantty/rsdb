# RSDB Agent Guide

This file is a task router. The repository system of record lives in
[`ARCHITECTURE.md`](ARCHITECTURE.md) and the structured documents under
[`docs/`](docs/index.md).

## Start Here

- Read [`ARCHITECTURE.md`](ARCHITECTURE.md) for the package map, wire protocol,
  packaging surface, and current security model.
- Read [`docs/index.md`](docs/index.md) for the documentation catalog.
- Use [`docs/operations.md`](docs/operations.md) for release, device-update,
  loopback, and regression workflows.
- Use [`docs/testing.md`](docs/testing.md) for verification expectations.
- Check [`docs/quality.md`](docs/quality.md) and
  [`docs/exec-plans/tech-debt-tracker.md`](docs/exec-plans/tech-debt-tracker.md)
  before large maintenance passes.

## Direct Workflow Entry Points

- GitHub release or release refresh:
  run `./scripts/release-rsdb.sh` from the repo root.
- Development device daemon update:
  run `./scripts/dev-update-rsdbd.sh [--target <ip[:port]>]`.
- Device regression smoke test:
  run `./scripts/test/rsdb-regression.sh [--target <ip[:port]>]`.
- Agent-surface regression smoke test:
  run `./scripts/test/rsdb-agent-regression.sh [--target <ip[:port]>]`.
- Full local loopback validation:
  run `./scripts/dev/local-loopback.sh`.

Do not reconstruct these script-driven flows manually unless the user asks for a
custom flow or the script itself needs repair.

## Maintenance Rules

- Keep `AGENTS.md` short. Put durable knowledge in versioned docs.
- Keep docs exact. Delete or rewrite stale statements in the same change that
  invalidates them.
- When the protocol, CLI contract, or daemon behavior changes, update
  `rsdb-proto`, `rsdb-cli`, `rsdbd`, the relevant smoke scripts, and the docs in
  the same change.
- Treat `rsdb agent schema` as the machine-facing contract summary. If that
  surface changes, update the docs that instruct agents how to use it.
- Surface exact script precondition failures instead of improvising around them.

## Report Back

- Release: report whether the release was created or refreshed, the commit used,
  the uploaded assets, and any failure plus the next corrective action.
- Development device update: report the detected target architecture, the RPM
  that was built and pushed, and whether the updated local CLI could talk to the
  restarted daemon.
- Regression smoke: report the script result first, then any additional ad hoc
  findings.
