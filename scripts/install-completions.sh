#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE_COMPLETION="$ROOT_DIR/scripts/rsdb-completion.bash"
CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
INSTALL_DIR="$CONFIG_HOME/rsdb"
INSTALL_PATH="$INSTALL_DIR/rsdb-completion.bash"
BASHRC_PATH="$HOME/.bashrc"
MARKER_START="# >>> rsdb bash completion >>>"
MARKER_END="# <<< rsdb bash completion <<<"

if [[ ! -f "$SOURCE_COMPLETION" ]]; then
    echo "error: completion source not found: $SOURCE_COMPLETION" >&2
    exit 1
fi

mkdir -p "$INSTALL_DIR"
cp "$SOURCE_COMPLETION" "$INSTALL_PATH"
chmod 0644 "$INSTALL_PATH"

if [[ ! -f "$BASHRC_PATH" ]]; then
    touch "$BASHRC_PATH"
fi

if ! grep -Fqx "$MARKER_START" "$BASHRC_PATH"; then
    cat >>"$BASHRC_PATH" <<'EOF'
# >>> rsdb bash completion >>>
if [ -f "${XDG_CONFIG_HOME:-$HOME/.config}/rsdb/rsdb-completion.bash" ]; then
    source "${XDG_CONFIG_HOME:-$HOME/.config}/rsdb/rsdb-completion.bash"
fi
# <<< rsdb bash completion <<<
EOF
fi

echo "Installed bash completion to $INSTALL_PATH"
echo "Ensured rsdb completion is sourced from $BASHRC_PATH"
echo "Open a new shell or run: source \"$BASHRC_PATH\""
