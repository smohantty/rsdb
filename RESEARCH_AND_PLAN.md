# rsdb Research and Implementation Plan

Research snapshot: 2026-03-10

## Executive Summary

Given that you control firmware and can install a root daemon with `systemd`, the best plan is:

1. Build `rsdbd` as a root Rust `systemd` service on the device.
2. Use one custom framed protocol that you own end-to-end.
3. Implement IP first over TCP with TLS and explicit pairing.
4. Add USB next, preferably by exposing USB Ethernet so the exact same socket protocol runs over both Wi-Fi and USB.
5. Only implement stock `sdbd` compatibility later if you need to support devices you do not control.

The key shift is that device-side privilege is no longer the blocker. It is now an advantage:

- Over IP on the same AP: straightforward with your own daemon.
- Over USB cable: feasible, with USB Ethernet as the preferred transport design.
- Over custom bulk USB: still possible, but only as a fallback when USB Ethernet is unavailable.

For Samsung TV specifically, official docs still matter as an operational constraint:

- same-network remote connection is the officially documented path
- USB installation is no longer supported for TV apps

That does not block a custom root daemon on your own firmware, but it does mean the custom-daemon path is a firmware-owned path, not a stock-TV deployment path.

## What Tizen SDB Actually Looks Like

### High-level architecture

Official Tizen docs describe `sdb` as a client/server/daemon system, directly analogous to `adb`:

- client on the developer machine
- server on the developer machine
- daemon on the device

The user-facing commands include:

- `devices`
- `connect <host>[:<port>]`
- `disconnect <host>[:<port>]`
- `push`
- `pull`
- `shell`
- `install`
- `uninstall`
- `forward`
- `start-server`
- `kill-server`

The official docs say TCP devices default to port `26101`.

For a custom `rsdbd`, do not reuse `26101` by default because it collides with stock `sdbd`. Use a separate default such as `27101` and keep it configurable.

### Wire protocol

The published Tizen `sdbd-3.0.50` source package shows that the transport protocol is still the classic ADB-style message protocol:

- 24-byte little-endian header
- commands `CNXN`, `OPEN`, `OKAY`, `CLSE`, `WRTE`
- host smart-socket service requests with 4-byte ASCII hex length + service string
- `OKAY` / `FAIL` host responses

Tizen-specific differences found in current source:

- protocol version is `0x02000000`
- max payload is `256 * 1024`
- extra packet types exist: `A_STAT` and `A_ENCR`
- extra local services exist: `capability:`, `sysinfo:`, `appcmd:`, `root:`, `grantfile:`, `tzplatformenv:`

Important mismatch:

- inherited protocol docs inside the source still mention host server port `5037`
- current Tizen source defines `DEFAULT_SDB_PORT` as `26099`

That means you should treat the ADB-style protocol semantics as authoritative, but treat port numbers as implementation details that must stay configurable.

### TCP transport

The same Tizen source package shows:

- `sdbd_tcp.socket` listens on `26101`
- peer IP validation is delegated to a plugin
- default open-source plugin accepts IPv4 peers, rejects pure IPv6 peers

So for real-world compatibility, assume:

- IPv4 LAN support matters first
- IPv6 is not guaranteed
- TV/OEM builds may override policy through plugins

### USB transport

The Tizen source package also shows the USB side is not an app-level socket:

- `sdbd.socket` uses `ListenUSBFunction=/dev/usb-funcs/sdb/default`
- `sdbd.service` uses `USBFunctionDescriptors=/etc/sdbd/descs`
- the daemon writes FunctionFS descriptors and strings

The current USB interface values in source are:

- class `0xff`
- subclass `0x20`
- protocol `0x02`
- two bulk endpoints
- OUT endpoint address `0x01`
- IN endpoint address `0x82`

This is the right shape for a Linux host implementation:

- enumerate interfaces by class/subclass/protocol
- claim the matching interface
- read/write the bulk endpoints

But this also means the device side is kernel/systemd/functionfs territory, not normal Tizen app territory.

### Security posture in the open-source default plugin

The open-source default plugin in `sdbd-3.0.50` is permissive and incomplete:

- secure protocol capability defaults to disabled
- auth helper stubs return failure
- IPv4 peer verification returns valid in the default plugin
- pure IPv6 verification returns invalid in the default plugin

I am inferring from this that shipping products rely on OEM or profile-specific plugin behavior for actual policy, especially TV builds. You should not copy this model into a new design. Use explicit authentication and encryption from the start.

## What Is Feasible on the Tizen Device

### Option A: privileged system daemon

This is the only option that can realistically become a full `sdb` replacement:

- USB gadget support
- broad shell execution
- full filesystem access
- app/package lifecycle hooks
- root or elevated operations

This matches how official `sdbd` is packaged and launched.

Use this if you control:

- the device image
- root access
- partner/platform privileges
- OEM firmware integration

### Option B: native service app

This is suitable for a same-AP IP agent, but not for a full `sdb` replacement.

It is useful for:

- TCP/TLS RPC
- app-specific automation
- file exchange within allowed paths
- discovery via DNS-SD

It is not a credible path to USB gadget support on stock devices.

I am also skeptical that it can safely or portably provide a true `sdb shell` equivalent on stock devices, because `sdbd` itself is packaged as a privileged system service with special capabilities and Smack labeling.

### TV-specific constraint

Samsung TV docs say:

- the TV and host must be on the same network
- you enable Developer Mode on the TV
- you enter the host PC IP on the TV
- Remote Device Manager connects using the device IP and port
- port `26101` is an internal device port
- installing applications or widgets through USB is no longer supported

If your target is Samsung TV, IP-first is not just convenient, it is the officially supported direction.

## Recommended Product Direction

### Recommended scope decision

Use a custom two-ended stack as the primary architecture:

1. `rsdbd`: root Rust daemon on Tizen, launched by `systemd`
2. `rsdb`: Rust host CLI and optional local host daemon on Linux

Primary transport order:

1. TCP/TLS over LAN
2. USB Ethernet carrying the exact same TCP/TLS protocol
3. custom bulk USB only if USB Ethernet is not available on the device image

This is the cleanest design because:

- you control protocol evolution
- you avoid inheriting undocumented `sdbd` quirks
- you can keep one protocol across Wi-Fi and USB
- authentication and authorization can be designed correctly from the start

### What to avoid

Do not optimize for stock `sdbd` compatibility unless you actually need stock-device support.

Reasons:

- it forces legacy protocol constraints into a system you own
- it makes USB harder if you try to mimic Tizen's bulk USB function exactly
- it encourages reverse-engineering instead of designing a cleaner transport

### Compatibility fallback

Keep a compatibility path in reserve:

- if later you need to talk to stock `sdbd`, build a separate compatibility transport/client layer
- do not make that compatibility layer the foundation of the new architecture

## Proposed Architecture

### Host side

Rust components:

- `rsdb-cli`
  - user commands
  - human-readable output
- `rsdb-proto`
  - custom framed protocol
  - multiplexed channels
  - request/response and streaming semantics
- `rsdb-auth`
  - pairing
  - trust store
  - session bootstrap
- `rsdb-transport-socket`
  - TCP/TLS transport
  - used for both Wi-Fi and USB Ethernet
- `rsdb-transport-usb-bulk`
  - optional fallback transport if USB Ethernet is unavailable
- `rsdb-fs`
  - file transfer and metadata operations
- `rsdb-shell`
  - interactive and non-interactive shell streams
- `rsdb-portfwd`
  - local/remote forwarding
- `rsdb-discovery`
  - optional mDNS / DNS-SD discovery on LAN
- `rsdb-hostd`
  - optional local host daemon for multi-client use
  - not required for the first milestone

Recommended Rust stack:

- `tokio`
- `bytes`
- `tokio-rustls` or `rustls`
- `nusb` or `rusb` only if bulk USB fallback is implemented
- `clap`
- `tracing`

### Device side

Primary form:

- `rsdbd` as a privileged native daemon running as root under `systemd`

Recommended device responsibilities:

- TCP listener on `27101` by default, configurable
- TLS termination and pairing enforcement
- optional DNS-SD advertisement
- command dispatch
- shell execution with PTY support
- file transfer service
- capability reporting
- app/package control service if needed
- forwarding endpoints
- audit logging

Recommended device integration:

- `rsdbd.service` under `systemd`
- optional USB gadget network setup service if you use USB Ethernet

USB strategies if you own firmware:

- preferred: USB Ethernet gadget
- fallback: custom FunctionFS or bulk USB transport

## Protocol and CLI Target

### Wire protocol target

Use a custom framed protocol with:

- version negotiation
- authenticated session establishment
- multiplexed channels
- request IDs
- streaming payload support
- explicit error frames
- heartbeat or keepalive

Recommended logical channels:

- `control`
- `shell`
- `fs`
- `forward`
- `app`
- `events`

### CLI target

Keep the user-facing CLI close to `adb`/`sdb` where it helps:

- `connect`
- `disconnect`
- `devices`
- `capability`
- `shell`
- `push`
- `pull`
- `forward`

Then add:

- `install`
- `uninstall`
- `dlog`
- `sysinfo`
- `events`
- `pair`

You can preserve familiar CLI semantics without preserving `sdbd` wire compatibility.

## USB Design

### Preferred USB plan: USB Ethernet

Preferred design:

1. expose a USB gadget network interface from Tizen
2. bring up a point-to-point or small private subnet over USB
3. run the same `rsdbd` TCP/TLS listener on that interface
4. let the Linux host talk to `rsdbd` with normal sockets

Why this is preferred:

- one protocol for Wi-Fi and USB
- no separate host USB packet engine
- easier tracing, testing, and debugging
- easier retry logic and connection management
- better reuse of host and daemon code

Recommended link setup options:

- static IPv4 on `usb0` plus host-side static peer
- link-local IPv6 if your environment supports it cleanly
- DHCP on the USB gadget link if you want dynamic configuration

### Fallback USB plan: custom bulk USB

Use this only if USB Ethernet is impossible on the target image.

In that design:

- the device exposes a custom USB function
- the host claims the interface directly
- the same logical protocol is carried over USB bulk reads and writes

This is still feasible, but more work on both sides.

### Device-side USB reality

Even with root and `systemd`, USB support still requires device-image integration:

- gadget driver or FunctionFS setup
- interface configuration
- service startup ordering
- network or endpoint initialization

Root privilege makes this possible. It does not make it automatic.

## Network Design

### Baseline

For LAN and USB Ethernet communication:

- device listener default `27101`
- IPv4 first
- keepalive enabled
- manual `connect <ip>:27101`

### Discovery

For better UX, advertise a service like:

- `_rsdb._tcp`

using Tizen DNSSD APIs on the device side and mDNS browsing on Linux.

### Security

Recommended minimum:

- pairing code on first connect
- device-generated Ed25519 key
- host-generated Ed25519 key
- trust store on both sides
- TLS for all socket-based transports
- the same auth model for Wi-Fi and USB Ethernet
- explicit authorization policy on privileged operations

## Phased Implementation Plan

### Phase 0: protocol and daemon skeleton

Goal:

- define the protocol and prove the daemon/host handshake on your own stack

Tasks:

- define frame format and channel model
- create `rsdbd` root daemon skeleton
- create `rsdb` host CLI skeleton
- implement TCP listener and TCP client
- implement version negotiation
- implement pairing and session bootstrap

Exit criteria:

- `rsdb connect <ip>`
- secure session established
- `rsdb devices` sees the target

### Phase 1: IP transport and core commands

Tasks:

- implement `shell`
- implement `capability`
- implement `push`
- implement `pull`
- implement `forward`
- implement PTY support for interactive shell
- implement structured daemon capability reporting

Exit criteria:

- Wi-Fi or LAN workflow is usable end-to-end
- reliable file transfer
- port forwarding works with gdbserver-style workflow
- interactive shell is stable

### Phase 2: USB Ethernet transport

Tasks:

- configure USB gadget networking on Tizen
- assign device and host link addresses
- expose `rsdbd` on the USB network interface
- add host-side USB-network detection and connect helpers
- verify the exact same TCP/TLS protocol works over USB

Exit criteria:

- `rsdb devices` shows USB-network-connected targets
- `shell` and `push/pull` work over USB
- no protocol fork exists between Wi-Fi and USB

### Phase 3: host daemon and discovery

Tasks:

- add `rsdb-hostd`
- local API between CLI and host daemon
- multi-device tracking
- DNS-SD discovery for LAN targets
- friendlier connect flows for USB-network targets

Exit criteria:

- CLI talks to local daemon
- multiple concurrent clients work

### Phase 4: optional bulk USB fallback

Tasks:

- implement custom USB function on the device
- implement host-side USB interface discovery
- map the same logical protocol onto bulk USB transport
- add hotplug and recovery handling

Exit criteria:

- USB works on targets where USB Ethernet is unavailable
- higher layers remain unchanged

### Phase 5: hardening

Tasks:

- pairing and trust model
- TLS or equivalent secure channel
- audit logging
- rate limits
- idle timeout
- recovery from network flaps
- `systemd` restart and boot integration validation

### Phase 6: optional stock `sdbd` compatibility

Only do this if you later need unmanaged-device support.

Tasks:

- build a separate `sdb-compat` module
- implement Tizen `sdbd` packet and service compatibility
- keep it isolated from the primary custom protocol stack

Exit criteria:

- stock devices are supported without contaminating the main architecture

## Recommended Repo Layout

```text
rsdb/
  Cargo.toml
  crates/
    rsdb-cli/
    rsdb-hostd/
    rsdb-proto/
    rsdb-auth/
    rsdb-transport-socket/
    rsdb-transport-usb-bulk/
    rsdb-fs/
    rsdb-shell/
    rsdb-portfwd/
    rsdb-discovery/
    rsdb-capability/
  device/
    rsdbd/
  packaging/
    systemd/
      rsdbd.service
    usb/
      gadget-network.sh
      gadget-bulk.sh
  docs/
    protocol-notes.md
    device-daemon-design.md
    usb-ethernet-plan.md
    sdb-compat.md
```

## Testing Plan

### Host protocol tests

- packet encode/decode golden tests
- framing and multiplexing tests
- session bootstrap tests
- file transfer fixture tests

### Integration tests

- TCP/TLS against a real Tizen device
- USB Ethernet against a real Tizen device
- boot-time service startup on target

### Transport tests

- verify identical protocol behavior over Wi-Fi and USB Ethernet
- reconnect after cable unplug or Wi-Fi drop
- host-side interface churn handling

### Failure tests

- cable unplug during transfer
- Wi-Fi drop during shell
- reconnect after daemon restart
- malformed packet handling

### Optional compatibility tests

If `sdb` compatibility is later added:

- compare behavior against stock `sdbd`
- keep compatibility fixtures separate from core protocol fixtures

## Key Risks

### Risk 1: USB gadget network support on target image

USB Ethernet is the cleanest design, but it depends on gadget support and device-image plumbing.

Mitigation:

- validate USB gadget networking on the target image early
- keep bulk USB as a fallback, not the primary plan

### Risk 2: privileged daemon attack surface

A root daemon with shell and file operations is high-risk if authentication is weak.

Mitigation:

- enforce pairing and TLS from the start
- add explicit authorization gates for dangerous operations
- keep audit logs

### Risk 3: over-designing the protocol too early

A custom protocol is an advantage, but it can drift into unnecessary complexity.

Mitigation:

- start with a minimal framed protocol
- make channels and message types additive
- version everything

### Risk 4: later stock-device support

If you later need stock-device support, the custom protocol will not help on its own.

Mitigation:

- isolate compatibility work into a separate module
- do not leak stock `sdbd` assumptions into the main protocol

## Concrete Recommendation

The best first milestone is now "write your own root `rsdbd` and keep one protocol across all socket-based links".

It is:

1. implement `rsdbd` as a root `systemd` service
2. implement a custom TCP/TLS protocol and host CLI
3. add `capability`, `shell`, `push`, `pull`, `forward`
4. expose the same daemon over USB Ethernet
5. only add custom bulk USB if USB Ethernet is not viable on the target image

That gives you one architecture, one security model, and ideally one transport protocol for both Wi-Fi and USB.

## Useful Local Context

Adjacent projects in this workspace are directly relevant:

- `cargo-tizen` can help cross-build Rust binaries for Tizen
- `tizen-tool-service` is a concrete example of a privileged Tizen-side remote service pattern

## Sources

- Tizen SDB overview and commands: https://developer.tizen.org/sdb/
- Tizen Studio Smart Development Bridge doc: https://docs.tizen.org/application/tizen-studio/common-tools/smart-development-bridge/
- Samsung TV device connection guide: https://developer.samsung.com/smarttv/develop/getting-started/using-sdk/tv-device.html
- Android adb command guide: https://developer.android.com/tools/adb
- AOSP adb overview: https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/docs/dev/overview.md
- AOSP adb protocol: https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/docs/dev/protocol.md
- AOSP adb sync protocol: https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/docs/dev/sync.md
- AOSP adb services: https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/docs/dev/services.md
- Official Tizen `sdbd` source package inspected for this research: https://download.tizen.org/snapshots/TIZEN/Tizen/Tizen-Unified/tizen-unified_20260305.120015/repos/standard_gcov/source/sdbd-3.0.50-0.src.rpm
- Tizen native service application API: https://docs.tizen.org/application/native/api/mobile/latest/group__CAPI__SERVICE__APP__MODULE.html
- Tizen native network framework API: https://docs.tizen.org/application/native/api/mobile/latest/group__CAPI__NETWORK__FRAMEWORK.html
- Tizen native DNS-SD API: https://docs.tizen.org/application/native/api/mobile/latest/group__CAPI__NETWORK__DNSSD__MODULE.html
- Tizen native USB host API: https://docs.tizen.org/application/native/api/mobile/latest/group__CAPI__USB__HOST__MODULE.html
