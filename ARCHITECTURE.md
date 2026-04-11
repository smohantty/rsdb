# RSDB Architecture

Snapshot date: 2026-04-11

RSDB is a small Rust workspace for controlling a reachable `rsdbd` daemon over
IP. The codebase has three runtime components:

- `crates/rsdb-cli`: the host-side `rsdb` binary. It owns saved-target
  management, discovery, ping/capability checks, interactive shell sessions,
  bulk push/pull, remote file editing, and the machine-facing `rsdb agent ...`
  command surface.
- `crates/rsdb-proto`: the shared wire protocol crate. It defines frame
  encoding, discovery payloads, control request/response enums, stream framing,
  transfer metadata, and structured agent filesystem types.
- `device/rsdbd`: the target-side root daemon. It listens for discovery and TCP
  control traffic, executes commands, serves shells and PTYs, exposes
  structured filesystem operations, and handles batch file transfer.

## Runtime Model

- The default discovery and control port is `27101`.
- UDP discovery uses the `RSDBDISC` magic and returns daemon identity, platform,
  protocol version, and feature flags.
- TCP control traffic uses the `RSDB` frame header defined in `rsdb-proto`.
- `rsdb-proto` currently advertises protocol version `2`.
- The daemon capability set includes discovery, ping, capability reporting,
  direct exec, exec streaming, shell/PTTY support, structured agent filesystem
  operations, and batch push/pull.
- The agent inline filesystem path is intentionally small. `fs.read`,
  `fs.write`, and inline remote edit paths are capped at 4 MiB in the current
  code.

## Persistence And Local State

- Saved targets live in `$XDG_CONFIG_HOME/rsdb/targets.json` or
  `~/.config/rsdb/targets.json`.
- The CLI keeps a current target pointer inside that registry and falls back to
  the most recent saved target if no explicit current target remains.
- `rsdb edit` resolves the editor in this order:
  explicit `--editor`, `RSDB_EDITOR`, `VISUAL`, `EDITOR`, then `vi`.

## Packaging And Delivery

- `.cargo-tizen.toml` currently defaults to `arch = "aarch64"`,
  `profile = "mobile"`, `platform_version = "10.0"`, and package `rsdbd`.
- `tizen/rpm/rsdbd.spec` defines the RPM install behavior.
- `tizen/rpm/sources/rsdbd.service` runs `/usr/bin/rsdbd --listen 0.0.0.0:27101 --server-id rsdbd-%m`.
- `tizen/rpm/sources/rsdbd.env` currently defaults to `RUST_LOG=debug` and
  `RSDB_LOG_FILE=/var/log/rsdbd.log`.
- `tizen/rpm/sources/` currently contains checked-in versioned aarch64 and
  armv7l RPMs for `0.1.0`, `0.1.1`, `0.1.2`, and `0.1.3`.
- Fresh RPMs are built with
  `cargo tizen rpm --package rsdbd --arch <arch> --release`, which writes to
  `target/tizen/<arch>/release/rpmbuild/RPMS/<arch>/`.
- There is no checked-in release-refresh or GitHub-release upload script today.

## Scripted Operational Surface

- Development device daemon refresh:
  `./scripts/dev-update-rsdbd.sh`
- Live shell/transfer smoke:
  `./scripts/test/rsdb-regression.sh`
- Live machine-facing regression:
  `./scripts/test/rsdb-agent-regression.sh`
- Local loopback harness:
  `./scripts/dev/local-loopback.sh`,
  `./scripts/dev/local-loopback-up.sh`,
  `./scripts/dev/local-loopback-down.sh`,
  `./scripts/test/local-loopback-smoke.sh`

These scripts are part of the architecture. Agents should use them rather than
reconstructing equivalent manual workflows.

## Change Boundaries

Keep these surfaces synchronized when behavior changes:

- Wire protocol changes:
  update `crates/rsdb-proto`, `crates/rsdb-cli`, `device/rsdbd`, and the agent
  regression coverage.
- Agent command-surface changes:
  update `rsdb agent schema`, the agent regression script, and the reference doc
  in `docs/references/`.
- Package-install or release-process changes:
  update `tizen/rpm/`, `.cargo-tizen.toml` when relevant, README/docs, and any
  manual publish guidance in `AGENTS.md`.
- Loopback or smoke-test changes:
  update the relevant script and `docs/testing.md`.

## Current Hard Constraints

- Security is currently `none`. The daemon advertises no authentication and no
  transport encryption.
- There is no checked-in CI workflow in this repository today.
- There is no checked-in release automation script in this repository today.
- Documentation is now structured, but no checked-in doc linter or freshness job
  enforces it yet.
