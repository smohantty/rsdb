#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
REMOTE="origin"

usage() {
    cat <<'EOF'
usage: ./scripts/release-rsdb.sh [--remote <name>]

Builds fresh rsdbd RPMs for aarch64 and armv7l, refreshes the version tag,
and updates the matching GitHub release with replacement assets.
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

ensure_clean_worktree() {
    if [[ -n "$(git status --porcelain)" ]]; then
        echo "error: worktree must be clean before releasing" >&2
        exit 1
    fi
}

write_sha256_sidecar() {
    local asset_path="$1"
    local digest

    digest="$(sha256sum "$asset_path" | awk '{print $1}')"
    printf '%s  %s\n' "$digest" "$(basename "$asset_path")" > "$asset_path.sha256"
}

release_notes() {
    local output_path="$1"
    local head_commit="$2"
    local head_short="$3"
    local today_utc="$4"
    local previous_commit="$5"
    local previous_short="$6"
    local changes="$7"
    local aarch64_rpm="$8"
    local armv7l_rpm="$9"

    {
        printf 'RSDB %s\n\n' "$TAG"

        if [[ -z "$previous_commit" ]]; then
            printf 'Published on %s from `main` commit `%s`.\n\n' "$today_utc" "$head_short"
        elif [[ "$previous_commit" == "$head_commit" ]]; then
            printf 'Refreshed on %s from `main` commit `%s`; the release tag already pointed at the latest code.\n\n' "$today_utc" "$head_short"
        else
            printf 'Refreshed on %s from `main` commit `%s` so the existing `%s` release matches the latest code.\n\n' "$today_utc" "$head_short" "$TAG"
            printf 'Changes since the previous `%s` tag target `%s`:\n' "$TAG" "$previous_short"
            if [[ -n "$changes" ]]; then
                printf '%s\n\n' "$changes"
            else
                printf -- '- No code changes; assets rebuilt.\n\n'
            fi
        fi

        printf 'Assets in this release:\n'
        printf -- '- `%s` (64-bit ARM)\n' "$(basename "$aarch64_rpm")"
        printf -- '- `%s`\n' "$(basename "$aarch64_rpm.sha256")"
        printf -- '- `%s` (32-bit ARM)\n' "$(basename "$armv7l_rpm")"
        printf -- '- `%s`\n\n' "$(basename "$armv7l_rpm.sha256")"

        printf 'Install on a Tizen target:\n'
        printf '```bash\n'
        printf '# aarch64\n'
        printf 'rpm -Uvh %s\n\n' "$(basename "$aarch64_rpm")"
        printf '# armv7l\n'
        printf 'rpm -Uvh %s\n' "$(basename "$armv7l_rpm")"
        printf '```\n\n'

        printf 'This RPM installs:\n'
        printf -- '- `/usr/bin/rsdbd`\n'
        printf -- '- `/usr/lib/systemd/system/rsdbd.service`\n'
        printf -- '- `/etc/rsdbd.env`\n\n'

        printf 'The package also handles service stop/reload/enable/restart during install and upgrade.\n'
    } > "$output_path"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --remote)
            if [[ $# -lt 2 ]]; then
                echo "error: --remote requires a value" >&2
                exit 1
            fi
            REMOTE="$2"
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

require_command git
require_command gh
require_command cargo
require_command sha256sum
require_command awk

if ! cargo tizen --version >/dev/null 2>&1; then
    echo "error: cargo-tizen is required" >&2
    exit 1
fi

cd "$ROOT_DIR"

CURRENT_BRANCH="$(git symbolic-ref --quiet --short HEAD || true)"
if [[ "$CURRENT_BRANCH" != "main" ]]; then
    echo "error: releases must be created from branch main (current: ${CURRENT_BRANCH:-detached})" >&2
    exit 1
fi

ensure_clean_worktree

if ! git remote get-url "$REMOTE" >/dev/null 2>&1; then
    echo "error: git remote not found: $REMOTE" >&2
    exit 1
fi

gh auth status >/dev/null

git fetch "$REMOTE" main --tags >/dev/null

if ! git show-ref --verify --quiet "refs/remotes/$REMOTE/main"; then
    echo "error: remote branch not found: $REMOTE/main" >&2
    exit 1
fi

if ! git merge-base --is-ancestor "$REMOTE/main" HEAD; then
    echo "error: local main must contain the latest $REMOTE/main before releasing" >&2
    exit 1
fi

HEAD_COMMIT="$(git rev-parse HEAD)"
HEAD_SHORT="$(git rev-parse --short HEAD)"
REMOTE_MAIN_COMMIT="$(git rev-parse "$REMOTE/main")"
VERSION="$(workspace_version)"

if [[ -z "$VERSION" ]]; then
    echo "error: failed to read workspace version from Cargo.toml" >&2
    exit 1
fi

TAG="v$VERSION"
RELEASE_NAME="RSDB $TAG"
TODAY_UTC="$(date -u '+%B %d, %Y')"
TODAY_UTC="${TODAY_UTC/ 0/ }"

PREVIOUS_COMMIT=""
PREVIOUS_SHORT=""
CHANGES=""

if git rev-parse "$TAG^{}" >/dev/null 2>&1; then
    PREVIOUS_COMMIT="$(git rev-parse "$TAG^{}")"
    PREVIOUS_SHORT="$(git rev-parse --short "$TAG^{}")"
    if [[ "$PREVIOUS_COMMIT" != "$HEAD_COMMIT" ]]; then
        CHANGES="$(git log --pretty='- %s' "$TAG..HEAD")"
    fi
fi

if [[ "$HEAD_COMMIT" != "$REMOTE_MAIN_COMMIT" ]]; then
    echo "Pushing main to $REMOTE"
    git push "$REMOTE" main
fi

echo "Building release assets"
(cd "$ROOT_DIR" && cargo tizen rpm -p rsdbd -A aarch64 --cargo-release)
(cd "$ROOT_DIR" && cargo tizen rpm -p rsdbd -A armv7l --cargo-release)

AARCH64_RPM="$ROOT_DIR/target/tizen/aarch64/release/rpmbuild/RPMS/aarch64/rsdbd-$VERSION-1.aarch64.rpm"
ARMV7L_RPM="$ROOT_DIR/target/tizen/armv7l/release/rpmbuild/RPMS/armv7l/rsdbd-$VERSION-1.armv7l.rpm"

for asset in "$AARCH64_RPM" "$ARMV7L_RPM"; do
    if [[ ! -f "$asset" ]]; then
        echo "error: expected asset not found: $asset" >&2
        exit 1
    fi
    write_sha256_sidecar "$asset"
done

NOTES_FILE="$(mktemp)"
trap 'rm -f "$NOTES_FILE"' EXIT

release_notes \
    "$NOTES_FILE" \
    "$HEAD_COMMIT" \
    "$HEAD_SHORT" \
    "$TODAY_UTC" \
    "$PREVIOUS_COMMIT" \
    "$PREVIOUS_SHORT" \
    "$CHANGES" \
    "$AARCH64_RPM" \
    "$ARMV7L_RPM"

if git rev-parse "$TAG" >/dev/null 2>&1; then
    git tag -fa "$TAG" -m "$RELEASE_NAME" HEAD
else
    git tag -a "$TAG" -m "$RELEASE_NAME" HEAD
fi

git push "$REMOTE" "refs/tags/$TAG" --force

ASSETS=(
    "$AARCH64_RPM"
    "$AARCH64_RPM.sha256"
    "$ARMV7L_RPM"
    "$ARMV7L_RPM.sha256"
)

if gh release view "$TAG" >/dev/null 2>&1; then
    gh release edit "$TAG" --title "$RELEASE_NAME" --notes-file "$NOTES_FILE" >/dev/null
    gh release upload "$TAG" "${ASSETS[@]}" --clobber >/dev/null
else
    gh release create "$TAG" "${ASSETS[@]}" --title "$RELEASE_NAME" --notes-file "$NOTES_FILE" >/dev/null
fi

REMOTE_TAG_COMMIT="$(git ls-remote "$REMOTE" "refs/tags/$TAG^{}" | awk '{print $1}')"
if [[ "$REMOTE_TAG_COMMIT" != "$HEAD_COMMIT" ]]; then
    echo "error: remote tag $TAG does not resolve to $HEAD_COMMIT" >&2
    exit 1
fi

RELEASE_ASSETS="$(gh release view "$TAG" --json assets --jq '.assets[].name')"
for asset in "${ASSETS[@]}"; do
    if ! grep -Fx "$(basename "$asset")" <<<"$RELEASE_ASSETS" >/dev/null; then
        echo "error: release asset missing after upload: $(basename "$asset")" >&2
        exit 1
    fi
done

RELEASE_URL="$(gh release view "$TAG" --json url --jq '.url')"

echo "Release ready"
echo "tag: $TAG"
echo "commit: $HEAD_COMMIT"
echo "url: $RELEASE_URL"
for asset in "${ASSETS[@]}"; do
    echo "asset: $(basename "$asset")"
done
