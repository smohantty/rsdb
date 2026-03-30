#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET=""
WAIT_SECS=60

usage() {
    cat <<'EOF'
usage: ./scripts/dev-update-rsdbd.sh [--target <ip[:port]>] [--wait-secs <seconds>]

Uses the currently working rsdb CLI to detect the target architecture, build the
matching rsdbd RPM, push it to the device, schedule a detached install, rebuild
and install the local rsdb CLI, then verify the new CLI works with the updated
daemon.
EOF
}

require_command() {
    local command_name="$1"

    if ! command -v "$command_name" >/dev/null 2>&1; then
        echo "error: $command_name is required" >&2
        exit 1
    fi
}

workspace_version() {
    awk '
        $0 == "[workspace.package]" { in_workspace = 1; next }
        /^\[/ { in_workspace = 0 }
        in_workspace && $1 == "version" {
            gsub(/"/, "", $3)
            print $3
            exit
        }
    ' "$ROOT_DIR/Cargo.toml"
}

normalize_arch() {
    local raw_arch="$1"

    case "$raw_arch" in
        aarch64|arm64)
            echo "aarch64"
            ;;
        armv7l|armv7hl|armv7*)
            echo "armv7l"
            ;;
        *)
            echo "error: unsupported remote architecture: $raw_arch" >&2
            exit 1
            ;;
    esac
}

service_start_marker() {
    local rsdb_bin="$1"
    shift
    local subcommand="$1"
    shift

    if [[ -n "$TARGET" ]]; then
        "$rsdb_bin" "$subcommand" --target "$TARGET" "$@" \
            | tr -d '\r' \
            | awk 'NF { value = $0 } END { print value }'
    else
        "$rsdb_bin" "$subcommand" "$@" \
            | tr -d '\r' \
            | awk 'NF { value = $0 } END { print value }'
    fi
}

deploy_rsdb() {
    local subcommand="$1"
    shift

    if [[ -n "$TARGET" ]]; then
        "$RSDB_DEPLOY_BIN" "$subcommand" --target "$TARGET" "$@"
    else
        "$RSDB_DEPLOY_BIN" "$subcommand" "$@"
    fi
}

test_rsdb() {
    local subcommand="$1"
    shift

    if [[ -n "$TARGET" ]]; then
        "$RSDB_TEST_BIN" "$subcommand" --target "$TARGET" "$@"
    else
        "$RSDB_TEST_BIN" "$subcommand" "$@"
    fi
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)
            if [[ $# -lt 2 ]]; then
                echo "error: --target requires a value" >&2
                exit 1
            fi
            TARGET="$2"
            shift 2
            ;;
        --wait-secs)
            if [[ $# -lt 2 ]]; then
                echo "error: --wait-secs requires a value" >&2
                exit 1
            fi
            WAIT_SECS="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

require_command cargo
require_command awk
require_command rsdb
require_command date

if ! cargo tizen --version >/dev/null 2>&1; then
    echo "error: cargo-tizen is required" >&2
    exit 1
fi

cd "$ROOT_DIR"

RSDB_DEPLOY_BIN="$(command -v rsdb)"
CLI_VERSION="$(workspace_version)"
if [[ -z "$CLI_VERSION" ]]; then
    echo "error: failed to read workspace version from Cargo.toml" >&2
    exit 1
fi

echo "Building local rsdb CLI"
cargo build --release -p rsdb-cli

echo "Detecting remote architecture"
RAW_ARCH="$(
    deploy_rsdb shell uname -m \
        | tr -d '\r' \
        | awk 'NF { value = $0 } END { print value }'
)"

if [[ -z "$RAW_ARCH" ]]; then
    echo "error: failed to determine remote architecture with \`rsdb shell uname -m\`" >&2
    exit 1
fi

TARGET_ARCH="$(normalize_arch "$RAW_ARCH")"
echo "Remote architecture: $RAW_ARCH -> $TARGET_ARCH"

echo "Capturing current rsdbd.service start marker"
BEFORE_START_MARKER="$(
    service_start_marker "$RSDB_DEPLOY_BIN" shell \
        systemctl show -p ActiveEnterTimestampMonotonic --value rsdbd.service
)"

echo "Building rsdbd RPM for $TARGET_ARCH"
(cd "$ROOT_DIR" && cargo tizen rpm --package rsdbd --arch "$TARGET_ARCH" --release)

RPM_PATH="$ROOT_DIR/target/tizen/$TARGET_ARCH/release/rpmbuild/RPMS/$TARGET_ARCH/rsdbd-$CLI_VERSION-1.$TARGET_ARCH.rpm"
if [[ ! -f "$RPM_PATH" ]]; then
    echo "error: expected RPM not found: $RPM_PATH" >&2
    exit 1
fi

REMOTE_RPM_PATH="/tmp/rsdbd-$CLI_VERSION-dev-$TARGET_ARCH.rpm"
UNIT_NAME="rsdbd-upgrade-$(date +%s)"
REMOTE_INSTALL_COMMAND="rpm -Uvh --force \"$REMOTE_RPM_PATH\"; status=\$?; rm -f \"$REMOTE_RPM_PATH\"; exit \$status"

echo "Pushing RPM to $REMOTE_RPM_PATH"
deploy_rsdb push "$RPM_PATH" "$REMOTE_RPM_PATH"

echo "Scheduling detached install with systemd-run ($UNIT_NAME)"
deploy_rsdb shell \
    systemd-run \
    --unit "$UNIT_NAME" \
    --quiet \
    --collect \
    --service-type=oneshot \
    --no-block \
    /bin/sh \
    -lc \
    "$REMOTE_INSTALL_COMMAND"

echo "Installing updated rsdb CLI locally"
cargo install --path crates/rsdb-cli --force

CARGO_BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
if [[ -x "$CARGO_BIN_DIR/rsdb" ]]; then
    RSDB_TEST_BIN="$CARGO_BIN_DIR/rsdb"
else
    RSDB_TEST_BIN="$(command -v rsdb)"
fi

if [[ -z "${RSDB_TEST_BIN:-}" || ! -x "$RSDB_TEST_BIN" ]]; then
    echo "error: updated rsdb binary not found after cargo install" >&2
    exit 1
fi

echo "Waiting up to $WAIT_SECS seconds for the updated daemon"
DEADLINE=$((SECONDS + WAIT_SECS))
while (( SECONDS < DEADLINE )); do
    if test_rsdb ping >/dev/null 2>&1 && test_rsdb shell true >/dev/null 2>&1; then
        AFTER_START_MARKER="$(
            service_start_marker "$RSDB_TEST_BIN" shell \
                systemctl show -p ActiveEnterTimestampMonotonic --value rsdbd.service
        )"
        if [[ -n "$BEFORE_START_MARKER" && "$AFTER_START_MARKER" == "$BEFORE_START_MARKER" ]]; then
            echo "error: daemon responded again, but rsdbd.service does not appear to have restarted" >&2
            echo "next check: $RSDB_TEST_BIN shell systemctl status rsdbd.service --no-pager -l" >&2
            exit 1
        fi
        echo "Development update complete"
        echo "target_arch: $TARGET_ARCH"
        echo "rpm: $RPM_PATH"
        echo "rsdb: $RSDB_TEST_BIN"
        echo "service_start_marker: ${BEFORE_START_MARKER:-unknown} -> ${AFTER_START_MARKER:-unknown}"
        exit 0
    fi
    sleep 1
done

echo "error: daemon did not become ready within $WAIT_SECS seconds" >&2
echo "next check: $RSDB_TEST_BIN devices" >&2
if [[ -n "$TARGET" ]]; then
    echo "next check: $RSDB_TEST_BIN ping --target $TARGET" >&2
    echo "next check: $RSDB_TEST_BIN shell --target $TARGET true" >&2
else
    echo "next check: $RSDB_TEST_BIN ping" >&2
    echo "next check: $RSDB_TEST_BIN shell true" >&2
fi
exit 1
