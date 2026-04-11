# RSDB Operations

Snapshot date: 2026-04-11

This repository already encodes its primary operational flows as scripts. Use
these scripts directly unless the script itself needs repair or the user asks
for a custom flow.

## Build And Install

- Build the whole workspace:
  `cargo build --workspace`
- Run the Rust tests:
  `cargo test --workspace`
- Install the host CLI:
  `cargo install --path crates/rsdb-cli --force`

## Build Device RPMs Manually

Current packaging inputs:

- `.cargo-tizen.toml`
- `tizen/rpm/rsdbd.spec`
- `tizen/rpm/sources/rsdbd.service`
- `tizen/rpm/sources/rsdbd.env`

Current checked-in versioned RPMs:

- `tizen/rpm/sources/rsdbd-0.1.0-1.{aarch64,armv7l}.rpm`
- `tizen/rpm/sources/rsdbd-0.1.1-1.{aarch64,armv7l}.rpm`
- `tizen/rpm/sources/rsdbd-0.1.2-1.{aarch64,armv7l}.rpm`
- `tizen/rpm/sources/rsdbd-0.1.3-1.{aarch64,armv7l}.rpm`

Build fresh RPMs with:

```bash
cargo tizen rpm --package rsdbd --arch aarch64 --release
cargo tizen rpm --package rsdbd --arch armv7l --release
```

Current output paths:

- `target/tizen/aarch64/release/rpmbuild/RPMS/aarch64/`
- `target/tizen/armv7l/release/rpmbuild/RPMS/armv7l/`

## Manual Device Install From An RPM

Current package targets:

- `rsdbd-*.aarch64.rpm`
- `rsdbd-*.armv7l.rpm`

Install or upgrade on the device with `rpm -Uvh`.

What the current RPM layout does:

- installs `/usr/bin/rsdbd`
- installs `/usr/lib/systemd/system/rsdbd.service`
- installs `/etc/rsdbd.env`
- preserves `/etc/rsdbd.env` on upgrade as `%config(noreplace)`
- runs `systemctl daemon-reload`
- enables `rsdbd.service`
- attempts to restart `rsdbd.service`
- prints exact status and log commands if the restart does not become healthy

## Release And Publish State

There is no checked-in GitHub release automation script in this repository
today.

If the user asks to publish or refresh a release, do not claim a standard
scripted flow exists. First inspect:

- the current branch and remote/tag state
- the current version in `Cargo.toml` and `tizen/rpm/rsdbd.spec`
- the RPMs present in `tizen/rpm/sources/` and any freshly built RPMs under
  `target/tizen/`

## Development Device Daemon Refresh

Entry point: `./scripts/dev-update-rsdbd.sh [--target <ip[:port]>]`

Current script preconditions:

- `cargo`, `awk`, `rsdb`, and `date` must exist
- `cargo tizen --version` must succeed
- the current `rsdb` CLI must already be able to talk to the target

What the script does today:

- builds the local release CLI
- detects remote architecture with `rsdb shell uname -m`
- normalizes the target architecture to `aarch64` or `armv7l`
- builds the matching `rsdbd` RPM
- pushes the RPM to `/tmp` on the target
- schedules a detached `rpm -Uvh --force` via `systemd-run`
- reinstalls the local `rsdb` CLI with `cargo install`
- waits for `ping` and `shell true` to succeed again
- verifies that `rsdbd.service` appears to have restarted

What to report after running it:

- the detected target architecture
- the RPM path that was built and pushed
- whether the updated local CLI could talk to the restarted daemon

## Live Regression Smoke Against A Device

Shell and transfer smoke:

- entry point:
  `./scripts/test/rsdb-regression.sh [--target <ip[:port]>]`
- current coverage:
  multi-source push, roundtrip pull, independent pull, PTY shell smoke,
  repeated short shell calls, repeated ping calls

Machine-facing agent smoke:

- entry point:
  `./scripts/test/rsdb-agent-regression.sh [--target <ip[:port]>]`
- current coverage:
  `schema`, `discover`, `exec`, `exec --stream`, `exec --check`, `fs stat`,
  `fs list`, `fs read`, `fs write`, `fs mkdir`, `fs rm`, `fs move`,
  `transfer push`, and `transfer pull`

Use the script result as the primary signal before inventing ad hoc checks.

## Local Loopback Harness

Fast end-to-end local validation entry point:

- `./scripts/dev/local-loopback.sh`

Current behavior:

- delegates to `./scripts/test/local-loopback-smoke.sh --spawn-daemon`
- builds local `rsdb` and `rsdbd`
- starts `rsdbd` on `127.0.0.1:27131`
- uses an isolated `XDG_CONFIG_HOME` under a temporary run dir
- runs `cargo test --workspace`
- runs both device-style smoke suites against the loopback daemon

Fine-grained control:

- start and print the run dir:
  `./scripts/dev/local-loopback-up.sh --print-run-dir`
- rerun the loopback smoke against an existing run dir:
  `./scripts/test/local-loopback-smoke.sh --run-dir <path>`
- stop and clean up:
  `./scripts/dev/local-loopback-down.sh --run-dir <path>`

## CLI Completion

Entry point: `./scripts/install-completions.sh`

Current behavior:

- copies `scripts/rsdb-completion.bash` to
  `$XDG_CONFIG_HOME/rsdb/rsdb-completion.bash`
- appends a guarded source block to `~/.bashrc`
- completion coverage today is only for `rsdb edit`
