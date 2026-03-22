#!/bin/sh
# Update the npmDepsHash in flake.nix after npm install.
# Requires nix to be installed — silently skips if unavailable.

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FLAKE="$SCRIPT_DIR/../flake.nix"

if [ ! -f "$FLAKE" ]; then
  exit 0
fi

if ! command -v nix >/dev/null 2>&1; then
  exit 0
fi

LOCK="$SCRIPT_DIR/../package-lock.json"
if [ ! -f "$LOCK" ]; then
  exit 0
fi

hash=$(nix run nixpkgs#prefetch-npm-deps -- "$LOCK" 2>/dev/null) || exit 0

if [ -n "$hash" ]; then
  sed -i '' "s|npmDepsHash = \".*\"|npmDepsHash = \"$hash\"|" "$FLAKE"
  echo "Updated npmDepsHash in flake.nix"
fi
