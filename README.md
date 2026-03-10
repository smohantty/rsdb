# RSDB

RSDB is a Rust host/target toolchain for interacting with a Tizen device over IP.

Current components:

- `rsdb`
  - host CLI for discovery, connection management, shell, push, and pull
- `rsdbd`
  - target daemon intended to run as a root `systemd` service
- `rsdb-proto`
  - shared protocol crate used by the CLI and daemon

Default port:

- TCP control and file transfer: `27101`
- UDP discovery: `27101`

## Prerequisites

### Host

- Linux host with Rust and Cargo installed
- `~/.cargo/bin` in `PATH` for user-local installs

### Target Tizen device

- root-capable environment
- `systemd`
- writable `/usr/bin` and `/etc/systemd/system`
- `install` and `systemctl`
- an `rsdbd` binary built for the target architecture
- a way to copy files onto the target device before running the deploy script

## Host Installation

Local install:

```bash
cargo install --path crates/rsdb-cli --force
```

System-wide install:

```bash
cargo install --path crates/rsdb-cli --root /usr/local --force
```

## Target Deployment

Build `rsdbd` for the Tizen target architecture on the host first:

```bash
cargo tizen build --release -- -p rsdbd
```

Copy these files to the target device into the same directory:

- `rsdbd`
- `rsdbd.service`
- `deploy-tizen.sh`

Use `scripts/deploy-tizen.sh` and the default service file from `packaging/systemd/rsdbd.service`. The service starts `rsdbd` on `0.0.0.0:27101`.

You can use any transport you want for that copy step: `scp`, `sdb`, removable media, or your own tooling.

Then, on the Tizen target device itself, run:

```bash
chmod +x ./deploy-tizen.sh
sudo ./deploy-tizen.sh
```

The default current-directory inputs are:

- `./rsdbd`
- `./rsdbd.service`

The script installs:

- binary to `/usr/bin/rsdbd`
- service file to `/usr/lib/systemd/system/rsdbd.service`
- environment file to `/etc/rsdbd.env` on first install with `RUST_LOG=debug`
- environment file default `RSDB_LOG_FILE=/var/log/rsdbd.log`
- log output to `/var/log/rsdbd.log`

If the service is already active, it stops it before reinstalling and then restarts it.
After restart, the script prints service status and recent logs so you can confirm the daemon came back successfully.
If you need different paths, edit the variables at the top of `scripts/deploy-tizen.sh`.
If everything works and you want quiet operation, change `RUST_LOG=debug` to `RUST_LOG=off` in `/etc/rsdbd.env` and restart `rsdbd.service`.

## Build

Build everything:

```bash
cargo build
```

Release build:

```bash
cargo build --release
```

## Commands

Discover devices on the local network or local loopback:

```bash
rsdb discover
```

Connect and save a target:

```bash
rsdb connect 192.168.0.42:27101 --name tv-lab
```

List saved targets:

```bash
rsdb devices
```

Read daemon capabilities:

```bash
rsdb capability --target tv-lab
```

Open an interactive target shell:

```bash
rsdb shell --target tv-lab
```

Run a command through the target shell:

```bash
rsdb shell --target tv-lab uname -a
```

Push a file to the target:

```bash
rsdb push --target tv-lab ./local.txt /tmp/remote.txt
```

Pull a file from the target:

```bash
rsdb pull --target tv-lab /tmp/remote.txt ./remote.txt
```

Ping a target:

```bash
rsdb ping --target tv-lab
```

## Logging

The CLI and daemon are quiet by default.

Enable logs only when debugging:

```bash
RUST_LOG=debug rsdb discover
RUST_LOG=debug rsdb shell --target tv-lab
RUST_LOG=debug rsdbd --listen 0.0.0.0:27101
RUST_LOG=trace rsdbd --listen 0.0.0.0:27101
```

## Repository Layout

- `crates/rsdb-cli`
  - host CLI
- `crates/rsdb-proto`
  - shared wire protocol
- `device/rsdbd`
  - target daemon
- `packaging/systemd`
  - service unit
- `scripts`
  - deployment helpers

## Notes

- `rsdb discover` assumes one daemon per target device on port `27101`
- multiple devices can use the same port because they are identified by different IP addresses
- for local development, `discover` also probes loopback so `127.0.0.1:27101` can be found
- the longer design and research notes are in [RESEARCH_AND_PLAN.md](./RESEARCH_AND_PLAN.md)
