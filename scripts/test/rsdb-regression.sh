#!/usr/bin/env bash
set -euo pipefail

TARGET=""
KEEP_TEMP=0
RSDB_BIN="${RSDB_BIN:-rsdb}"

usage() {
    cat <<'EOF'
usage: ./scripts/test/rsdb-regression.sh [--target <ip[:port]>] [--keep-temp]

Runs a live regression smoke test against an rsdb-connected device:
- multi-source push
- roundtrip pull verification
- independent multi-source pull verification
- interactive shell smoke via PTY
- repeated short shell and ping calls
EOF
}

require_command() {
    local command_name="$1"

    if ! command -v "$command_name" >/dev/null 2>&1; then
        echo "error: $command_name is required" >&2
        exit 1
    fi
}

rsdb_cmd() {
    local subcommand="$1"
    shift

    if [[ -n "$TARGET" ]]; then
        "$RSDB_BIN" "$subcommand" --target "$TARGET" "$@"
    else
        "$RSDB_BIN" "$subcommand" "$@"
    fi
}

elapsed_ms() {
    local start_ns="$1"
    local end_ns

    end_ns="$(date +%s%N)"
    echo $(((end_ns - start_ns) / 1000000))
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

require_command "$RSDB_BIN"
require_command diff
require_command grep
require_command mktemp
require_command script
require_command seq
require_command head

LOCAL_ROOT="$(mktemp -d /tmp/rsdb-regression-XXXXXX)"
REMOTE_PUSH_ROOT="/tmp/rsdb-regression-push-$(date +%s)"
REMOTE_PULL_ROOT="/tmp/rsdb-regression-pull-$(date +%s)"
ROUNDTRIP_DIR="$LOCAL_ROOT/roundtrip"
PULL_DIR="$LOCAL_ROOT/pull-result"
EXPECTED_PULL_DIR="$LOCAL_ROOT/expected-pull"
INTERACTIVE_LOG="$LOCAL_ROOT/interactive.log"

cleanup() {
    rsdb_cmd shell rm -rf "$REMOTE_PUSH_ROOT" "$REMOTE_PULL_ROOT" >/dev/null 2>&1 || true
    if [[ "$KEEP_TEMP" -eq 0 ]]; then
        rm -rf "$LOCAL_ROOT"
    fi
}

trap cleanup EXIT

rsdb_cmd ping >/dev/null

mkdir -p "$LOCAL_ROOT/src/alpha/sub" "$LOCAL_ROOT/src/beta" "$LOCAL_ROOT/src/emptydir"
mkdir -p "$ROUNDTRIP_DIR" "$PULL_DIR" "$EXPECTED_PULL_DIR/gamma/sub" "$EXPECTED_PULL_DIR/delta" "$EXPECTED_PULL_DIR/empty"

printf 'hello from push test\n' > "$LOCAL_ROOT/src/lonely.txt"
printf 'alpha notes\nline two\n' > "$LOCAL_ROOT/src/alpha/notes.txt"
seq 1 512 > "$LOCAL_ROOT/src/alpha/sub/numbers.txt"
head -c 1048576 /dev/zero > "$LOCAL_ROOT/src/beta/blob.bin"

push_start_ns="$(date +%s%N)"
rsdb_cmd push \
    "$LOCAL_ROOT/src/alpha" \
    "$LOCAL_ROOT/src/beta" \
    "$LOCAL_ROOT/src/lonely.txt" \
    "$LOCAL_ROOT/src/emptydir" \
    "$REMOTE_PUSH_ROOT/"
push_ms="$(elapsed_ms "$push_start_ns")"

pull_back_start_ns="$(date +%s%N)"
rsdb_cmd pull \
    "$REMOTE_PUSH_ROOT/alpha" \
    "$REMOTE_PUSH_ROOT/beta" \
    "$REMOTE_PUSH_ROOT/lonely.txt" \
    "$REMOTE_PUSH_ROOT/emptydir" \
    "$ROUNDTRIP_DIR/"
pull_back_ms="$(elapsed_ms "$pull_back_start_ns")"

diff -qr "$LOCAL_ROOT/src" "$ROUNDTRIP_DIR"

rsdb_cmd shell mkdir -p "$REMOTE_PULL_ROOT/gamma/sub" "$REMOTE_PULL_ROOT/delta" "$REMOTE_PULL_ROOT/empty"
rsdb_cmd shell sh -lc \
    "printf 'pull info\n' > '$REMOTE_PULL_ROOT/gamma/info.txt'; \
     : > '$REMOTE_PULL_ROOT/gamma/sub/matrix.txt'; \
     i=1; \
     while [ \$i -le 1024 ]; do echo matrix-line >> '$REMOTE_PULL_ROOT/gamma/sub/matrix.txt'; i=\$((i+1)); done; \
     head -c 524288 /dev/zero > '$REMOTE_PULL_ROOT/delta/zeros.bin'"

printf 'pull info\n' > "$EXPECTED_PULL_DIR/gamma/info.txt"
: > "$EXPECTED_PULL_DIR/gamma/sub/matrix.txt"
for i in $(seq 1 1024); do
    echo matrix-line >> "$EXPECTED_PULL_DIR/gamma/sub/matrix.txt"
done
head -c 524288 /dev/zero > "$EXPECTED_PULL_DIR/delta/zeros.bin"

pull_start_ns="$(date +%s%N)"
rsdb_cmd pull \
    "$REMOTE_PULL_ROOT/gamma" \
    "$REMOTE_PULL_ROOT/delta" \
    "$REMOTE_PULL_ROOT/empty" \
    "$PULL_DIR/"
pull_ms="$(elapsed_ms "$pull_start_ns")"

diff -qr "$EXPECTED_PULL_DIR" "$PULL_DIR"

interactive_start_ns="$(date +%s%N)"
printf 'echo __RSDB_INTERACTIVE_OK__\necho __RSDB_PWD__:$PWD\nexit\n' \
    | script -qfec "$RSDB_BIN shell${TARGET:+ --target $TARGET}" "$INTERACTIVE_LOG" >/dev/null 2>&1
interactive_ms="$(elapsed_ms "$interactive_start_ns")"

grep -F '__RSDB_INTERACTIVE_OK__' "$INTERACTIVE_LOG" >/dev/null
grep -F '__RSDB_PWD__:' "$INTERACTIVE_LOG" >/dev/null

shell_cmd_start_ns="$(date +%s%N)"
rsdb_cmd shell uname -a >/dev/null
shell_cmd_ms="$(elapsed_ms "$shell_cmd_start_ns")"

repeat_shell_start_ns="$(date +%s%N)"
for _ in $(seq 1 20); do
    rsdb_cmd shell true >/dev/null
done
repeat_shell_ms="$(elapsed_ms "$repeat_shell_start_ns")"

repeat_ping_start_ns="$(date +%s%N)"
for _ in $(seq 1 20); do
    rsdb_cmd ping >/dev/null
done
repeat_ping_ms="$(elapsed_ms "$repeat_ping_start_ns")"

echo "PASS"
echo "target: ${TARGET:-current}"
echo "temp_dir: $LOCAL_ROOT"
echo "push_ms: $push_ms"
echo "push_roundtrip_pull_ms: $pull_back_ms"
echo "independent_pull_ms: $pull_ms"
echo "interactive_shell_ms: $interactive_ms"
echo "shell_cmd_ms: $shell_cmd_ms"
echo "repeated_shell_true_20_ms: $repeat_shell_ms"
echo "repeated_ping_20_ms: $repeat_ping_ms"
