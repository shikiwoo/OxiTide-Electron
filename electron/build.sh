#!/bin/bash
# Build the Electron flavour of OxiTide. Run inside the `oxitide-dev` distrobox:
#   distrobox enter oxitide-dev -- ./electron/build.sh
set -euo pipefail
cd "$(dirname "$0")/.."
npm --prefix ui ci
npm --prefix ui run build            # Tauri's embedded dist (phone remote + blank page)
npm --prefix ui run build:electron   # the UI Electron renders
cargo build --release -p oxitide-app
npm --prefix electron ci
node electron/node_modules/electron/install.js   # fetches the Electron binary (checksum-verified)
echo "Built. Run with: ./electron/run.sh"
