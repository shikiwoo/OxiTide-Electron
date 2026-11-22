#!/bin/bash
# Build the OxiTide Electron AppImage into electron/out/dist/.
#   ./electron/package.sh [version]
# The version defaults to the backend's (workspace Cargo.toml) plus "-dev".
# Needs everything build.sh needs; CI runs it on ubuntu-22.04 (glibc floor).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo_version=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
version="${1:-${cargo_version}-dev}"
./electron/build.sh
npm --prefix electron pkg set version="$version"
trap 'npm --prefix electron pkg set version=0.0.0-dev' EXIT
npm --prefix electron run appimage
ls -l electron/out/dist/*.AppImage
