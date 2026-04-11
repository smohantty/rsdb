# RSDB Repository Map

Snapshot date: 2026-04-11

## Top Level

- `AGENTS.md`: short task router for coding agents
- `ARCHITECTURE.md`: top-level system map and change boundaries
- `README.md`: user-facing overview and quickstart
- `CLAUDE.md`: points Claude-style agents at `AGENTS.md`
- `Cargo.toml`: workspace definition, shared version, shared dependencies, and
  shared Clippy lint level

## Rust Workspace

- `crates/rsdb-proto`: shared framing, discovery, control messages, structured
  filesystem types, and protocol helpers
- `crates/rsdb-cli`: the `rsdb` binary; user-facing and machine-facing host
  surface
- `device/rsdbd`: the target daemon; command execution, shell/PTTY, structured
  filesystem operations, and bulk transfer

## Packaging

- `packaging/rsdbd.spec`: RPM install/update/remove behavior
- `packaging/artifacts/rsdbd.service`: systemd unit
- `packaging/artifacts/rsdbd.env`: default environment file
- `packaging/artifacts/aarch64/rsdbd` and
  `packaging/artifacts/armv7l/rsdbd`: checked-in architecture-specific daemon
  artifacts present in the repo today

## Scripts

- `scripts/release-rsdb.sh`: GitHub release/tag refresh and RPM asset upload
- `scripts/dev-update-rsdbd.sh`: detect target arch, build matching RPM, push,
  detached install, reinstall local CLI, verify daemon restart
- `scripts/dev/local-loopback*.sh`: build binaries and manage a loopback daemon
  run dir
- `scripts/test/rsdb-regression.sh`: live shell and bulk-transfer smoke test
- `scripts/test/rsdb-agent-regression.sh`: live machine-facing smoke test
- `scripts/test/local-loopback-smoke.sh`: local harness that runs workspace
  tests plus both smoke suites against a loopback daemon
- `scripts/install-completions.sh` and `scripts/rsdb-completion.bash`:
  static bash completion install path for `rsdb edit`

## Docs

- `docs/`: system-of-record knowledge base
- `docs/exec-plans/`: checked-in plans and the tech-debt tracker
- `docs/references/`: external reference snippets and agent bootstrap text

## What Is Not In The Repo

- No `.github/workflows` directory or other checked-in CI automation
- No checked-in doc linter or doc-freshness automation
- No auth, ACL, or transport-encryption layer for the daemon
