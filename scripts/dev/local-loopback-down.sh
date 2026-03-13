#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_DIR="${RSDB_LOOPBACK_RUN_DIR:-}"
KEEP_TEMP=0

usage() {
    cat <<'EOF'
usage: ./scripts/dev/local-loopback-down.sh [--run-dir <path>] [--keep-temp]

Stops a local loopback rsdbd process started by local-loopback-up.sh and removes
its run directory unless --keep-temp is set.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --run-dir)
            if [[ $# -lt 2 ]]; then
                echo "error: --run-dir requires a value" >&2
                exit 1
            fi
            RUN_DIR="$2"
            shift 2
            ;;
        --keep-temp)
            KEEP_TEMP=1
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

if [[ -z "$RUN_DIR" ]]; then
    echo "error: pass --run-dir <path> or set RSDB_LOOPBACK_RUN_DIR" >&2
    exit 1
fi

RUN_DIR="$(cd "$RUN_DIR" && pwd)"
PID_FILE="$RUN_DIR/daemon.pid"

if [[ -f "$PID_FILE" ]]; then
    DAEMON_PID="$(tr -d '[:space:]' <"$PID_FILE")"
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" >/dev/null 2>&1; then
        kill "$DAEMON_PID" >/dev/null 2>&1 || true
        for _ in 1 2 3 4 5; do
            if ! kill -0 "$DAEMON_PID" >/dev/null 2>&1; then
                break
            fi
            sleep 1
        done
        if kill -0 "$DAEMON_PID" >/dev/null 2>&1; then
            kill -KILL "$DAEMON_PID" >/dev/null 2>&1 || true
        fi
    fi
    rm -f "$PID_FILE"
fi

if [[ "$KEEP_TEMP" -eq 0 ]]; then
    rm -rf "$RUN_DIR"
    echo "Local loopback daemon stopped and removed: $RUN_DIR"
else
    echo "Local loopback daemon stopped"
    echo "run_dir: $RUN_DIR"
fi
