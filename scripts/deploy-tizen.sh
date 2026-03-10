#!/usr/bin/env bash
set -euo pipefail

BINARY_PATH="./rsdbd"
SERVICE_FILE="./rsdbd.service"
INSTALL_BIN="/usr/bin/rsdbd"
INSTALL_SERVICE="/etc/systemd/system/rsdbd.service"
SERVICE_NAME="rsdbd.service"

if [[ $# -ne 0 ]]; then
    echo "error: this script takes no arguments; edit the variables at the top if you need different paths" >&2
    exit 1
fi

if [[ "$(id -u)" -ne 0 ]]; then
    echo "error: run this script as root on the target device" >&2
    exit 1
fi

for tool in install systemctl; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: required command not found: $tool" >&2
        exit 1
    fi
done

if [[ ! -f "$BINARY_PATH" ]]; then
    echo "error: rsdbd binary not found: $BINARY_PATH" >&2
    exit 1
fi

if [[ ! -f "$SERVICE_FILE" ]]; then
    echo "error: service file not found: $SERVICE_FILE" >&2
    exit 1
fi

print_logs() {
    if command -v journalctl >/dev/null 2>&1; then
        echo "Recent logs for $SERVICE_NAME:"
        journalctl --no-pager -u "$SERVICE_NAME" -n 20 || true
    fi
}

if systemctl is-active --quiet "$SERVICE_NAME"; then
    echo "Stopping existing service: $SERVICE_NAME"
    systemctl stop "$SERVICE_NAME"
fi

echo "Installing binary to $INSTALL_BIN"
install -Dm755 "$BINARY_PATH" "$INSTALL_BIN"
echo "Installing service file to $INSTALL_SERVICE"
install -Dm644 "$SERVICE_FILE" "$INSTALL_SERVICE"

echo "Reloading systemd"
systemctl daemon-reload
echo "Enabling service $SERVICE_NAME"
systemctl enable "$SERVICE_NAME"
echo "Restarting service $SERVICE_NAME"
systemctl restart "$SERVICE_NAME"

if systemctl is-active --quiet "$SERVICE_NAME"; then
    echo "Service restart succeeded: $SERVICE_NAME"
    systemctl --no-pager --full status "$SERVICE_NAME"
    print_logs
else
    echo "error: service is not active after restart: $SERVICE_NAME" >&2
    systemctl --no-pager --full status "$SERVICE_NAME" || true
    print_logs
    exit 1
fi

echo "Installed rsdbd to $INSTALL_BIN"
echo "Installed service to $INSTALL_SERVICE"
