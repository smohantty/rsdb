#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_FILE="$ROOT_DIR/.cargo-tizen.toml"
SPEC_FILE="$ROOT_DIR/packaging/rpm/rsdbd.spec"
SERVICE_FILE="$ROOT_DIR/packaging/systemd/rsdbd.service"
ENV_FILE="$ROOT_DIR/packaging/rpm/rsdbd.env"

if [[ $# -gt 1 ]]; then
    echo "usage: $0 [aarch64|armv7l]" >&2
    exit 1
fi

REQUESTED_ARCH="${1:-}"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo is required" >&2
    exit 1
fi

if ! command -v rpmbuild >/dev/null 2>&1; then
    echo "error: rpmbuild is required" >&2
    exit 1
fi

if [[ ! -f "$CONFIG_FILE" ]]; then
    echo "error: cargo-tizen config not found: $CONFIG_FILE" >&2
    exit 1
fi

VERSION="$(
    awk '
        $0 == "[workspace.package]" { in_workspace = 1; next }
        /^\[/ { in_workspace = 0 }
        in_workspace && $1 == "version" {
            gsub(/"/, "", $3)
            print $3
            exit
        }
    ' "$ROOT_DIR/Cargo.toml"
)"

if [[ -z "$VERSION" ]]; then
    echo "error: failed to read workspace version from Cargo.toml" >&2
    exit 1
fi

toml_value() {
    local section="$1"
    local key="$2"
    awk -v wanted_section="$section" -v wanted_key="$key" '
        /^\[/ {
            section = $0
            gsub(/^\[/, "", section)
            gsub(/\]$/, "", section)
            next
        }
        section == wanted_section && $1 == wanted_key {
            value = $3
            gsub(/"/, "", value)
            print value
            exit
        }
    ' "$CONFIG_FILE"
}

ARCH="${REQUESTED_ARCH:-$(toml_value default arch)}"
ARCH="${ARCH:-aarch64}"
case "$ARCH" in
    aarch64)
        TARGET_TRIPLE="$(toml_value "arch.$ARCH" rust_target)"
        TARGET_TRIPLE="${TARGET_TRIPLE:-aarch64-unknown-linux-gnu}"
        RPM_ARCH="$(toml_value "arch.$ARCH" rpm_build_arch)"
        RPM_ARCH="${RPM_ARCH:-aarch64}"
        ;;
    armv7l)
        TARGET_TRIPLE="$(toml_value "arch.$ARCH" rust_target)"
        TARGET_TRIPLE="${TARGET_TRIPLE:-armv7-unknown-linux-gnueabi}"
        RPM_ARCH="$(toml_value "arch.$ARCH" rpm_build_arch)"
        RPM_ARCH="${RPM_ARCH:-armv7l}"
        ;;
    *)
        echo "error: invalid arch: $ARCH (expected aarch64 or armv7l)" >&2
        exit 1
        ;;
esac

PROFILE_NAME="$(toml_value default profile)"
PROFILE_NAME="${PROFILE_NAME:-mobile}"
PLATFORM_VERSION="$(toml_value default platform_version)"
PLATFORM_VERSION="${PLATFORM_VERSION:-10.0}"
PROVIDER="$(toml_value default provider)"
PROVIDER="${PROVIDER:-rootstrap}"
TOPDIR="$ROOT_DIR/target/packages/rpm/$RPM_ARCH"
HOST_ARCH="$(uname -m)"

echo "Building rsdbd for Tizen arch=$ARCH profile=$PROFILE_NAME platform_version=$PLATFORM_VERSION provider=$PROVIDER"
(
    cd "$ROOT_DIR"
    cargo tizen build --config "$CONFIG_FILE" -A "$ARCH" --release -- -p rsdbd
)

BINARY_PATH="$ROOT_DIR/target/tizen/$ARCH/cargo/$TARGET_TRIPLE/release/rsdbd"
if [[ ! -f "$BINARY_PATH" ]]; then
    echo "error: built rsdbd binary not found: $BINARY_PATH" >&2
    exit 1
fi

rm -rf "$TOPDIR"
mkdir -p "$TOPDIR"/BUILD "$TOPDIR"/BUILDROOT "$TOPDIR"/RPMS "$TOPDIR"/SOURCES "$TOPDIR"/SPECS "$TOPDIR"/SRPMS

cp "$BINARY_PATH" "$TOPDIR/SOURCES/rsdbd"
cp "$SERVICE_FILE" "$TOPDIR/SOURCES/rsdbd.service"
cp "$ENV_FILE" "$TOPDIR/SOURCES/rsdbd.env"
cp "$SPEC_FILE" "$TOPDIR/SPECS/rsdbd.spec"

RPMBUILD_ARGS=(
    -bb \
    --target "$RPM_ARCH"
    --define "_topdir $TOPDIR"
    --define "_build_id_links none"
    --define "rsdbd_version $VERSION"
    --define "rsdbd_arch $RPM_ARCH"
)

if [[ "$HOST_ARCH" != "$ARCH" ]]; then
    RPMBUILD_ARGS+=(
        --define "__brp_strip /bin/true"
        --define "__brp_strip_static_archive /bin/true"
        --define "__brp_strip_comment_note /bin/true"
    )
fi

RPMBUILD_ARGS+=("$TOPDIR/SPECS/rsdbd.spec")

if ! rpmbuild "${RPMBUILD_ARGS[@]}"; then
    if [[ "$ARCH" == "armv7l" ]]; then
        echo "error: rpmbuild rejected target arch $RPM_ARCH on this host. The armv7l daemon binary was built successfully, but this host RPM toolchain cannot emit an armv7l RPM." >&2
    fi
    exit 1
fi

RPM_PATH="$(find "$TOPDIR/RPMS" -type f -name "rsdbd-${VERSION}-1*.rpm" | head -n 1)"
if [[ -z "$RPM_PATH" ]]; then
    echo "error: rpmbuild completed but no RPM was found under $TOPDIR/RPMS" >&2
    exit 1
fi

echo "Built RPM:"
echo "$RPM_PATH"
