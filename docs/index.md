# RSDB Docs Index

Snapshot date: 2026-04-11

This directory is the repository-local knowledge base. `AGENTS.md` stays short
and routes into these documents.

## Read Order

1. [`../ARCHITECTURE.md`](../ARCHITECTURE.md)
2. [`core-beliefs.md`](core-beliefs.md)
3. [`repository-map.md`](repository-map.md)
4. [`operations.md`](operations.md)
5. [`testing.md`](testing.md)
6. [`quality.md`](quality.md)
7. [`exec-plans/index.md`](exec-plans/index.md)

## Catalog

- [`core-beliefs.md`](core-beliefs.md):
  the maintenance rules that should stay stable across future agent work
- [`repository-map.md`](repository-map.md):
  what each top-level directory currently owns
- [`operations.md`](operations.md):
  release, device-update, regression, loopback, and completion workflows
- [`testing.md`](testing.md):
  the current verification surface and known gaps
- [`quality.md`](quality.md):
  the current quality snapshot and where the repo is still weak
- [`exec-plans/`](exec-plans/index.md):
  checked-in plans and the current tech-debt tracker
- [`references/agent-tool-bootstrap.md`](references/agent-tool-bootstrap.md):
  host-level instructions for other coding agents that should use `rsdb`

## Current Repository Facts

- The workspace has three Rust members:
  `crates/rsdb-proto`, `crates/rsdb-cli`, and `device/rsdbd`.
- The primary operational surface is script-driven under `scripts/`.
- The repo currently has no checked-in CI workflow.
- The daemon currently exposes no auth or transport encryption.
