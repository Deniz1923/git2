#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
if [[ -z "$VERSION" ]]; then
  echo "could not determine workspace version from Cargo.toml" >&2
  exit 1
fi

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
PKG_NAME="git2-${VERSION}-${OS}-${ARCH}"
DIST_DIR="$ROOT_DIR/target/release-dist"
STAGE_DIR="$DIST_DIR/$PKG_NAME"
ARCHIVE="$DIST_DIR/${PKG_NAME}.tar.gz"
CHECKSUM_FILE="${ARCHIVE}.sha256"

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  cargo build --release --workspace
fi

BIN_PATH="$ROOT_DIR/target/release/git2"
if [[ ! -x "$BIN_PATH" ]]; then
  echo "release binary not found at $BIN_PATH" >&2
  exit 1
fi

rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR/docs"

install -m 0755 "$BIN_PATH" "$STAGE_DIR/git2"
install -m 0644 "$ROOT_DIR/README.md" "$STAGE_DIR/README.md"
install -m 0644 "$ROOT_DIR/LICENSE-MIT" "$STAGE_DIR/LICENSE-MIT"
install -m 0644 "$ROOT_DIR/SECURITY.md" "$STAGE_DIR/SECURITY.md"
install -m 0644 "$ROOT_DIR/docs/runbook.md" "$STAGE_DIR/docs/runbook.md"

tar -C "$DIST_DIR" -czf "$ARCHIVE" "$PKG_NAME"
(
  cd "$DIST_DIR"
  sha256sum "$(basename "$ARCHIVE")" > "$(basename "$CHECKSUM_FILE")"
  sha256sum -c "$(basename "$CHECKSUM_FILE")"
)

rm -rf "$STAGE_DIR"
echo "Packaged: $ARCHIVE"
echo "Checksum: $CHECKSUM_FILE"
