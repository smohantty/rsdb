# RSDB Agent Guide

This file is a task router. The repository system of record lives in
[`ARCHITECTURE.md`](ARCHITECTURE.md) and the structured documents under
[`docs/`](docs/index.md).

## Start Here

- Read [`ARCHITECTURE.md`](ARCHITECTURE.md) for the package map, wire protocol,
  packaging surface, and current security model.
- Read [`docs/index.md`](docs/index.md) for the documentation catalog.
- Use [`docs/operations.md`](docs/operations.md) for packaging, device-update,
  loopback, and regression workflows.
- Use [`docs/testing.md`](docs/testing.md) for verification expectations.
- Check [`docs/quality.md`](docs/quality.md) and
  [`docs/exec-plans/tech-debt-tracker.md`](docs/exec-plans/tech-debt-tracker.md)
  before large maintenance passes.

## Direct Workflow Entry Points

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

There is no checked-in GitHub release refresh script in this repository today.
Use [`docs/operations.md`](docs/operations.md) for the current manual RPM-build
and packaging state before proposing any publish flow.

## Maintenance Rules

- Keep `AGENTS.md` short. Put durable knowledge in versioned docs.
- Keep docs exact. Delete or rewrite stale statements in the same change that
  invalidates them.
- When the protocol, CLI contract, or daemon behavior changes, update
  `rsdb-proto`, `rsdb-cli`, `rsdbd`, the relevant smoke scripts, and the docs in
  the same change.
- Treat `rsdb agent schema` as the machine-facing contract summary. If that
  surface changes, update the docs that instruct agents how to use it.
- Keep path references exact. The current packaging inputs live under
  `tizen/rpm/`, not `packaging/`.
- Surface exact script precondition failures instead of improvising around them.

## Report Back

- Development device update: report the detected target architecture, the RPM
  that was built and pushed, and whether the updated local CLI could talk to the
  restarted daemon.
- Manual packaging or publish work: report the exact commands used, the commit
  and assets touched, and any remaining manual follow-up.
- Regression smoke: report the script result first, then any additional ad hoc
  findings.
