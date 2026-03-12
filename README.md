# RSDB

RSDB lets you connect from a Linux host to a Tizen device over IP.

## Prerequisites

### Host

- Linux
- Rust and Cargo
- `~/.cargo/bin` in `PATH` if you install the CLI for your user

### Target device

- Tizen device with `systemd`
- `rpm`
- root access for package install or upgrade

## Install the CLI from source

Clone this repository, then from the repo root install the CLI:

```bash
cargo install --path crates/rsdb-cli --force
```

If you want a system-wide install instead:

```bash
sudo cargo install --path crates/rsdb-cli --root /usr/local --force
```

## Install the daemon from the release page

Open the latest release:

```text
https://github.com/smohantty/rsdb/releases/latest
```

Download the RPM that matches your device:

- `rsdbd-*.aarch64.rpm`
- `rsdbd-*.armv7l.rpm`

Copy the RPM to the target device, then install or upgrade it:

```bash
rpm -Uvh rsdbd-*.aarch64.rpm
```

Or for 32-bit ARM:

```bash
rpm -Uvh rsdbd-*.armv7l.rpm
```

The RPM:

- installs `rsdbd`
- installs and enables `rsdbd.service`
- preserves `/etc/rsdbd.env` on upgrade
- attempts to restart the service automatically

If the service does not come up after install, the RPM prints the exact commands to check status and logs.

## Notes

- The default port for discovery and control traffic is `27101`
- If you want quieter daemon logs after install, set `RUST_LOG=off` in `/etc/rsdbd.env` and restart `rsdbd.service`
