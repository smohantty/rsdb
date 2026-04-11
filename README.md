# RSDB

RSDB is a Rust workspace for controlling a reachable `rsdbd` daemon over IP.
The current repository contains:

- `rsdb`: a host CLI for discovery, saved-target management, ping/capability
  checks, shell access, bulk push/pull, remote editing, and a machine-facing
  `agent` surface.
- `rsdb-proto`: the shared wire protocol and structured request/response types.
- `rsdbd`: the target-side daemon packaged for Tizen-style `systemd` + `rpm`
  environments.
- Shell-script entrypoints for live regression, development-device daemon
  refresh, and a local loopback harness.

`AGENTS.md` is the task router. `ARCHITECTURE.md` and `docs/` are the
repository-local system of record.

## Quickstart

Build the workspace:

```bash
cargo build --workspace
```

Run the Rust test suite:

```bash
cargo test --workspace
```

Install the host CLI:

```bash
cargo install --path crates/rsdb-cli --force
```

Run the local loopback harness:

```bash
./scripts/dev/local-loopback.sh
```

## Target Device Requirements

- `systemd`
- `rpm`
- root access for install or upgrade

## Install `rsdbd` From An RPM

The packaging inputs live under `tizen/rpm/`, and the repository currently also
contains checked-in versioned RPMs under `tizen/rpm/sources/`.

Use an RPM that matches the device architecture:

- `rsdbd-*.aarch64.rpm`
- `rsdbd-*.armv7l.rpm`

Install or upgrade it on the device:

```bash
# aarch64
rpm -Uvh rsdbd-*.aarch64.rpm

# armv7l
rpm -Uvh rsdbd-*.armv7l.rpm
```

The current package installs:

- `/usr/bin/rsdbd`
- `/usr/lib/systemd/system/rsdbd.service`
- `/etc/rsdbd.env`

On upgrade, `/etc/rsdbd.env` is preserved as an RPM `%config(noreplace)` file.
The package attempts to restart `rsdbd.service` and prints exact follow-up
commands if the service does not come back cleanly.

## Runtime Facts

- The default discovery and control port is `27101`.
- Saved targets live in `$XDG_CONFIG_HOME/rsdb/targets.json` or
  `~/.config/rsdb/targets.json`.
- The machine-facing command surface is `rsdb agent ...`.
- `rsdb agent schema` prints the current agent contract summary.
- `rsdbd` currently advertises `security: ["none"]`. There is no
  authentication or transport encryption in this codebase today.

## Primary Operational Entry Points

- Build a fresh device RPM manually:
  `cargo tizen rpm --package rsdbd --arch <aarch64|armv7l> --release`
- Update `rsdbd` on a development device:
  `./scripts/dev-update-rsdbd.sh`
- Run shell and transfer regression smoke against a device:
  `./scripts/test/rsdb-regression.sh`
- Run machine-facing agent regression smoke against a device:
  `./scripts/test/rsdb-agent-regression.sh`
- Install bash completion:
  `./scripts/install-completions.sh`

There is no checked-in GitHub release automation script in this repo today.

## Docs

- [`ARCHITECTURE.md`](ARCHITECTURE.md): top-level package and protocol map
- [`docs/index.md`](docs/index.md): documentation catalog
- [`docs/operations.md`](docs/operations.md): scripted workflows and their
  preconditions
- [`docs/testing.md`](docs/testing.md): verification matrix
- [`docs/quality.md`](docs/quality.md): current maintenance-quality snapshot
