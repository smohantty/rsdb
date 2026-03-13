#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
PORT=27131
RUN_DIR=""
WAIT_SECS=15
SERVER_ID="rsdbd-local"
PRINT_RUN_DIR=0

usage() {
    cat <<'EOF'
usage: ./scripts/dev/local-loopback-up.sh [--port <port>] [--run-dir <path>] [--wait-secs <seconds>] [--server-id <id>] [--print-run-dir]

Builds the local rsdb and rsdbd binaries, starts rsdbd on loopback, waits for
the daemon to answer ping, and writes an isolated env file under the run dir.
EOF
}

require_command() {
    local command_name="$1"

    if ! command -v "$command_name" >/dev/null 2>&1; then
        echo "error: $command_name is required" >&2
        exit 1
    fi
}

wait_for_ready() {
    local rsdb_bin="$1"
    local target="$2"
    local pid="$3"
    local log_file="$4"
    local deadline=$((SECONDS + WAIT_SECS))

    while (( SECONDS < deadline )); do
        if ! kill -0 "$pid" >/dev/null 2>&1; then
            echo "error: local rsdbd exited before becoming ready" >&2
            echo "log: $log_file" >&2
            exit 1
        fi

        if "$rsdb_bin" ping --target "$target" >/dev/null 2>&1; then
            return 0
        fi

        sleep 1
    done

    echo "error: local rsdbd did not become ready within $WAIT_SECS seconds" >&2
    echo "next check: $rsdb_bin ping --target $target" >&2
    echo "log: $log_file" >&2
    exit 1
}

write_env_file() {
    local env_file="$1"
    local rsdb_bin="$2"
    local rsdbd_bin="$3"
    local run_dir="$4"
    local config_home="$5"
    local target="$6"
    local port="$7"
    local log_file="$8"
    local pid_file="$9"

    cat >"$env_file" <<EOF
#!/usr/bin/env bash
export RSDB_BIN=$(printf '%q' "$rsdb_bin")
export RSDBD_BIN=$(printf '%q' "$rsdbd_bin")
export RSDB_TARGET=$(printf '%q' "$target")
export RSDB_LOOPBACK_PORT=$(printf '%q' "$port")
export RSDB_LOOPBACK_RUN_DIR=$(printf '%q' "$run_dir")
export RSDB_LOOPBACK_LOG=$(printf '%q' "$log_file")
export RSDB_LOOPBACK_PID_FILE=$(printf '%q' "$pid_file")
export XDG_CONFIG_HOME=$(printf '%q' "$config_home")
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --port)
            if [[ $# -lt 2 ]]; then
                echo "error: --port requires a value" >&2
                exit 1
            fi
            PORT="$2"
            shift 2
            ;;
        --run-dir)
            if [[ $# -lt 2 ]]; then
                echo "error: --run-dir requires a value" >&2
                exit 1
            fi
            RUN_DIR="$2"
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
        --server-id)
            if [[ $# -lt 2 ]]; then
                echo "error: --server-id requires a value" >&2
                exit 1
            fi
            SERVER_ID="$2"
            shift 2
            ;;
        --print-run-dir)
            PRINT_RUN_DIR=1
            shift
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
require_command mktemp

if [[ -z "$RUN_DIR" ]]; then
    RUN_DIR="$(mktemp -d /tmp/rsdb-loopback-XXXXXX)"
else
    mkdir -p "$RUN_DIR"
fi

RUN_DIR="$(cd "$RUN_DIR" && pwd)"
CONFIG_HOME="$RUN_DIR/config"
LOG_FILE="$RUN_DIR/daemon.log"
PID_FILE="$RUN_DIR/daemon.pid"
ENV_FILE="$RUN_DIR/env.sh"
TARGET="127.0.0.1:$PORT"

if [[ -e "$PID_FILE" ]]; then
    echo "error: loopback run dir already contains a pid file: $PID_FILE" >&2
    exit 1
fi

mkdir -p "$CONFIG_HOME"

cd "$ROOT_DIR"

echo "Building local rsdb and rsdbd" >&2
cargo build --bin rsdb --bin rsdbd

RSDB_BIN="$ROOT_DIR/target/debug/rsdb"
RSDBD_BIN="$ROOT_DIR/target/debug/rsdbd"

if [[ ! -x "$RSDB_BIN" ]]; then
    echo "error: rsdb binary not found after build: $RSDB_BIN" >&2
    exit 1
fi

if [[ ! -x "$RSDBD_BIN" ]]; then
    echo "error: rsdbd binary not found after build: $RSDBD_BIN" >&2
    exit 1
fi

echo "Starting local rsdbd on $TARGET" >&2
"$RSDBD_BIN" --listen "$TARGET" --server-id "$SERVER_ID" >"$LOG_FILE" 2>&1 &
DAEMON_PID=$!
printf '%s\n' "$DAEMON_PID" >"$PID_FILE"

wait_for_ready "$RSDB_BIN" "$TARGET" "$DAEMON_PID" "$LOG_FILE"
write_env_file "$ENV_FILE" "$RSDB_BIN" "$RSDBD_BIN" "$RUN_DIR" "$CONFIG_HOME" "$TARGET" "$PORT" "$LOG_FILE" "$PID_FILE"

if [[ "$PRINT_RUN_DIR" -eq 1 ]]; then
    printf '%s\n' "$RUN_DIR"
    exit 0
fi

echo "Local loopback daemon is ready"
echo "run_dir: $RUN_DIR"
echo "target: $TARGET"
echo "pid: $DAEMON_PID"
echo "log: $LOG_FILE"
echo "env: $ENV_FILE"
