#!/usr/bin/env bash
# Build the `vex` and `vex-serve` binaries for cloud deployment.
#
# Outputs to ./dist/:
#   - vex         (CLI, default features)
#   - vex-serve   (network daemon, with s3-backend + postgres-backend)
#   - SHA256SUMS  (sha256 of both binaries)
#   - VERSION     (semver from workspace Cargo.toml)
#
# This script is the single source of truth for the binaries the Architur
# cloud consumes. Architur references vex by these artifacts only — never
# by source path — so the vex repository is free to live anywhere.
#
# Usage:
#   tools/build-vex.sh                  # release build
#   tools/build-vex.sh --debug          # debug build (faster, larger)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PROFILE="release"
PROFILE_FLAG="--release"
if [[ "${1:-}" == "--debug" ]]; then
  PROFILE="debug"
  PROFILE_FLAG=""
fi

DIST="$ROOT/dist"
mkdir -p "$DIST"

VERSION="$(awk -F\" '/^version[[:space:]]*=/ { print $2; exit }' Cargo.toml)"
echo "Building vex v$VERSION ($PROFILE)..."

# Local CLI: no cloud features, smallest binary.
cargo build $PROFILE_FLAG -p vex-cli
# Cloud daemon: requires the S3 + Postgres backends.
# `vex-serve` will land in a follow-up phase; until then we ship a stub so
# the deployment surface is in place.
if cargo metadata --format-version 1 --no-deps 2>/dev/null \
     | grep -q '"name":"vex-serve"'; then
  cargo build $PROFILE_FLAG -p vex-serve \
    --features 'vex-storage/s3-backend vex-storage/postgres-backend'
fi

cp "target/$PROFILE/vex" "$DIST/vex"
if [[ -f "target/$PROFILE/vex-serve" ]]; then
  cp "target/$PROFILE/vex-serve" "$DIST/vex-serve"
fi

(
  cd "$DIST"
  : > SHA256SUMS
  for f in vex vex-serve; do
    [[ -f "$f" ]] && sha256sum "$f" >> SHA256SUMS
  done
  echo "$VERSION" > VERSION
  echo
  echo "=== dist/ ==="
  ls -lh
  echo
  echo "=== SHA256SUMS ==="
  cat SHA256SUMS
)
