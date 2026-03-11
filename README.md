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
- `rpm`
- writable `/usr/bin` and `/usr/lib/systemd/system`
- `systemctl`

### Host packaging tools for `rsdbd`

- `cargo-tizen`
- `rpmbuild`

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

Build a Tizen RPM for `rsdbd` on the host:

```bash
./scripts/build-rsdbd-rpm.sh aarch64
```

This produces an RPM under:

```bash
target/packages/rpm/aarch64/RPMS/aarch64/
```

The build script accepts:

- `aarch64`
- `armv7l`

On some Ubuntu hosts, `rpmbuild` may reject `armv7l` RPM emission even though `cargo tizen build -A armv7l` succeeds. In that case the script still verifies the daemon binary build and exits with a clear packaging error.

If you omit the argument, it uses the repo defaults from `.cargo-tizen.toml`:

```toml
[default]
arch = "aarch64"
profile = "tizen"
platform_version = "10.0"
provider = "rootstrap"
```

Copy the RPM to the target device and install or upgrade it:

```bash
rpm -Uvh rsdbd-*.aarch64.rpm
```

The RPM handles:

- installing `/usr/bin/rsdbd`
- installing `/usr/lib/systemd/system/rsdbd.service`
- installing `/etc/rsdbd.env` as a preserved config file
- creating `/var/log/rsdbd.log` if needed
- stopping the old service during upgrade
- `systemctl daemon-reload`
- `systemctl enable rsdbd.service`
- `systemctl restart rsdbd.service`

If you already changed `/etc/rsdbd.env` on the device, RPM keeps your existing file during upgrade.

If everything works and you want quiet operation, change `RUST_LOG=debug` to `RUST_LOG=off` in `/etc/rsdbd.env` and restart `rsdbd.service`.

### Development fallback

For ad-hoc local testing on the target, `scripts/deploy-tizen.sh` is still available as a manual fallback.

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

Connect and make a target current:

```bash
rsdb connect 192.168.0.42
```

List saved targets and show the current one:

```bash
rsdb devices
```

Read daemon capabilities:

```bash
rsdb capability
```

Open an interactive target shell:

```bash
rsdb shell
```

Run a command through the target shell:

```bash
rsdb shell uname -a
```

Push a file to the target:

```bash
rsdb push ./local.txt /tmp/remote.txt
```

Pull a file from the target:

```bash
rsdb pull /tmp/remote.txt ./remote.txt
```

Ping a target:

```bash
rsdb ping
```

## Logging

The CLI and daemon are quiet by default.

Enable logs only when debugging:

```bash
RUST_LOG=debug rsdb discover
RUST_LOG=debug rsdb shell
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
- `packaging/rpm`
  - RPM packaging assets
- `scripts`
  - deployment helpers

## Notes

- `rsdb discover` assumes one daemon per target device on port `27101`
- multiple devices can use the same port because they are identified by different IP addresses
- for local development, `discover` also probes loopback so `127.0.0.1:27101` can be found
