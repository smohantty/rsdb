#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
UP_SCRIPT="$ROOT_DIR/scripts/dev/local-loopback-up.sh"
DOWN_SCRIPT="$ROOT_DIR/scripts/dev/local-loopback-down.sh"
REGRESSION_SCRIPT="$ROOT_DIR/scripts/test/rsdb-regression.sh"
AGENT_REGRESSION_SCRIPT="$ROOT_DIR/scripts/test/rsdb-agent-regression.sh"
RUN_DIR="${RSDB_LOOPBACK_RUN_DIR:-}"
PORT=27131
SPAWN_DAEMON=0
KEEP_DAEMON=0
KEEP_TEMP=0
RUN_WORKSPACE_TESTS=1

usage() {
    cat <<'EOF'
usage: ./scripts/test/local-loopback-smoke.sh [--spawn-daemon] [--run-dir <path>] [--port <port>] [--keep-daemon] [--keep-temp] [--no-workspace-tests]

Runs the workspace unit tests plus the existing rsdb shell/transfer and agent
regression smoke scripts against a local loopback rsdbd instance.
EOF
}

cleanup() {
    local status="$1"

    if [[ "$status" -ne 0 && -n "${RSDB_LOOPBACK_LOG:-}" ]]; then
        echo "loopback log: $RSDB_LOOPBACK_LOG" >&2
    fi

    if [[ "$SPAWN_DAEMON" -eq 1 && "$KEEP_DAEMON" -eq 0 && -n "$RUN_DIR" ]]; then
        if [[ "$KEEP_TEMP" -eq 1 || "$status" -ne 0 ]]; then
            "$DOWN_SCRIPT" --run-dir "$RUN_DIR" --keep-temp >/dev/null
        else
            "$DOWN_SCRIPT" --run-dir "$RUN_DIR" >/dev/null
        fi
    fi
}

load_env() {
    local run_dir="$1"
    local env_file="$run_dir/env.sh"

    if [[ ! -f "$env_file" ]]; then
        echo "error: loopback env file not found: $env_file" >&2
        exit 1
    fi

    # shellcheck disable=SC1090
    . "$env_file"
    export RSDB_BIN RSDBD_BIN RSDB_TARGET RSDB_LOOPBACK_PORT RSDB_LOOPBACK_RUN_DIR RSDB_LOOPBACK_LOG RSDB_LOOPBACK_PID_FILE XDG_CONFIG_HOME
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --spawn-daemon)
            SPAWN_DAEMON=1
            shift
            ;;
        --run-dir)
            if [[ $# -lt 2 ]]; then
                echo "error: --run-dir requires a value" >&2
                exit 1
            fi
            RUN_DIR="$2"
            shift 2
            ;;
        --port)
            if [[ $# -lt 2 ]]; then
                echo "error: --port requires a value" >&2
                exit 1
            fi
            PORT="$2"
            shift 2
            ;;
        --keep-daemon)
            KEEP_DAEMON=1
            shift
            ;;
        --keep-temp)
            KEEP_TEMP=1
            shift
            ;;
        --no-workspace-tests)
            RUN_WORKSPACE_TESTS=0
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

trap 'status=$?; cleanup "$status"; exit "$status"' EXIT

if [[ "$SPAWN_DAEMON" -eq 1 ]]; then
    UP_ARGS=(--port "$PORT" --print-run-dir)
    if [[ -n "$RUN_DIR" ]]; then
        UP_ARGS+=(--run-dir "$RUN_DIR")
    fi
    RUN_DIR="$("$UP_SCRIPT" "${UP_ARGS[@]}")"
fi

if [[ -z "$RUN_DIR" ]]; then
    echo "error: pass --spawn-daemon or --run-dir <path>, or set RSDB_LOOPBACK_RUN_DIR" >&2
    exit 1
fi

RUN_DIR="$(cd "$RUN_DIR" && pwd)"
load_env "$RUN_DIR"

if [[ ! -x "$RSDB_BIN" ]]; then
    echo "error: rsdb binary is missing or not executable: $RSDB_BIN" >&2
    exit 1
fi

if [[ "$RUN_WORKSPACE_TESTS" -eq 1 ]]; then
    echo "Running workspace unit tests"
    cargo test --workspace
fi

echo "Checking local loopback daemon"
"$RSDB_BIN" ping --target "$RSDB_TARGET" >/dev/null

echo "Running rsdb shell and transfer regression smoke"
RSDB_BIN="$RSDB_BIN" "$REGRESSION_SCRIPT" --target "$RSDB_TARGET"

echo "Running rsdb agent regression smoke"
RSDB_BIN="$RSDB_BIN" "$AGENT_REGRESSION_SCRIPT" --target "$RSDB_TARGET"

echo "PASS"
echo "run_dir: $RUN_DIR"
echo "target: $RSDB_TARGET"
echo "log: $RSDB_LOOPBACK_LOG"
