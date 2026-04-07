#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

VERSION_FILE="$ROOT_DIR/VERSION"
if [[ ! -f "$VERSION_FILE" ]]; then
    echo "ERROR: VERSION file not found at $VERSION_FILE" >&2
    exit 1
fi

VERSION="$(cat "$VERSION_FILE" | tr -d '[:space:]')"
if [[ -z "$VERSION" ]]; then
    echo "ERROR: VERSION file is empty" >&2
    exit 1
fi

echo "Syncing version: $VERSION"

# Update root Cargo.toml [workspace.package] version
CARGO_ROOT="$ROOT_DIR/Cargo.toml"
echo "  Updating $CARGO_ROOT"
sed -i "s/^version = \"[^\"]*\"/version = \"$VERSION\"/" "$CARGO_ROOT"

# Update src-tauri/Cargo.toml
CARGO_TAURI="$ROOT_DIR/src-tauri/Cargo.toml"
echo "  Updating $CARGO_TAURI"
sed -i "s/^version = \"[^\"]*\"/version = \"$VERSION\"/" "$CARGO_TAURI"

# Update sp-ui/Cargo.toml
CARGO_UI="$ROOT_DIR/sp-ui/Cargo.toml"
echo "  Updating $CARGO_UI"
sed -i "s/^version = \"[^\"]*\"/version = \"$VERSION\"/" "$CARGO_UI"

# Update src-tauri/tauri.conf.json
TAURI_CONF="$ROOT_DIR/src-tauri/tauri.conf.json"
echo "  Updating $TAURI_CONF"
sed -i "s/\"version\": \"[^\"]*\"/\"version\": \"$VERSION\"/" "$TAURI_CONF"

echo "Done. All version fields set to $VERSION"
