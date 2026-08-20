#!/usr/bin/env bash
set -euo pipefail

tag=${1:?usage: check-release-version.sh vX.Y.Z}
tag_version=${tag#v}
cargo_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' src-tauri/Cargo.toml | head -1)
tauri_version=$(sed -n 's/^[[:space:]]*"version": "\([^"]*\)".*/\1/p' src-tauri/tauri.conf.json | head -1)

if [[ "$tag_version" != "$cargo_version" || "$tag_version" != "$tauri_version" ]]; then
  echo "version mismatch: tag=$tag_version Cargo.toml=$cargo_version tauri.conf.json=$tauri_version" >&2
  exit 1
fi
