#!/usr/bin/env bash
set -euo pipefail

TARGET=""
KEEP_TEMP=0
RSDB_BIN="${RSDB_BIN:-rsdb}"

usage() {
    cat <<'EOF'
usage: ./scripts/test/rsdb-agent-regression.sh [--target <ip[:port]>] [--keep-temp]

Runs a live regression smoke test against an rsdb-connected device using only
the machine-facing `rsdb agent` surface:
- schema
- discover
- exec
- exec --stream
- exec --check failure reporting
- fs stat/list/read/write/mkdir/rm/move with precondition failure validation
- transfer push --verify --atomic --if-changed
- transfer pull --verify
EOF
}

require_command() {
    local command_name="$1"

    if ! command -v "$command_name" >/dev/null 2>&1; then
        echo "error: $command_name is required" >&2
        exit 1
    fi
}

resolve_default_target() {
    python3 - <<'PY'
import json
import os
import pathlib
import sys

config_home = os.environ.get("XDG_CONFIG_HOME")
if config_home:
    config_dir = pathlib.Path(config_home) / "rsdb"
else:
    home = os.environ.get("HOME")
    if not home:
        print("error: HOME is not set", file=sys.stderr)
        sys.exit(1)
    config_dir = pathlib.Path(home) / ".config" / "rsdb"

registry_path = config_dir / "targets.json"
if not registry_path.exists():
    print("error: no saved rsdb targets; pass --target <ip[:port]>", file=sys.stderr)
    sys.exit(1)

registry = json.loads(registry_path.read_text())
targets = registry.get("targets") or []
current_target = registry.get("current_target")
if current_target:
    for target in targets:
        if target.get("addr") == current_target or target.get("name") == current_target:
            print(target["addr"])
            sys.exit(0)
if targets:
    print(targets[-1]["addr"])
    sys.exit(0)

print("error: no saved rsdb targets; pass --target <ip[:port]>", file=sys.stderr)
sys.exit(1)
PY
}

split_target() {
    python3 - "$1" <<'PY'
import sys

value = sys.argv[1]
host, sep, port = value.rpartition(":")
if not sep or not port.isdigit():
    host = value
    port = "27101"
print(f"{host} {port}")
PY
}

assert_json_success() {
    python3 - "$1" "$2" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
expected_command = sys.argv[2]
obj = json.loads(path.read_text())
assert obj["schema_version"] == "agent.v1", obj
assert obj["command"] == expected_command, obj
assert obj["ok"] is True, obj
assert obj["error"] is None, obj
PY
}

assert_json_failure_code() {
    python3 - "$1" "$2" "$3" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
expected_command = sys.argv[2]
expected_code = sys.argv[3]
obj = json.loads(path.read_text())
assert obj["schema_version"] == "agent.v1", obj
assert obj["command"] == expected_command, obj
assert obj["ok"] is False, obj
assert obj["error"]["code"] == expected_code, obj
PY
}

assert_exec_stream() {
    python3 - "$1" <<'PY'
import json
import pathlib
import sys

events = [json.loads(line) for line in pathlib.Path(sys.argv[1]).read_text().splitlines() if line.strip()]
assert any(event["event"] == "stdout" and event["data"]["chunk"] == "out" for event in events), events
assert any(event["event"] == "stderr" and event["data"]["chunk"] == "err" for event in events), events
assert any(event["event"] == "completed" and event["data"]["status"] == 0 for event in events), events
PY
}

agent_cmd() {
    "$RSDB_BIN" agent --target "$TARGET" "$@"
}

cleanup() {
    if [[ -n "$TARGET" ]]; then
        agent_cmd fs rm "$REMOTE_ROOT" --recursive --force --if-exists >/dev/null 2>&1 || true
    fi
    if [[ "$KEEP_TEMP" -eq 0 ]]; then
        rm -rf "$LOCAL_ROOT"
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
require_command mktemp
require_command python3

if [[ -z "$TARGET" ]]; then
    TARGET="$(resolve_default_target)"
fi

read -r TARGET_HOST TARGET_PORT <<<"$(split_target "$TARGET")"

LOCAL_ROOT="$(mktemp -d /tmp/rsdb-agent-regression-XXXXXX)"
ROUNDTRIP_DIR="$LOCAL_ROOT/roundtrip"
REMOTE_ROOT="/tmp/rsdb-agent-regression-$(date +%s)"
SCHEMA_OUTPUT="$LOCAL_ROOT/schema.json"
DISCOVER_OUTPUT="$LOCAL_ROOT/discover.json"
EXEC_OUTPUT="$LOCAL_ROOT/exec.json"
EXEC_CWD_OUTPUT="$LOCAL_ROOT/exec-cwd.json"
EXEC_CHECK_OUTPUT="$LOCAL_ROOT/exec-check.json"
EXEC_TIMEOUT_OUTPUT="$LOCAL_ROOT/exec-timeout.json"
EXEC_STREAM_OUTPUT="$LOCAL_ROOT/exec-stream.jsonl"
FS_MKDIR_OUTPUT="$LOCAL_ROOT/fs-mkdir.json"
FS_MKDIR_REPEAT_OUTPUT="$LOCAL_ROOT/fs-mkdir-repeat.json"
FS_WRITE_OUTPUT="$LOCAL_ROOT/fs-write.json"
FS_STAT_OUTPUT="$LOCAL_ROOT/fs-stat.json"
FS_LIST_OUTPUT="$LOCAL_ROOT/fs-list.json"
FS_READ_OUTPUT="$LOCAL_ROOT/fs-read.json"
FS_READ_TRUNCATED_OUTPUT="$LOCAL_ROOT/fs-read-truncated.json"
FS_BINARY_WRITE_OUTPUT="$LOCAL_ROOT/fs-binary-write.json"
FS_BINARY_READ_OUTPUT="$LOCAL_ROOT/fs-binary-read.json"
FS_UPDATE_OUTPUT="$LOCAL_ROOT/fs-update.json"
FS_GUARD_OUTPUT="$LOCAL_ROOT/fs-guard.json"
FS_IF_MISSING_OUTPUT="$LOCAL_ROOT/fs-if-missing.json"
FS_MOVE_OUTPUT="$LOCAL_ROOT/fs-move.json"
FS_MOVE_GUARD_OUTPUT="$LOCAL_ROOT/fs-move-guard.json"
FS_MOVE_OVERWRITE_OUTPUT="$LOCAL_ROOT/fs-move-overwrite.json"
FS_RM_OUTPUT="$LOCAL_ROOT/fs-rm.json"
FS_RM_MISSING_OUTPUT="$LOCAL_ROOT/fs-rm-missing.json"
FS_RM_TREE_OUTPUT="$LOCAL_ROOT/fs-rm-tree.json"
PUSH_ONE_OUTPUT="$LOCAL_ROOT/push-one.json"
PUSH_TWO_OUTPUT="$LOCAL_ROOT/push-two.json"
PULL_OUTPUT="$LOCAL_ROOT/pull.json"

trap cleanup EXIT

mkdir -p "$LOCAL_ROOT/bundle/sub" "$ROUNDTRIP_DIR"
printf 'agent-fs-original\n' > "$LOCAL_ROOT/fs-source.txt"
printf 'agent-fs-updated\n' > "$LOCAL_ROOT/fs-updated.txt"
printf 'listing nested info\n' > "$LOCAL_ROOT/listing-info.txt"
printf 'listing hidden info\n' > "$LOCAL_ROOT/listing-hidden.txt"
python3 - "$LOCAL_ROOT/fs-binary.bin" <<'PY'
import pathlib
import sys

pathlib.Path(sys.argv[1]).write_bytes(b"\x00\x01agent-binary\xff\n")
PY
printf 'bundle info\n' > "$LOCAL_ROOT/bundle/sub/info.txt"
printf 'hidden data\n' > "$LOCAL_ROOT/bundle/.hidden"
printf 'single transfer file\n' > "$LOCAL_ROOT/lone.txt"

"$RSDB_BIN" agent schema > "$SCHEMA_OUTPUT"
assert_json_success "$SCHEMA_OUTPUT" "schema"
python3 - "$SCHEMA_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
names = {operation["name"] for operation in obj["data"]["operations"]}
assert {
    "schema",
    "discover",
    "exec",
    "fs.stat",
    "fs.list",
    "fs.read",
    "fs.write",
    "fs.mkdir",
    "fs.rm",
    "fs.move",
    "transfer.push",
    "transfer.pull",
} <= names, obj
PY

"$RSDB_BIN" agent discover \
    --probe-addr "$TARGET_HOST" \
    --port "$TARGET_PORT" \
    --timeout-ms 500 > "$DISCOVER_OUTPUT"
assert_json_success "$DISCOVER_OUTPUT" "discover"
python3 - "$DISCOVER_OUTPUT" "$TARGET" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
targets = obj["data"]["targets"]
assert targets, obj
assert any(target["compatible"] for target in targets), obj
assert any(target["target"] == sys.argv[2] for target in targets), obj
assert any("fs.list" in target["supported_operations"] for target in targets if target["target"] == sys.argv[2]), obj
PY

agent_cmd exec -- printf 'agent-exec-ok\n' > "$EXEC_OUTPUT"
assert_json_success "$EXEC_OUTPUT" "exec"
python3 - "$EXEC_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["status"] == 0, obj
assert obj["data"]["stdout"] == "agent-exec-ok\n", obj
PY

agent_cmd fs mkdir "$REMOTE_ROOT/cwd" --parents >/dev/null
agent_cmd exec --cwd "$REMOTE_ROOT/cwd" -- pwd > "$EXEC_CWD_OUTPUT"
assert_json_success "$EXEC_CWD_OUTPUT" "exec"
python3 - "$EXEC_CWD_OUTPUT" "$REMOTE_ROOT/cwd" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["cwd"] == sys.argv[2], obj
assert obj["data"]["stdout"].strip() == sys.argv[2], obj
PY

if agent_cmd exec --check -- sh -c 'printf check-failed 1>&2; exit 7' > "$EXEC_CHECK_OUTPUT"; then
    echo "error: exec --check unexpectedly succeeded for a nonzero exit status" >&2
    exit 1
fi
assert_json_failure_code "$EXEC_CHECK_OUTPUT" "exec" "exec.nonzero"
python3 - "$EXEC_CHECK_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["error"]["details"]["status"] == 7, obj
assert "check-failed" in obj["error"]["details"]["stderr"], obj
PY

if agent_cmd exec --timeout-secs 1 -- sh -c 'sleep 2' > "$EXEC_TIMEOUT_OUTPUT"; then
    echo "error: exec --timeout-secs unexpectedly succeeded for a timed out command" >&2
    exit 1
fi
assert_json_failure_code "$EXEC_TIMEOUT_OUTPUT" "exec" "exec.timeout"

agent_cmd exec --stream -- sh -c 'printf out; printf err 1>&2' > "$EXEC_STREAM_OUTPUT"
assert_exec_stream "$EXEC_STREAM_OUTPUT"

agent_cmd fs mkdir "$REMOTE_ROOT/listing/sub" --parents > "$FS_MKDIR_OUTPUT"
assert_json_success "$FS_MKDIR_OUTPUT" "fs.mkdir"
python3 - "$FS_MKDIR_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["created"] is True, obj
PY

agent_cmd fs mkdir "$REMOTE_ROOT/listing/sub" --parents > "$FS_MKDIR_REPEAT_OUTPUT"
assert_json_success "$FS_MKDIR_REPEAT_OUTPUT" "fs.mkdir"
python3 - "$FS_MKDIR_REPEAT_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["created"] is False, obj
PY

agent_cmd fs write "$REMOTE_ROOT/listing/sub/info.txt" \
    --input-file "$LOCAL_ROOT/listing-info.txt" >/dev/null
agent_cmd fs write "$REMOTE_ROOT/listing/.hidden" \
    --input-file "$LOCAL_ROOT/listing-hidden.txt" >/dev/null

agent_cmd fs write "$REMOTE_ROOT/fs/data.txt" \
    --input-file "$LOCAL_ROOT/fs-source.txt" \
    --create-parent \
    --atomic > "$FS_WRITE_OUTPUT"
assert_json_success "$FS_WRITE_OUTPUT" "fs.write"

agent_cmd fs stat "$REMOTE_ROOT/fs/data.txt" --hash sha256 > "$FS_STAT_OUTPUT"
assert_json_success "$FS_STAT_OUTPUT" "fs.stat"
OLD_SHA="$(python3 - "$FS_STAT_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["exists"] is True, obj
assert obj["data"]["sha256"], obj
print(obj["data"]["sha256"])
PY
)"

agent_cmd fs list "$REMOTE_ROOT/listing" --recursive --include-hidden --hash sha256 > "$FS_LIST_OUTPUT"
assert_json_success "$FS_LIST_OUTPUT" "fs.list"
python3 - "$FS_LIST_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
entries = {entry["path"]: entry for entry in obj["data"]["entries"]}
assert any(path.endswith("/listing/.hidden") for path in entries), obj
assert any(path.endswith("/listing/sub") and entries[path]["kind"] == "directory" for path in entries), obj
assert any(path.endswith("/listing/sub/info.txt") and entries[path]["sha256"] for path in entries), obj
PY

agent_cmd fs read "$REMOTE_ROOT/fs/data.txt" > "$FS_READ_OUTPUT"
assert_json_success "$FS_READ_OUTPUT" "fs.read"
python3 - "$FS_READ_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["content"] == "agent-fs-original\n", obj
assert obj["data"]["truncated"] is False, obj
PY

agent_cmd fs read "$REMOTE_ROOT/fs/data.txt" --max-bytes 5 > "$FS_READ_TRUNCATED_OUTPUT"
assert_json_success "$FS_READ_TRUNCATED_OUTPUT" "fs.read"
python3 - "$FS_READ_TRUNCATED_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["bytes"] == 5, obj
assert obj["data"]["content"] == "agent", obj
assert obj["data"]["truncated"] is True, obj
PY

agent_cmd fs write "$REMOTE_ROOT/fs/binary.bin" \
    --input-file "$LOCAL_ROOT/fs-binary.bin" \
    --encoding base64 > "$FS_BINARY_WRITE_OUTPUT"
assert_json_success "$FS_BINARY_WRITE_OUTPUT" "fs.write"

agent_cmd fs read "$REMOTE_ROOT/fs/binary.bin" --encoding base64 > "$FS_BINARY_READ_OUTPUT"
assert_json_success "$FS_BINARY_READ_OUTPUT" "fs.read"
python3 - "$FS_BINARY_READ_OUTPUT" "$LOCAL_ROOT/fs-binary.bin" <<'PY'
import base64
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
expected = pathlib.Path(sys.argv[2]).read_bytes()
assert base64.b64decode(obj["data"]["content"]) == expected, obj
assert obj["data"]["truncated"] is False, obj
PY

agent_cmd fs write "$REMOTE_ROOT/fs/data.txt" \
    --input-file "$LOCAL_ROOT/fs-updated.txt" > "$FS_UPDATE_OUTPUT"
assert_json_success "$FS_UPDATE_OUTPUT" "fs.write"

if agent_cmd fs write "$REMOTE_ROOT/fs/data.txt" \
    --input-file "$LOCAL_ROOT/fs-source.txt" \
    --if-sha256 "$OLD_SHA" > "$FS_GUARD_OUTPUT"; then
    echo "error: fs.write with a stale sha256 precondition unexpectedly succeeded" >&2
    exit 1
fi
assert_json_failure_code "$FS_GUARD_OUTPUT" "fs.write" "fs.precondition_failed"

if agent_cmd fs write "$REMOTE_ROOT/fs/data.txt" \
    --input-file "$LOCAL_ROOT/fs-source.txt" \
    --if-missing > "$FS_IF_MISSING_OUTPUT"; then
    echo "error: fs.write with --if-missing unexpectedly succeeded for an existing file" >&2
    exit 1
fi
assert_json_failure_code "$FS_IF_MISSING_OUTPUT" "fs.write" "fs.precondition_failed"

agent_cmd fs move "$REMOTE_ROOT/fs/data.txt" "$REMOTE_ROOT/fs/moved.txt" > "$FS_MOVE_OUTPUT"
assert_json_success "$FS_MOVE_OUTPUT" "fs.move"
agent_cmd fs write "$REMOTE_ROOT/fs/conflict.txt" \
    --input-file "$LOCAL_ROOT/fs-source.txt" >/dev/null

if agent_cmd fs move "$REMOTE_ROOT/fs/moved.txt" "$REMOTE_ROOT/fs/conflict.txt" > "$FS_MOVE_GUARD_OUTPUT"; then
    echo "error: fs.move without --overwrite unexpectedly replaced the destination" >&2
    exit 1
fi
assert_json_failure_code "$FS_MOVE_GUARD_OUTPUT" "fs.move" "fs.precondition_failed"

agent_cmd fs move "$REMOTE_ROOT/fs/moved.txt" "$REMOTE_ROOT/fs/conflict.txt" --overwrite > "$FS_MOVE_OVERWRITE_OUTPUT"
assert_json_success "$FS_MOVE_OVERWRITE_OUTPUT" "fs.move"
python3 - "$FS_MOVE_OVERWRITE_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["overwritten"] is True, obj
PY

agent_cmd fs rm "$REMOTE_ROOT/fs/binary.bin" > "$FS_RM_OUTPUT"
assert_json_success "$FS_RM_OUTPUT" "fs.rm"
python3 - "$FS_RM_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["removed"] is True, obj
PY

agent_cmd fs rm "$REMOTE_ROOT/fs/binary.bin" --force --if-exists > "$FS_RM_MISSING_OUTPUT"
assert_json_success "$FS_RM_MISSING_OUTPUT" "fs.rm"
python3 - "$FS_RM_MISSING_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["removed"] is False, obj
PY

agent_cmd fs rm "$REMOTE_ROOT/listing" --recursive > "$FS_RM_TREE_OUTPUT"
assert_json_success "$FS_RM_TREE_OUTPUT" "fs.rm"

agent_cmd transfer push \
    --verify sha256 \
    --atomic \
    --if-changed \
    "$LOCAL_ROOT/bundle" \
    "$LOCAL_ROOT/lone.txt" \
    "$REMOTE_ROOT/transfer/" > "$PUSH_ONE_OUTPUT"
assert_json_success "$PUSH_ONE_OUTPUT" "transfer.push"
python3 - "$PUSH_ONE_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["verified"] is True, obj
assert obj["data"]["atomic"] is True, obj
assert obj["data"]["skipped"] is False, obj
PY

agent_cmd transfer push \
    --verify sha256 \
    --atomic \
    --if-changed \
    "$LOCAL_ROOT/bundle" \
    "$LOCAL_ROOT/lone.txt" \
    "$REMOTE_ROOT/transfer/" > "$PUSH_TWO_OUTPUT"
assert_json_success "$PUSH_TWO_OUTPUT" "transfer.push"
python3 - "$PUSH_TWO_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["verified"] is True, obj
assert obj["data"]["skipped"] is True, obj
PY

agent_cmd transfer pull \
    --verify sha256 \
    "$REMOTE_ROOT/transfer/bundle" \
    "$REMOTE_ROOT/transfer/lone.txt" \
    "$ROUNDTRIP_DIR/" > "$PULL_OUTPUT"
assert_json_success "$PULL_OUTPUT" "transfer.pull"
python3 - "$PULL_OUTPUT" <<'PY'
import json
import pathlib
import sys

obj = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert obj["data"]["verified"] is True, obj
PY

diff -qr "$LOCAL_ROOT/bundle" "$ROUNDTRIP_DIR/bundle"
diff -q "$LOCAL_ROOT/lone.txt" "$ROUNDTRIP_DIR/lone.txt"

echo "PASS"
echo "target: $TARGET"
echo "temp_dir: $LOCAL_ROOT"
