#!/usr/bin/env sh
set -eu

if [ "${1:-}" = "" ]; then
  echo "Usage: $0 <mermaid-version>" >&2
  exit 1
fi

VERSION="$1"
ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
ASSETS_DIR="$ROOT_DIR/assets"

URL="https://unpkg.com/mermaid@${VERSION}/dist/mermaid.min.js"
OUT_FILE="$ASSETS_DIR/mermaid.min.js"
VERSION_FILE="$ASSETS_DIR/mermaid.version"

echo "Downloading mermaid ${VERSION}..."
curl -fsSL "$URL" -o "$OUT_FILE"
printf '%s\n' "$VERSION" > "$VERSION_FILE"
echo "Wrote $OUT_FILE"
echo "Wrote $VERSION_FILE"
